use super::*;

fn jpeg(w: u16, h: u16) -> Vec<u8> {
    let mut rgb = Vec::with_capacity(w as usize * h as usize * 3);
    for y in 0..h {
        for x in 0..w {
            rgb.extend_from_slice(&[(x % 256) as u8, (y % 256) as u8, 128]);
        }
    }
    let mut out = Vec::new();
    jpeg_encoder::Encoder::new(&mut out, 90)
        .encode(&rgb, w, h, jpeg_encoder::ColorType::Rgb)
        .unwrap();
    out
}

#[test]
fn opens_and_decodes_a_jpeg_as_planar_ycbcr() {
    let doc = MediaDocument::open_bytes("a.jpg".into(), jpeg(640, 480)).unwrap();
    let info = doc.info();
    assert_eq!(info.format_key, "jpeg");
    assert_eq!((info.display_width, info.display_height), (640, 480));
    assert_eq!(info.kind, MediaKind::Image);
    assert!(matches!(info.transform, ColourTransform::Srgb));
    let px = doc.decode(2, None).unwrap();
    assert_eq!((px.width, px.height, px.scale), (320, 240, 2));
    assert_eq!(px.kind, PixelKind::Planar8 { ycbcr: true });
    assert_eq!(px.planes.len(), 3);
    for p in &px.planes {
        assert!(p.bytes_per_row >= p.width);
        assert!(p.data.len() >= (p.bytes_per_row * (p.height - 1) + p.width) as usize);
    }
}

#[test]
fn decodes_regions() {
    let doc = MediaDocument::open_bytes("a.jpg".into(), jpeg(1024, 768)).unwrap();
    doc.prepare_regions().unwrap();
    let r = MediaRect {
        x: 512,
        y: 256,
        width: 256,
        height: 256,
    };
    let px = doc.decode(1, Some(r)).unwrap();
    assert_eq!((px.origin_x, px.origin_y), (512, 256));
    assert_eq!((px.width, px.height), (256, 256));
}

#[test]
fn rejects_what_no_decoder_reads() {
    let e = MediaDocument::open_bytes("a.zzz".into(), vec![0x13; 64])
        .err()
        .unwrap();
    assert!(matches!(
        e,
        MediaError::Unsupported { .. } | MediaError::Malformed { .. }
    ));
}

#[test]
fn claims_hundreds_of_extensions() {
    let exts = supported_extensions();
    assert!(exts.len() > 300, "{}", exts.len());
    for e in [
        "jpg", "png", "heic", "avif", "jxl", "dng", "nef", "exr", "mp4", "gif", "pdf", "svg",
    ] {
        assert!(supports_extension(e.into()), "{e}");
    }
}

#[test]
fn wide_layouts_become_half_floats_or_u16() {
    let raster = |layout, bytes: Vec<u8>, bpp: usize| Raster {
        width: 2,
        height: 1,
        origin: (0, 0),
        scale: 1,
        layout,
        planes: vec![specular_core::RasterPlane {
            width: 2,
            height: 1,
            stride: 2 * bpp,
            data: bytes,
            h_div: 1,
            v_div: 1,
        }],
        truncated: false,
    };
    let g = pixels(raster(
        PixelLayout::Gray16,
        [0u16, 65535].iter().flat_map(|v| v.to_ne_bytes()).collect(),
        2,
    ));
    assert_eq!(g.kind, PixelKind::Rgba16Unorm);
    assert_eq!(g.planes[0].data.len(), 16);
    assert_eq!(get_u16(&g.planes[0].data, 4), 65535);
    assert_eq!(get_u16(&g.planes[0].data, 7), 65535);

    let half = |v: f32| f32_to_f16(v).to_ne_bytes();
    let bytes: Vec<u8> = [1.0f32, 0.0, 0.0, 1.0, 0.0, 1.0, 0.0, 1.0]
        .iter()
        .flat_map(|&v| half(v))
        .collect();
    let p3 = pixels(raster(PixelLayout::Rgba16F(Primaries::DisplayP3), bytes, 8));
    assert_eq!(p3.kind, PixelKind::Rgba16Float);
    // P3 red is outside sRGB: more than 1 in red, negative green.
    let red = f16_to_f32(get_u16(&p3.planes[0].data, 0));
    let green = f16_to_f32(get_u16(&p3.planes[0].data, 1));
    assert!(red > 1.0 && green < 0.0, "{red} {green}");
}
