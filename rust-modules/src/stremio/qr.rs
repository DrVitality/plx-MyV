//! The sign-in QR, drawn by the bridge in the exact form the sign-in screen expects of plex.tv's:
//! a PNG of WHITE modules on a TRANSPARENT ground, no quiet zone. The screen tints that image
//! black over its own white card, and the card's padding is the quiet zone
//! (`screens/login.rs`, `draw_waiting`), so any other colouring would draw an unscannable code.

/// Pixels per QR module. The screen scales the image to its card, so this only sets how much
/// detail survives that scaling; plex.tv's own images are about this size.
const SCALE: usize = 8;

/// `text` encoded as a QR code and rendered to PNG bytes. `None` only when the text is too long
/// for any QR version, which a link URL never is.
pub fn png(text: &str) -> Option<Vec<u8>> {
    let code = qrcodegen::QrCode::encode_text(text, qrcodegen::QrCodeEcc::Medium).ok()?;
    let modules = usize::try_from(code.size()).ok()?;
    let side = modules * SCALE;
    let mut rgba = vec![0u8; side * side * 4];
    for y in 0..side {
        for x in 0..side {
            // `get_module` takes i32 coordinates; both are below `code.size()`, an i32.
            let dark = code.get_module((x / SCALE) as i32, (y / SCALE) as i32);
            if dark {
                let at = (y * side + x) * 4;
                rgba[at..at + 4].copy_from_slice(&[255, 255, 255, 255]);
            }
        }
    }
    let mut out = Vec::new();
    let encoder = image::codecs::png::PngEncoder::new(&mut out);
    image::ImageEncoder::write_image(encoder, &rgba, side as u32, side as u32,
        image::ExtendedColorType::Rgba8).ok()?;
    Some(out)
}
