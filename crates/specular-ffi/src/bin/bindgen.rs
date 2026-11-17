//! `cargo run -p specular-ffi --features bindgen --bin specular-ffi-bindgen -- generate …`:
//! writes the Swift (or Kotlin) bindings of the library.

fn main() {
    uniffi::uniffi_bindgen_main()
}
