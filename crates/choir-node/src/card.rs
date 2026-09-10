//! A social-preview card drawn per repository, as a PNG this node
//! encodes itself.
//!
//! [`crate::ui::CARD`] is one image for the whole node, and it was the
//! right thing while the only address anybody pasted was an invite. Now
//! every repository page is shareable, and one image across all of them
//! means a channel full of links that are visually identical.
//!
//! **Why this is drawn rather than rendered.** The generic card comes
//! from `scripts/card.html` through a headless browser, which is fine
//! for one image cut at release time and impossible per request. The
//! alternatives were a font crate and an image crate; this workspace
//! hand-rolls base64, JWT and the ssh wire format rather than take a
//! dependency for a bounded job, and a two-colour PNG with a pixel font
//! is a more bounded job than any of those.
//!
//! **Why not SVG,** which would have been fifteen lines: the servers
//! that render these cards do not fetch it. A preview image has to be a
//! raster or it is not a preview image.
//!
//! The encoder writes **stored** deflate blocks, so there is no
//! compressor here either. At one bit per pixel a 1200x630 card is
//! 95 KiB uncompressed, which is smaller than most photographs anybody
//! would have used instead, and the whole zlib layer is a two-byte
//! header, a length-prefixed copy and an Adler-32.

/// Where a per-repository card is served, named once because the route,
/// the `og:image` tag and the tests must agree.
pub(crate) const PREFIX: &str = "/static/card/";

/// The address of `repo`'s card, relative to the node.
pub(crate) fn path(repo: &str) -> String {
    format!("{PREFIX}{repo}.png")
}

/// The card's dimensions, which are the ratio every preview renderer
/// crops to and the smallest size none of them upscales.
const WIDTH: usize = 1200;
const HEIGHT: usize = 630;

/// Row stride at one bit per pixel.
const STRIDE: usize = WIDTH.div_ceil(8);

/// The palette, as the two colours the sheet already names: `--strong`
/// for the ground and `--ground` for the ink, which is the dark shape of
/// the surface rather than the light one. A preview lands in somebody
/// else's window next to somebody else's content, and the dark card is
/// the one that reads as ours rather than as part of theirs.
const GROUND: [u8; 3] = [0x12, 0x10, 0x0e];
const INK: [u8; 3] = [0xfa, 0xf8, 0xf4];

/// Glyph width in pixels, before scaling.
const GW: usize = 5;
/// Glyph height in pixels, before scaling.
const GH: usize = 7;

/// A 5x7 pixel font, upper case only, in the five characters a
/// repository name can be spelled with plus the separator.
///
/// **Upper case only, and that is a decision rather than a shortcut.**
/// Rendering `choir/choir` as `CHOIR/CHOIR` costs 26 glyphs of data and
/// reads as a wordmark, which is what a card is; the exact spelling is
/// in the `og:title` beside it, so nothing is lost that the reader does
/// not already have. Case-faithful would have meant 66 glyphs authored
/// by hand, and a hand-authored glyph that is subtly wrong is a defect
/// nobody can see in a diff.
///
/// Each row is the low five bits, most significant bit leftmost.
const FONT: [(char, [u8; GH]); 41] = [
    (' ', [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
    ('A', [0x0e, 0x11, 0x11, 0x1f, 0x11, 0x11, 0x11]),
    ('B', [0x1e, 0x11, 0x11, 0x1e, 0x11, 0x11, 0x1e]),
    ('C', [0x0e, 0x11, 0x10, 0x10, 0x10, 0x11, 0x0e]),
    ('D', [0x1e, 0x11, 0x11, 0x11, 0x11, 0x11, 0x1e]),
    ('E', [0x1f, 0x10, 0x10, 0x1e, 0x10, 0x10, 0x1f]),
    ('F', [0x1f, 0x10, 0x10, 0x1e, 0x10, 0x10, 0x10]),
    ('G', [0x0e, 0x11, 0x10, 0x17, 0x11, 0x11, 0x0f]),
    ('H', [0x11, 0x11, 0x11, 0x1f, 0x11, 0x11, 0x11]),
    ('I', [0x0e, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0e]),
    ('J', [0x07, 0x02, 0x02, 0x02, 0x02, 0x12, 0x0c]),
    ('K', [0x11, 0x12, 0x14, 0x18, 0x14, 0x12, 0x11]),
    ('L', [0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x1f]),
    ('M', [0x11, 0x1b, 0x15, 0x15, 0x11, 0x11, 0x11]),
    ('N', [0x11, 0x11, 0x19, 0x15, 0x13, 0x11, 0x11]),
    ('O', [0x0e, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0e]),
    ('P', [0x1e, 0x11, 0x11, 0x1e, 0x10, 0x10, 0x10]),
    ('Q', [0x0e, 0x11, 0x11, 0x11, 0x15, 0x12, 0x0d]),
    ('R', [0x1e, 0x11, 0x11, 0x1e, 0x14, 0x12, 0x11]),
    ('S', [0x0f, 0x10, 0x10, 0x0e, 0x01, 0x01, 0x1e]),
    ('T', [0x1f, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04]),
    ('U', [0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0e]),
    ('V', [0x11, 0x11, 0x11, 0x11, 0x11, 0x0a, 0x04]),
    ('W', [0x11, 0x11, 0x11, 0x15, 0x15, 0x1b, 0x11]),
    ('X', [0x11, 0x11, 0x0a, 0x04, 0x0a, 0x11, 0x11]),
    ('Y', [0x11, 0x11, 0x0a, 0x04, 0x04, 0x04, 0x04]),
    ('Z', [0x1f, 0x01, 0x02, 0x04, 0x08, 0x10, 0x1f]),
    ('0', [0x0e, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0e]),
    ('1', [0x04, 0x0c, 0x04, 0x04, 0x04, 0x04, 0x0e]),
    ('2', [0x0e, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1f]),
    ('3', [0x1f, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0e]),
    ('4', [0x02, 0x06, 0x0a, 0x12, 0x1f, 0x02, 0x02]),
    ('5', [0x1f, 0x10, 0x1e, 0x01, 0x01, 0x11, 0x0e]),
    ('6', [0x06, 0x08, 0x10, 0x1e, 0x11, 0x11, 0x0e]),
    ('7', [0x1f, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08]),
    ('8', [0x0e, 0x11, 0x11, 0x0e, 0x11, 0x11, 0x0e]),
    ('9', [0x0e, 0x11, 0x11, 0x0f, 0x01, 0x02, 0x0c]),
    ('/', [0x01, 0x02, 0x02, 0x04, 0x08, 0x08, 0x10]),
    ('-', [0x00, 0x00, 0x00, 0x1f, 0x00, 0x00, 0x00]),
    ('.', [0x00, 0x00, 0x00, 0x00, 0x00, 0x0c, 0x0c]),
    ('_', [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x1f]),
];

/// The glyph for `c`, upper-cased, or the space glyph.
///
/// A character with no glyph becomes a space rather than a box: the
/// grammar a repository name is checked against admits nothing outside
/// this table, so reaching the fallback means the name did not come
/// from that grammar and the card should say less, not more.
fn glyph(c: char) -> [u8; GH] {
    let c = c.to_ascii_uppercase();
    FONT.iter()
        .find(|(g, _)| *g == c)
        .map_or(FONT[0].1, |(_, rows)| *rows)
}

/// A one-bit canvas, indexed by the palette above.
struct Canvas {
    bits: Vec<u8>,
}

impl Canvas {
    fn new() -> Self {
        Self {
            bits: vec![0; STRIDE * HEIGHT],
        }
    }

    fn set(&mut self, x: usize, y: usize) {
        if x >= WIDTH || y >= HEIGHT {
            return;
        }
        self.bits[y * STRIDE + x / 8] |= 0x80 >> (x % 8);
    }

    fn rect(&mut self, x: usize, y: usize, w: usize, h: usize) {
        for dy in 0..h {
            for dx in 0..w {
                self.set(x + dx, y + dy);
            }
        }
    }

    /// Draws `text` with its top-left at `(x, y)`, each source pixel
    /// becoming a `scale`-sized square.
    fn text(&mut self, text: &str, x: usize, y: usize, scale: usize) {
        for (i, c) in text.chars().enumerate() {
            let rows = glyph(c);
            let ox = x + i * (GW + 1) * scale;
            for (ry, row) in rows.iter().enumerate() {
                for rx in 0..GW {
                    if row & (0x10 >> rx) != 0 {
                        self.rect(ox + rx * scale, y + ry * scale, scale, scale);
                    }
                }
            }
        }
    }
}

/// Width in pixels of `text` at `scale`, with no trailing gap.
fn text_width(text: &str, scale: usize) -> usize {
    let n = text.chars().count();
    if n == 0 {
        0
    } else {
        n * (GW + 1) * scale - scale
    }
}

/// The largest scale at which `text` fits inside `room`, floored at 1 so
/// an absurdly long name renders small rather than not at all.
fn fitting_scale(text: &str, room: usize, max: usize) -> usize {
    (1..=max)
        .rev()
        .find(|scale| text_width(text, *scale) <= room)
        .unwrap_or(1)
}

/// The card for `name`, as PNG bytes.
///
/// `name` is drawn as given, upper-cased by the font. Nothing here reads
/// the repository, the ACL or the disk: the image is a function of the
/// string, which is what lets the route serve it without checking
/// whether the repository exists. A card is not a disclosure if it
/// only ever repeats its own URL back.
#[must_use]
pub(crate) fn png(name: &str) -> Vec<u8> {
    let mut canvas = Canvas::new();

    // The name, as large as it fits across the middle, with a rule under
    // it and the node's own word below that. Three elements: what this
    // is, a line, and whose surface it is on.
    let margin = 100;
    let room = WIDTH - margin * 2;
    let scale = fitting_scale(name, room, 16);
    let tag = "A CHOIR NODE";
    let tag_scale = 4;

    // The three elements are centred as one block, not individually.
    // Centring the name alone and hanging the rest under it put the
    // whole composition low by about the height of the tag, which on a
    // short name left a third of the card empty at the bottom and read
    // as a cropping mistake.
    const RULE: usize = 4;
    const GAP: usize = 40;
    let block = GH * scale + GAP + RULE + GAP + GH * tag_scale;
    let top = (HEIGHT - block) / 2;

    let w = text_width(name, scale);
    let x = (WIDTH - w) / 2;
    canvas.text(name, x, top, scale);

    let rule_y = top + GH * scale + GAP;
    canvas.rect(x, rule_y, w.max(200), RULE);

    let tw = text_width(tag, tag_scale);
    canvas.text(tag, (WIDTH - tw) / 2, rule_y + RULE + GAP, tag_scale);

    encode(&canvas)
}

/// Wraps the canvas in the smallest PNG that can carry it: a header, a
/// two-entry palette, one image chunk and an end marker.
fn encode(canvas: &Canvas) -> Vec<u8> {
    let mut out = Vec::with_capacity(STRIDE * HEIGHT + 1024);
    out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&u32::try_from(WIDTH).unwrap_or(u32::MAX).to_be_bytes());
    ihdr.extend_from_slice(&u32::try_from(HEIGHT).unwrap_or(u32::MAX).to_be_bytes());
    // One bit per pixel, colour type 3 (palette), no compression method
    // but the one, no filtering, no interlace.
    ihdr.extend_from_slice(&[1, 3, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);

    let mut plte = Vec::with_capacity(6);
    plte.extend_from_slice(&GROUND);
    plte.extend_from_slice(&INK);
    chunk(&mut out, b"PLTE", &plte);

    // Every scanline carries its filter byte, and the filter is None:
    // there is nothing for a predictor to earn on two-colour data that
    // is not being compressed afterwards.
    let mut raw = Vec::with_capacity((STRIDE + 1) * HEIGHT);
    for row in canvas.bits.chunks(STRIDE) {
        raw.push(0);
        raw.extend_from_slice(row);
    }
    chunk(&mut out, b"IDAT", &zlib_stored(&raw));
    chunk(&mut out, b"IEND", &[]);
    out
}

/// One PNG chunk: length, type, payload, and the CRC over the last two.
fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&u32::try_from(data.len()).unwrap_or(u32::MAX).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc = Vec::with_capacity(4 + data.len());
    crc.extend_from_slice(kind);
    crc.extend_from_slice(data);
    out.extend_from_slice(&crc32(&crc).to_be_bytes());
}

/// A zlib stream carrying `data` in stored deflate blocks.
///
/// Deflate's stored block is the escape hatch the format keeps for data
/// that does not compress, and using it deliberately is what lets this
/// file contain no compressor. The cost is the whole of it: 5 bytes of
/// framing per 65535, which on a card is 8 bytes.
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 64);
    // CM=8 (deflate), CINFO=7 (32K window), and a check byte that makes
    // the two-byte header a multiple of 31.
    out.extend_from_slice(&[0x78, 0x01]);
    let mut chunks = data.chunks(0xffff).peekable();
    while let Some(block) = chunks.next() {
        let final_block = u8::from(chunks.peek().is_none());
        out.push(final_block);
        let len = u16::try_from(block.len()).unwrap_or(u16::MAX);
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(block);
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

/// CRC-32 as PNG specifies it, computed without a table: the card is
/// encoded once per request and the table would be the only state in
/// this module.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffff_u32;
    for byte in data {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// Adler-32, which is what zlib checks the stream with.
fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1_u32, 0_u32);
    for byte in data {
        a = (a + u32::from(*byte)) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bytes have to be a PNG to anything that reads PNGs, and the
    /// parts a decoder rejects on are the framing rather than the
    /// picture: a wrong length, a wrong CRC or a wrong zlib check byte
    /// all render as "broken image" in the one place this is ever seen.
    #[test]
    fn the_card_is_a_png_a_decoder_would_accept() {
        let png = png("choir/choir");
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);

        // Walk the chunks the way a decoder does, checking each CRC.
        let mut at = 8;
        let mut kinds = Vec::new();
        while at < png.len() {
            let len = u32::from_be_bytes(png[at..at + 4].try_into().expect("4 bytes")) as usize;
            let kind = &png[at + 4..at + 8];
            let body = &png[at + 8..at + 8 + len];
            let want = u32::from_be_bytes(
                png[at + 8 + len..at + 12 + len]
                    .try_into()
                    .expect("4 bytes"),
            );
            let mut over = Vec::from(kind);
            over.extend_from_slice(body);
            assert_eq!(
                crc32(&over),
                want,
                "chunk {} carries a bad CRC",
                String::from_utf8_lossy(kind)
            );
            kinds.push(String::from_utf8_lossy(kind).into_owned());
            at += 12 + len;
        }
        assert_eq!(at, png.len(), "the last chunk did not end the file");
        assert_eq!(kinds, ["IHDR", "PLTE", "IDAT", "IEND"]);
    }

    /// The zlib header has a check constraint that is easy to get wrong
    /// and invisible until a decoder refuses the stream.
    #[test]
    fn the_zlib_header_passes_its_own_check() {
        let stream = zlib_stored(b"anything at all");
        assert_eq!(stream[0], 0x78);
        assert_eq!(
            (u32::from(stream[0]) * 256 + u32::from(stream[1])) % 31,
            0,
            "the two header bytes are not a multiple of 31"
        );
        // A stored block's length and its complement must agree, which
        // is the other thing a decoder checks before copying anything.
        let len = u16::from_le_bytes([stream[3], stream[4]]);
        let nlen = u16::from_le_bytes([stream[5], stream[6]]);
        assert_eq!(len, !nlen);
        assert_eq!(len as usize, b"anything at all".len());
    }

    /// Known answers, so an edit to either checksum is caught here
    /// rather than by a broken image in somebody's chat window.
    #[test]
    fn the_checksums_are_the_standard_ones() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(adler32(b"Wikipedia"), 0x11e6_0398);
    }

    /// A name that cannot fit at full size must shrink rather than run
    /// off the card, and the shrink has a floor.
    #[test]
    fn a_long_name_is_scaled_down_rather_than_cropped() {
        let short = fitting_scale("ab/cd", WIDTH - 200, 16);
        let long = fitting_scale(
            "an-extremely-long-owner/an-extremely-long-repository-name",
            WIDTH - 200,
            16,
        );
        assert_eq!(short, 16, "a short name did not use the full size");
        assert!(long < short, "a long name was not scaled down");
        assert!(long >= 1, "a long name scaled to nothing");
        assert!(
            text_width(
                "an-extremely-long-owner/an-extremely-long-repository-name",
                long
            ) <= WIDTH - 200,
            "the scaled name still overruns the card"
        );
    }

    /// Every character a repository name may contain has a glyph, so a
    /// legal name never renders with holes in it.
    #[test]
    fn the_font_covers_every_character_a_name_may_use() {
        for c in ('a'..='z')
            .chain('A'..='Z')
            .chain('0'..='9')
            .chain(['.', '-', '_', '/'])
        {
            assert_ne!(
                glyph(c),
                FONT[0].1,
                "{c} has no glyph and would render as a space"
            );
        }
    }
}
