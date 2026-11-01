//! QR codes affichés à l'écran (page de configuration des sources).

use qrcode::{Color, QrCode};
use slint::{Image, Rgba8Pixel, SharedPixelBuffer};

/// QR code net (modules de `scale` pixels, marge de 4 modules), noir sur blanc.
pub fn image(data: &str) -> Image {
    let Ok(code) = QrCode::new(data.as_bytes()) else { return Image::default() };
    let n = code.width();
    let colors = code.to_colors();
    let scale = 10usize;
    let quiet = 4usize;
    let size = (n + 2 * quiet) * scale;
    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(size as u32, size as u32);
    let px = buf.make_mut_slice();
    for (i, p) in px.iter_mut().enumerate() {
        let (x, y) = (i % size / scale, i / size / scale);
        let dark = x >= quiet && y >= quiet && x < n + quiet && y < n + quiet && colors[(y - quiet) * n + (x - quiet)] == Color::Dark;
        let v = if dark { 0 } else { 255 };
        *p = Rgba8Pixel { r: v, g: v, b: v, a: 255 };
    }
    Image::from_rgba8(buf)
}
