//! Renders a small color swatch as an inline SVG `data:` URI for use in hover
//! markdown. The hover *type line* lives in a fenced ` ```lua ` block where
//! markdown images do not render, so the swatch is emitted into the hover
//! *description* region (plain markdown), mirroring the realm badge.

/// Side length of the rendered swatch, in pixels.
const SWATCH_SIZE: u32 = 14;

/// Builds the description-region markdown for a color swatch: an inline SVG
/// image followed by the `Color(r, g, b[, a])` text.
///
/// `alpha` is included in the text only when it is not fully opaque.
pub(crate) fn color_swatch_markdown(red: u8, green: u8, blue: u8, alpha: u8) -> String {
    let data_uri = color_swatch_data_uri(red, green, blue);
    let text = if alpha == u8::MAX {
        format!("Color({}, {}, {})", red, green, blue)
    } else {
        format!("Color({}, {}, {}, {})", red, green, blue, alpha)
    };
    format!("![]({}) `{}`", data_uri, text)
}

/// Builds a `data:image/svg+xml;base64,...` URI for a solid color square.
pub(crate) fn color_swatch_data_uri(red: u8, green: u8, blue: u8) -> String {
    let svg = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{size}\" height=\"{size}\">\
<rect width=\"{size}\" height=\"{size}\" rx=\"2\" fill=\"#{r:02X}{g:02X}{b:02X}\" \
stroke=\"#88888880\" stroke-width=\"1\"/></svg>",
        size = SWATCH_SIZE,
        r = red,
        g = green,
        b = blue,
    );
    format!(
        "data:image/svg+xml;base64,{}",
        base64_encode(svg.as_bytes())
    )
}

/// Minimal standard base64 encoder (no padding omission, no line wrapping).
/// Kept local to avoid adding a dependency for a tiny fixed-size payload.
fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;

        out.push(TABLE[((triple >> 18) & 0x3F) as usize] as char);
        out.push(TABLE[((triple >> 12) & 0x3F) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((triple >> 6) & 0x3F) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(triple & 0x3F) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base64_decode(input: &str) -> Vec<u8> {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let lookup = |c: u8| TABLE.iter().position(|&t| t == c).unwrap() as u32;
        let bytes: Vec<u8> = input.bytes().filter(|&b| b != b'=').collect();
        let mut out = Vec::new();
        for chunk in bytes.chunks(4) {
            let mut buf = 0u32;
            for (i, &c) in chunk.iter().enumerate() {
                buf |= lookup(c) << (18 - 6 * i);
            }
            out.push((buf >> 16) as u8);
            if chunk.len() > 2 {
                out.push((buf >> 8) as u8);
            }
            if chunk.len() > 3 {
                out.push(buf as u8);
            }
        }
        out
    }

    #[test]
    fn base64_roundtrips_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn data_uri_decodes_to_svg_with_expected_fill() {
        let uri = color_swatch_data_uri(255, 0, 0);
        let b64 = uri.strip_prefix("data:image/svg+xml;base64,").unwrap();
        let svg = String::from_utf8(base64_decode(b64)).unwrap();
        assert!(svg.contains("fill=\"#FF0000\""), "svg was: {svg}");
        assert!(svg.contains("<svg"));
    }

    #[test]
    fn swatch_markdown_includes_image_and_text() {
        let md = color_swatch_markdown(255, 0, 0, 255);
        assert!(md.contains("data:image/svg+xml;base64,"));
        assert!(md.contains("`Color(255, 0, 0)`"));
        assert!(!md.contains("0, 0, 255)")); // no spurious alpha when opaque
    }

    #[test]
    fn swatch_markdown_includes_alpha_when_translucent() {
        let md = color_swatch_markdown(255, 0, 0, 128);
        assert!(md.contains("`Color(255, 0, 0, 128)`"));
    }
}
