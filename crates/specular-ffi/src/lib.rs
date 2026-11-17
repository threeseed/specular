//! Specular's decoders for other apps, through UniFFI.
//!
//! A host app (Matrix's Swift client) opens a file with [`MediaDocument::open`] and draws it
//! on its own GPU: this crate hands out what Specular's renderer uploads, not finished
//! bitmaps. Pixels are never rotated (the EXIF orientation is in [`MediaInfo`] for the view
//! transform), 8-bit YCbCr and planar RGB stay planar for the shader to convert, and the
//! embedded ICC profile comes as the same source transform Specular's convert shader applies
//! (per-channel curves and a matrix, or a 33³ cube, both into linear BT.709).
//!
//! What a host does with it:
//!
//! 1. [`MediaDocument::open`] (cheap: headers only) and [`MediaDocument::info`].
//! 2. [`MediaDocument::decode`] at a coarse scale for the first picture, then the finer
//!    levels or, zoomed in on a big image, [`MediaDocument::decode`] with a region per tile.
//! 3. For animations and movies, [`MediaDocument::frames`] and [`MediaFrames::next_frame`].
//! 4. For multi-page files, [`MediaDocument::open_page`].
//!
//! Every format of the registry is here: still images, camera RAW, HDR, documents, the
//! drawings, scientific and medical formats, and movies through `specular-video` (their
//! frames; sound is the host's to play).

// UniFFI's scaffolding is generated `unsafe` code (UNSAFE.md, `specular-ffi`); the crate's
// own code has none.
#![allow(unsafe_code)]

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use parking_lot::Mutex;
use specular_core::hdr::{decode16, f16_to_f32, f32_to_f16, float_exposure, mat_mul, to_bt709};
use specular_core::{CancelToken, HdrEncoding, PixelLayout, Primaries, Raster, Source, Transfer};
use specular_decode::{DecodeError, DecodeRequest, Frames, Registry, Session};

uniffi::setup_scaffolding!("specular");

/// The registry every document opens through, with movies and sound installed.
fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        specular_video::install();
        Registry::new()
    })
}

/// Why a file could not be shown.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum MediaError {
    /// No decoder reads the file.
    #[error("unsupported format")]
    Unsupported { message: String },
    /// The file is damaged or cut short.
    #[error("{message}")]
    Malformed { message: String },
    /// The file could not be read.
    #[error("{message}")]
    Io { message: String },
    /// [`MediaDocument::cancel`] stopped the decode.
    #[error("cancelled")]
    Cancelled,
    /// The image needs more memory than the budget allows.
    #[error("{message}")]
    TooLarge { message: String },
}

impl From<DecodeError> for MediaError {
    fn from(e: DecodeError) -> MediaError {
        let message = e.to_string();
        match e {
            DecodeError::Cancelled => MediaError::Cancelled,
            DecodeError::UnsupportedFormat
            | DecodeError::Unsupported(_)
            | DecodeError::NotOneOf { .. }
            | DecodeError::Ambiguous(_)
            | DecodeError::NeedsDecoder(_) => MediaError::Unsupported { message },
            DecodeError::Io(_) => MediaError::Io { message },
            DecodeError::TooLarge | DecodeError::NeedsMemory(_) => MediaError::TooLarge { message },
            _ => MediaError::Malformed { message },
        }
    }
}

impl From<std::io::Error> for MediaError {
    fn from(e: std::io::Error) -> MediaError {
        MediaError::Io {
            message: e.to_string(),
        }
    }
}

/// How the planes of [`MediaPixels`] are laid out and coded, which is how the host uploads
/// and converts them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum PixelKind {
    /// One 8-bit plane of grey, coded like [`PixelKind::Rgba8`].
    Gray8,
    /// One interleaved RGBA8 plane, straight alpha. The RGB is coded: through the
    /// document's [`ColourTransform`], or sRGB without one.
    Rgba8,
    /// Three 8-bit planes, each at its own size (subsampled chroma is sited centred, so
    /// sampling every plane at the same normalised position is right). `ycbcr`: Y, Cb, Cr in
    /// BT.601 full range (JFIF); otherwise R, G, B. Coded like [`PixelKind::Rgba8`] after the
    /// YCbCr → RGB matrix.
    Planar8 { ycbcr: bool },
    /// One interleaved plane of native-endian `u16` RGBA (straight alpha), coded like
    /// [`PixelKind::Rgba8`].
    Rgba16Unorm,
    /// One interleaved plane of native-endian half-float RGBA (straight alpha): linear BT.709
    /// light with 1.0 = SDR reference white. May exceed 1 (HDR) and go negative (colours
    /// outside sRGB), the host's extended linear sRGB. Never goes through the transform.
    Rgba16Float,
}

/// One plane of pixels.
#[derive(Clone, Debug, uniffi::Record)]
pub struct MediaPlane {
    pub width: u32,
    pub height: u32,
    pub bytes_per_row: u32,
    pub data: Vec<u8>,
}

/// Decoded pixels of (part of) an image at a scale.
#[derive(Clone, Debug, uniffi::Record)]
pub struct MediaPixels {
    /// Size in pixels at this scale (the first plane's).
    pub width: u32,
    pub height: u32,
    /// Top-left inside the scaled image (non-zero for region decodes).
    pub origin_x: u32,
    pub origin_y: u32,
    /// Downscale factor relative to the full image (1, 2, 4, 8…).
    pub scale: u32,
    pub kind: PixelKind,
    pub planes: Vec<MediaPlane>,
    /// For [`PixelKind::Rgba16Float`]: the factor to multiply by before display (1 for light;
    /// below 1 for data far above reference white, so its 99.5th percentile is white).
    pub exposure: f32,
    /// The content's peak after `exposure`, in units of reference white (1 for SDR): the
    /// host tone-maps when the display's headroom is lower.
    pub peak: f32,
    /// The data ended early; the missing part is grey.
    pub truncated: bool,
}

/// The source transform of an embedded ICC profile, which Specular's convert shader applies
/// to coded RGB (every [`PixelKind`] but [`PixelKind::Rgba16Float`]).
#[derive(Clone, Debug, uniffi::Enum)]
pub enum ColourTransform {
    /// sRGB (or near enough, or no profile): the exact sRGB decode.
    Srgb,
    /// `samples` RGBA half-float texels of per-channel curves (coded 0…1 across the texels
    /// → linear), then `rgb' = M · rgb + offset` with `matrix` three rows of
    /// (m0, m1, m2, offset), into linear BT.709.
    Curves { samples: Vec<u16>, matrix: Vec<f32> },
    /// A `size`³ table of RGBA half-float texels, red fastest: coded RGB → linear BT.709.
    Cube { size: u32, samples: Vec<u16> },
}

/// What kind of thing the document is, for the host's chrome (a play bar, page arrows).
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum MediaKind {
    Image,
    Animation,
    Movie,
    Sound,
    Document,
}

/// What an opened document is.
#[derive(Clone, Debug, uniffi::Record)]
pub struct MediaInfo {
    /// The format's stable key ("jpeg", "nef"…) and its display name.
    pub format_key: String,
    pub format_name: String,
    pub kind: MediaKind,
    /// Stored size (before orientation).
    pub width: u32,
    pub height: u32,
    /// EXIF orientation 1…8, applied by the view (flip first, then quarter turns clockwise).
    pub orientation: u8,
    /// Size as displayed (orientation applied).
    pub display_width: u32,
    pub display_height: u32,
    pub has_alpha: bool,
    pub bit_depth: u8,
    /// Short technical description of the coding ("baseline 4:2:0").
    pub coding: String,
    /// Downscale factors the decoder supports natively, finest first.
    pub scales: Vec<u32>,
    /// Regions decode cheaply (worth tiling when zoomed in).
    pub fast_regions: bool,
    pub page_count: u32,
    pub page_index: u32,
    /// Frames to play (`None` when not stated up front, as in GIF).
    pub frame_count: Option<u32>,
    /// Times to play an animation; 0 forever.
    pub loop_count: u32,
    /// A movie's or sound's duration.
    pub duration_ms: Option<u64>,
    pub frame_rate: Option<f64>,
    /// A movie has sound (which the host plays: frames come from here).
    pub has_sound: bool,
    pub transform: ColourTransform,
}

/// A rectangle in scaled image pixels.
#[derive(Clone, Copy, Debug, uniffi::Record)]
pub struct MediaRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// One opened file (or one page of it). Shared between threads: decodes may run
/// concurrently, and [`MediaDocument::cancel`] stops those in progress.
#[derive(uniffi::Object)]
pub struct MediaDocument {
    session: Arc<dyn Session>,
    cancel: Mutex<CancelToken>,
}

#[uniffi::export]
impl MediaDocument {
    /// Opens the file at `path`: reads its headers, decodes nothing.
    #[uniffi::constructor]
    pub fn open(path: String) -> Result<Arc<MediaDocument>, MediaError> {
        let source = Source::open(&path)?;
        Ok(Self::wrap(registry().open(Arc::new(source))?))
    }

    /// Opens a file held in memory; `name` (with its extension) helps identify formats
    /// without a signature.
    #[uniffi::constructor]
    pub fn open_bytes(name: String, data: Vec<u8>) -> Result<Arc<MediaDocument>, MediaError> {
        let source = Source::from_vec(name, data);
        Ok(Self::wrap(registry().open(Arc::new(source))?))
    }

    pub fn info(&self) -> MediaInfo {
        let info = self.session.info();
        let (display_width, display_height) = info.display_size();
        let animation = self.session.animation();
        let video = self.session.video();
        let kind = if let Some(v) = video {
            if v.frames > 0 {
                MediaKind::Movie
            } else {
                MediaKind::Sound
            }
        } else if animation.is_some() {
            MediaKind::Animation
        } else if self.session.page_count() > 1 || info.format.is_text() {
            MediaKind::Document
        } else {
            MediaKind::Image
        };
        MediaInfo {
            format_key: specular_decode::ident::key(info.format).to_ascii_lowercase(),
            format_name: info.format.name().to_owned(),
            kind,
            width: info.width,
            height: info.height,
            orientation: info.orientation.0,
            display_width,
            display_height,
            has_alpha: info.has_alpha,
            bit_depth: info.bit_depth,
            coding: info.coding.clone(),
            scales: info.scales.to_vec(),
            fast_regions: info.fast_regions,
            page_count: self.session.page_count(),
            page_index: self.session.page_index(),
            frame_count: animation.and_then(|a| a.frame_count),
            loop_count: animation.map_or(0, |a| a.loop_count),
            duration_ms: video.and_then(|v| v.duration_us).map(|us| us / 1000),
            frame_rate: video.and_then(|v| v.frame_rate),
            has_sound: video.is_some_and(|v| v.audio.is_some()),
            transform: transform_of(info.icc_profile.as_deref().map(Vec::as_slice)),
        }
    }

    /// The embedded preview (a camera's JPEG, an EXIF thumbnail) at about `long_side`, for
    /// showing something before the first decode finishes. `None` when there is none.
    pub fn decode_preview(&self, long_side: u32) -> Result<Option<MediaPixels>, MediaError> {
        match self.session.decode_preview_for(long_side) {
            Some(r) => Ok(Some(pixels(r?))),
            None => Ok(None),
        }
    }

    /// Decodes the image at 1/`scale` (one of [`MediaInfo::scales`], or any power of two
    /// the decoder reduces to), the whole of it or `region` in scaled pixels.
    pub fn decode(&self, scale: u32, region: Option<MediaRect>) -> Result<MediaPixels, MediaError> {
        let mut req = DecodeRequest::full(scale.max(1));
        req.region = region.map(|r| specular_decode::Rect {
            x: r.x,
            y: r.y,
            w: r.width,
            h: r.height,
        });
        let cancel = self.cancel.lock().clone();
        Ok(pixels(self.session.decode(&req, &cancel)?))
    }

    /// Builds what makes region decodes fast (the JPEG row index); idempotent. Worth calling
    /// on a worker before tiling a big image.
    pub fn prepare_regions(&self) -> Result<(), MediaError> {
        let cancel = self.cancel.lock().clone();
        Ok(self.session.prepare_regions(&cancel)?)
    }

    /// Announces the whole-image levels (1/scale) about to be requested, so decoders that
    /// produce several in one pass do.
    pub fn want_levels(&self, scales: Vec<u32>) {
        self.session.want_levels(&scales);
    }

    /// Opens page `page` (0-based, below [`MediaInfo::page_count`]).
    pub fn open_page(&self, page: u32) -> Result<Arc<MediaDocument>, MediaError> {
        Ok(Self::wrap(self.session.open_page(page)?))
    }

    /// Starts decoding frames at 1/`scale`, for animations and movies.
    pub fn frames(&self, scale: u32) -> Result<Arc<MediaFrames>, MediaError> {
        let frames = self.session.frames(scale.max(1))?;
        Ok(Arc::new(MediaFrames {
            frames: Mutex::new(frames),
            cancel: CancelToken::new(),
        }))
    }

    /// Stops the decodes in progress (they fail with [`MediaError::Cancelled`]); later
    /// decodes run again.
    pub fn cancel(&self) {
        let mut c = self.cancel.lock();
        c.cancel();
        *c = CancelToken::new();
    }

    /// Drops the caches that only speed up later decodes (under memory pressure). Returns
    /// the bytes released.
    pub fn trim_memory(&self) -> u64 {
        self.session.trim_memory() as u64
    }
}

impl MediaDocument {
    fn wrap(session: Arc<dyn Session>) -> Arc<MediaDocument> {
        Arc::new(MediaDocument {
            session,
            cancel: Mutex::new(CancelToken::new()),
        })
    }
}

/// One frame of an animation or movie.
#[derive(Clone, Debug, uniffi::Record)]
pub struct MediaFrame {
    pub index: u32,
    /// How long to show it, as stored.
    pub delay_ms: u32,
    /// The whole canvas, ready to show.
    pub pixels: MediaPixels,
}

/// A sequential frame decoder.
#[derive(uniffi::Object)]
pub struct MediaFrames {
    frames: Mutex<Box<dyn Frames>>,
    cancel: CancelToken,
}

#[uniffi::export]
impl MediaFrames {
    /// The next frame, or `None` after the last.
    pub fn next_frame(&self) -> Result<Option<MediaFrame>, MediaError> {
        let mut f = self.frames.lock();
        match f.next_frame(&self.cancel) {
            None => Ok(None),
            Some(r) => {
                let frame = r?;
                Ok(Some(MediaFrame {
                    index: frame.index,
                    delay_ms: millis(frame.delay),
                    pixels: pixels(frame.raster),
                }))
            }
        }
    }

    /// Restarts at frame 0.
    pub fn rewind(&self) -> Result<(), MediaError> {
        Ok(self.frames.lock().rewind()?)
    }

    /// Positions the decoder at frame `index`; false when it cannot seek (rewind and step).
    pub fn seek(&self, index: u32) -> Result<bool, MediaError> {
        Ok(self.frames.lock().seek(index)?)
    }

    /// Every frame's delay, when the container states them up front.
    pub fn delays_ms(&self) -> Option<Vec<u32>> {
        self.frames
            .lock()
            .delays()
            .map(|d| d.into_iter().map(millis).collect())
    }
}

fn millis(d: Duration) -> u32 {
    u32::try_from(d.as_millis()).unwrap_or(u32::MAX)
}

/// Whether a decoder claims files with this extension (without the dot).
#[uniffi::export]
pub fn supports_extension(ext: String) -> bool {
    registry().supports_extension(std::path::Path::new(&format!("x.{ext}")))
}

/// Every extension a decoder claims, lower case, sorted.
#[uniffi::export]
pub fn supported_extensions() -> Vec<String> {
    registry()
        .extension_claims()
        .into_keys()
        .map(str::to_owned)
        .collect()
}

/// Specular's version.
#[uniffi::export]
pub fn specular_version() -> String {
    env!("CARGO_PKG_VERSION").to_owned()
}

fn transform_of(icc: Option<&[u8]>) -> ColourTransform {
    match icc.and_then(specular_color::policy::source_lut) {
        None => ColourTransform::Srgb,
        Some(specular_color::SourceLut::Curves { data, matrix }) => ColourTransform::Curves {
            samples: data,
            matrix: matrix.to_vec(),
        },
        Some(specular_color::SourceLut::Cube { data }) => ColourTransform::Cube {
            size: specular_color::gpu::CUBE_SIZE as u32,
            samples: data,
        },
    }
}

/// A raster as the host uploads it: the 8-bit layouts as they are, wide coded RGB as
/// `u16`, everything linear or HDR as half floats in BT.709.
fn pixels(r: Raster) -> MediaPixels {
    let r = match r.layout {
        PixelLayout::Yuv(_) => match specular_core::yuv::to_rgb(&r) {
            Some(rgb) => rgb,
            None => r,
        },
        _ => r,
    };
    let (exposure, peak) = match r.layout {
        PixelLayout::Rgba16F(_) => float_exposure(&r),
        PixelLayout::Rgba16(e) if e.is_hdr() => (1.0, specular_core::hdr::peak_from_nits(None)),
        _ => (1.0, 1.0),
    };
    let (kind, planes) = match r.layout {
        PixelLayout::Gray => (PixelKind::Gray8, first_plane(&r)),
        PixelLayout::Rgba => (PixelKind::Rgba8, first_plane(&r)),
        PixelLayout::YCbCr => (PixelKind::Planar8 { ycbcr: true }, all_planes(&r, 3)),
        PixelLayout::Rgb => (PixelKind::Planar8 { ycbcr: false }, all_planes(&r, 3)),
        PixelLayout::Rgba16(HdrEncoding {
            transfer: Transfer::Srgb,
            primaries: Primaries::Bt709,
        }) => (PixelKind::Rgba16Unorm, first_plane(&r)),
        PixelLayout::Rgba16(e) => (PixelKind::Rgba16Float, vec![rgba16_to_float(&r, e)]),
        PixelLayout::Gray16 => (PixelKind::Rgba16Unorm, vec![gray16_to_rgba16(&r, false)]),
        PixelLayout::GrayAlpha16 => (PixelKind::Rgba16Unorm, vec![gray16_to_rgba16(&r, true)]),
        PixelLayout::Rgba16F(Primaries::Bt709) => (PixelKind::Rgba16Float, first_plane(&r)),
        PixelLayout::Rgba16F(p) => (PixelKind::Rgba16Float, vec![float_to_bt709(&r, p)]),
        // `to_rgb` failed (planes missing): mid grey rather than nothing.
        PixelLayout::Yuv(_) => (PixelKind::Gray8, vec![grey_plane(r.width, r.height)]),
    };
    MediaPixels {
        width: r.width,
        height: r.height,
        origin_x: r.origin.0,
        origin_y: r.origin.1,
        scale: r.scale,
        kind,
        planes,
        exposure,
        peak,
        truncated: r.truncated,
    }
}

fn plane(p: &specular_core::RasterPlane) -> MediaPlane {
    MediaPlane {
        width: p.width,
        height: p.height,
        bytes_per_row: u32::try_from(p.stride).unwrap_or(u32::MAX),
        data: p.data.clone(),
    }
}

fn first_plane(r: &Raster) -> Vec<MediaPlane> {
    match r.planes.first() {
        Some(p) => vec![plane(p)],
        None => vec![grey_plane(r.width, r.height)],
    }
}

fn all_planes(r: &Raster, n: usize) -> Vec<MediaPlane> {
    if r.planes.len() < n {
        return vec![grey_plane(r.width, r.height); n];
    }
    r.planes[..n].iter().map(plane).collect()
}

fn grey_plane(width: u32, height: u32) -> MediaPlane {
    MediaPlane {
        width,
        height,
        bytes_per_row: width,
        data: vec![128; width as usize * height as usize],
    }
}

/// Rows of `n`-byte pixels of the first plane, packed, through `f` into 8-byte pixels.
fn map_rows(r: &Raster, n: usize, f: impl Fn(&[u8], &mut [u8])) -> MediaPlane {
    let (w, h) = (r.width as usize, r.height as usize);
    let mut out = vec![0u8; w * h * 8];
    if let Some(p) = r.planes.first() {
        for y in 0..h {
            let Some(row) = p.data.get(y * p.stride..y * p.stride + w * n) else {
                break;
            };
            for (src, dst) in row
                .chunks_exact(n)
                .zip(out[y * w * 8..(y + 1) * w * 8].chunks_exact_mut(8))
            {
                f(src, dst);
            }
        }
    }
    MediaPlane {
        width: r.width,
        height: r.height,
        bytes_per_row: u32::try_from(w * 8).unwrap_or(u32::MAX),
        data: out,
    }
}

fn put_u16(dst: &mut [u8], v: [u16; 4]) {
    for (c, v) in dst.chunks_exact_mut(2).zip(v) {
        c.copy_from_slice(&v.to_ne_bytes());
    }
}

fn get_u16(src: &[u8], i: usize) -> u16 {
    u16::from_ne_bytes([src[2 * i], src[2 * i + 1]])
}

fn gray16_to_rgba16(r: &Raster, alpha: bool) -> MediaPlane {
    let n = if alpha { 4 } else { 2 };
    map_rows(r, n, |s, d| {
        let g = get_u16(s, 0);
        let a = if alpha { get_u16(s, 1) } else { u16::MAX };
        put_u16(d, [g, g, g, a]);
    })
}

fn rgba16_to_float(r: &Raster, e: HdrEncoding) -> MediaPlane {
    map_rows(r, 8, |s, d| {
        let rgb = decode16(e, [get_u16(s, 0), get_u16(s, 1), get_u16(s, 2)]);
        let a = f32::from(get_u16(s, 3)) / 65535.0;
        put_u16(d, [rgb[0], rgb[1], rgb[2], a].map(f32_to_f16));
    })
}

fn float_to_bt709(r: &Raster, p: Primaries) -> MediaPlane {
    let m = to_bt709(p);
    map_rows(r, 8, |s, d| {
        let v = [0, 1, 2].map(|i| {
            let v = f16_to_f32(get_u16(s, i));
            if v.is_finite() { v } else { 0.0 }
        });
        let rgb = mat_mul(m, v);
        put_u16(
            d,
            [
                f32_to_f16(rgb[0]),
                f32_to_f16(rgb[1]),
                f32_to_f16(rgb[2]),
                get_u16(s, 3),
            ],
        );
    })
}

#[cfg(test)]
mod tests;
