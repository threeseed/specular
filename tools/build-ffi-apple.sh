#!/usr/bin/env bash
# Builds `specular-ffi` for Apple platforms: `SpecularFFI.xcframework` (one dynamic framework
# per platform, so the library carries its own Rust standard library beside a host app's own
# Rust core) and the Swift bindings, `Sources/Specular/Specular.swift`.
#
#   tools/build-ffi-apple.sh --output DIR [--debug] [--macos-deployment-target 27.0]
#                            [--ios-deployment-target 27.0]
#
# Writes DIR/SpecularFFI.xcframework and DIR/Sources/Specular/Specular.swift. Needs Xcode and
# the targets aarch64-apple-darwin, aarch64-apple-ios and aarch64-apple-ios-sim (installed
# here when missing).
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
out=""
profile=release
macos_target=27.0
ios_target=27.0
while [ $# -gt 0 ]; do
    case "$1" in
        --output) out="$2"; shift 2 ;;
        --debug) profile=debug; shift ;;
        --release) profile=release; shift ;;
        --macos-deployment-target) macos_target="$2"; shift 2 ;;
        --ios-deployment-target) ios_target="$2"; shift 2 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done
[ -n "$out" ] || { echo "pass --output DIR" >&2; exit 2; }
mkdir -p "$out"
out="$(cd "$out" && pwd)"

cd "$root"
targets=(aarch64-apple-darwin aarch64-apple-ios aarch64-apple-ios-sim)
rustup target add "${targets[@]}" >/dev/null
flags=()
[ "$profile" = release ] && flags+=(--release)

for t in "${targets[@]}"; do
    case "$t" in
        *-darwin) export MACOSX_DEPLOYMENT_TARGET="$macos_target" ;;
        *) export IPHONEOS_DEPLOYMENT_TARGET="$ios_target" ;;
    esac
    cargo build -p specular-ffi --lib --target "$t" "${flags[@]}"
done

work="$root/target/ffi-apple"
rm -rf "$work"
mkdir -p "$work/bindings"

# The Swift bindings, the C header and the module map, from the macOS library's metadata.
cargo run -p specular-ffi --features bindgen --bin specular-ffi-bindgen "${flags[@]}" -- \
    generate --library "target/aarch64-apple-darwin/$profile/libspecular_ffi.dylib" \
    --language swift --out-dir "$work/bindings"

# One framework per platform. macOS frameworks are versioned bundles; iOS ones are flat.
framework() {
    local target="$1" platform="$2" min="$3"
    local dir="$work/$target/SpecularFFI.framework"
    local lib="target/$target/$profile/libspecular_ffi.dylib"
    local res
    if [ "$platform" = macos ]; then
        mkdir -p "$dir/Versions/A/Headers" "$dir/Versions/A/Modules" "$dir/Versions/A/Resources"
        cp "$lib" "$dir/Versions/A/SpecularFFI"
        install_name_tool -id @rpath/SpecularFFI.framework/Versions/A/SpecularFFI "$dir/Versions/A/SpecularFFI"
        cp "$work/bindings/SpecularFFI.h" "$dir/Versions/A/Headers/"
        res="$dir/Versions/A/Resources"
        ln -s A "$dir/Versions/Current"
        for l in SpecularFFI Headers Modules Resources; do ln -s "Versions/Current/$l" "$dir/$l"; done
        modules="$dir/Versions/A/Modules"
    else
        mkdir -p "$dir/Headers" "$dir/Modules"
        cp "$lib" "$dir/SpecularFFI"
        install_name_tool -id @rpath/SpecularFFI.framework/SpecularFFI "$dir/SpecularFFI"
        cp "$work/bindings/SpecularFFI.h" "$dir/Headers/"
        res="$dir"
        modules="$dir/Modules"
    fi
    cat > "$modules/module.modulemap" <<EOF
framework module SpecularFFI {
    umbrella header "SpecularFFI.h"
    export *
}
EOF
    cat > "$res/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key><string>en</string>
    <key>CFBundleExecutable</key><string>SpecularFFI</string>
    <key>CFBundleIdentifier</key><string>com.amazynk.specular.ffi</string>
    <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
    <key>CFBundleName</key><string>SpecularFFI</string>
    <key>CFBundlePackageType</key><string>FMWK</string>
    <key>CFBundleShortVersionString</key><string>$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)</string>
    <key>CFBundleVersion</key><string>1</string>
    <key>MinimumOSVersion</key><string>$min</string>
</dict>
</plist>
EOF
}
framework aarch64-apple-darwin macos "$macos_target"
framework aarch64-apple-ios ios "$ios_target"
framework aarch64-apple-ios-sim ios "$ios_target"

rm -rf "$out/SpecularFFI.xcframework"
xcodebuild -create-xcframework \
    -framework "$work/aarch64-apple-darwin/SpecularFFI.framework" \
    -framework "$work/aarch64-apple-ios/SpecularFFI.framework" \
    -framework "$work/aarch64-apple-ios-sim/SpecularFFI.framework" \
    -output "$out/SpecularFFI.xcframework"

mkdir -p "$out/Sources/Specular"
cp "$work/bindings/Specular.swift" "$out/Sources/Specular/Specular.swift"
git -C "$root" rev-parse HEAD > "$out/SPECULAR_REVISION"
echo "wrote $out/SpecularFFI.xcframework and $out/Sources/Specular/Specular.swift"
