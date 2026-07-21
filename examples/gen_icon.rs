//! Generates the app icon assets. Run with: cargo run --example gen_icon
//!
//! Draws a magnifying glass on a rounded gradient tile entirely in code, so
//! the icon can be regenerated/tweaked without any design tools.

use image::codecs::ico::{IcoEncoder, IcoFrame};
use image::{DynamicImage, ExtendedColorType, RgbaImage};

const SIZE: u32 = 256;

fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn sd_rounded_rect(x: f32, y: f32, half: f32, radius: f32) -> f32 {
    let qx = x.abs() - (half - radius);
    let qy = y.abs() - (half - radius);
    let ax = qx.max(0.0);
    let ay = qy.max(0.0);
    (ax * ax + ay * ay).sqrt() + qx.max(qy).min(0.0) - radius
}

fn sd_circle(x: f32, y: f32, cx: f32, cy: f32, r: f32) -> f32 {
    ((x - cx).powi(2) + (y - cy).powi(2)).sqrt() - r
}

fn sd_capsule(x: f32, y: f32, ax: f32, ay: f32, bx: f32, by: f32, r: f32) -> f32 {
    let pax = x - ax;
    let pay = y - ay;
    let bax = bx - ax;
    let bay = by - ay;
    let h = ((pax * bax + pay * bay) / (bax * bax + bay * bay)).clamp(0.0, 1.0);
    ((pax - bax * h).powi(2) + (pay - bay * h).powi(2)).sqrt() - r
}

fn over(dst: [f32; 4], src: [f32; 3], alpha: f32) -> [f32; 4] {
    let a = alpha.clamp(0.0, 1.0);
    [
        src[0] * a + dst[0] * (1.0 - a),
        src[1] * a + dst[1] * (1.0 - a),
        src[2] * a + dst[2] * (1.0 - a),
        a + dst[3] * (1.0 - a),
    ]
}

fn render() -> RgbaImage {
    let mut img = RgbaImage::new(SIZE, SIZE);
    let s = SIZE as f32;
    let aa = 1.5; // anti-alias edge width in pixels

    // Glass geometry: ring centered upper-left, handle to lower-right.
    let ring_c = (s * 0.44, s * 0.44);
    let ring_r = s * 0.21;
    let ring_w = s * 0.065;
    let handle_from = (
        ring_c.0 + ring_r * 0.75,
        ring_c.1 + ring_r * 0.75,
    );
    let handle_to = (s * 0.735, s * 0.735);
    let handle_w = s * 0.052;

    for py in 0..SIZE {
        for px in 0..SIZE {
            let x = px as f32 + 0.5;
            let y = py as f32 + 0.5;
            let mut c: [f32; 4] = [0.0, 0.0, 0.0, 0.0];

            // Rounded-square tile with a vertical blue→violet gradient.
            let tile = sd_rounded_rect(x - s / 2.0, y - s / 2.0, s * 0.46, s * 0.115);
            let tile_a = 1.0 - smoothstep(-aa, aa, tile);
            if tile_a > 0.0 {
                let t = y / s;
                let d = x / s;
                let top = [0.13 + 0.10 * d, 0.36 - 0.08 * d, 0.86];
                let bottom = [0.42 + 0.08 * d, 0.16, 0.72];
                let bg = [
                    top[0] * (1.0 - t) + bottom[0] * t,
                    top[1] * (1.0 - t) + bottom[1] * t,
                    top[2] * (1.0 - t) + bottom[2] * t,
                ];
                c = over(c, bg, tile_a);

                // Soft inner glow in the lens area.
                let lens = sd_circle(x, y, ring_c.0, ring_c.1, ring_r - ring_w * 0.4);
                let lens_a = (1.0 - smoothstep(-ring_r, ring_r * 0.4, lens)) * 0.28 * tile_a;
                c = over(c, [0.75, 0.9, 1.0], lens_a);

                // Handle (draw first so the ring overlaps its join).
                let handle = sd_capsule(
                    x, y,
                    handle_from.0, handle_from.1,
                    handle_to.0, handle_to.1,
                    handle_w,
                );
                let handle_a = (1.0 - smoothstep(-aa, aa, handle)) * tile_a;
                c = over(c, [1.0, 1.0, 1.0], handle_a);

                // Ring.
                let ring = sd_circle(x, y, ring_c.0, ring_c.1, ring_r).abs() - ring_w;
                let ring_a = (1.0 - smoothstep(-aa, aa, ring)) * tile_a;
                c = over(c, [1.0, 1.0, 1.0], ring_a);

                // Glint on the lens, upper-left.
                let glint = sd_circle(
                    x, y,
                    ring_c.0 - ring_r * 0.38,
                    ring_c.1 - ring_r * 0.38,
                    ring_r * 0.16,
                );
                let glint_a = (1.0 - smoothstep(-aa, aa * 2.0, glint)) * 0.8 * tile_a;
                c = over(c, [1.0, 1.0, 1.0], glint_a);
            }

            img.put_pixel(
                px,
                py,
                image::Rgba([
                    (c[0] * 255.0).round() as u8,
                    (c[1] * 255.0).round() as u8,
                    (c[2] * 255.0).round() as u8,
                    (c[3] * 255.0).round() as u8,
                ]),
            );
        }
    }
    img
}

fn main() {
    let base = render();
    std::fs::create_dir_all("assets").unwrap();

    let dynamic = DynamicImage::ImageRgba8(base);
    dynamic.save("assets/icon-256.png").unwrap();

    let sizes = [16u32, 24, 32, 48, 64, 128, 256];
    let frames: Vec<RgbaImage> = sizes
        .iter()
        .map(|&sz| {
            dynamic
                .resize_exact(sz, sz, image::imageops::FilterType::Lanczos3)
                .to_rgba8()
        })
        .collect();
    let ico_frames: Vec<IcoFrame> = sizes
        .iter()
        .zip(&frames)
        .map(|(&sz, img)| IcoFrame::as_png(img.as_raw(), sz, sz, ExtendedColorType::Rgba8).unwrap())
        .collect();

    let file = std::fs::File::create("assets/icon.ico").unwrap();
    IcoEncoder::new(file).encode_images(&ico_frames).unwrap();

    // Placeholder splash (swapped for real brand art when available): the
    // icon centered on a dark gradient, 1280x720.
    let (sw, sh) = (1280u32, 720u32);
    let mut splash = RgbaImage::new(sw, sh);
    for y in 0..sh {
        let t = y as f32 / sh as f32;
        let r = (10.0 + 14.0 * t) as u8;
        let g = (12.0 + 10.0 * t) as u8;
        let b = (34.0 + 26.0 * t) as u8;
        for x in 0..sw {
            splash.put_pixel(x, y, image::Rgba([r, g, b, 255]));
        }
    }
    let badge = dynamic.resize_exact(320, 320, image::imageops::FilterType::Lanczos3);
    image::imageops::overlay(
        &mut splash,
        &badge,
        (sw as i64 - 320) / 2,
        (sh as i64 - 320) / 2,
    );
    DynamicImage::ImageRgba8(splash)
        .save("assets/splash.png")
        .unwrap();

    println!("wrote assets/icon-256.png, assets/icon.ico, and assets/splash.png");
}
