//! Dump a framebuffer over the serial console in the format of the
//! Espressif [`esp_lcd_screenshot`](https://components.espressif.com/components/espressif/esp_lcd_screenshot)
//! component:
//!
//! ```text
//! FRAMEBUFFER_BEGIN <width> <height> <fourcc>
//! FB_BASE64 <base64 payload chunk>
//! ...
//! FRAMEBUFFER_END
//! ```
//!
//! The payload lines are plain text, so any host-side tool written for
//! `esp_lcd_screenshot` can decode them unchanged — including the
//! `screenshot-decoder` tool that lives next to this crate.
//!
//! # Usage
//!
//! The crate is chip-agnostic: bring your own `core::fmt::Write` sink.
//! With `esp-println`, a three-line shim is enough:
//!
//! ```ignore
//! struct SerialWriter;
//!
//! impl core::fmt::Write for SerialWriter {
//!     fn write_str(&mut self, s: &str) -> core::fmt::Result {
//!         esp_println::print!("{s}");
//!         Ok(())
//!     }
//! }
//!
//! // Rgb565 framebuffer stored as little-endian u16 pixels:
//! let pixel_bytes: alloc::vec::Vec<u8> =
//!     esp_screenshot::rgb565_le_bytes(&frame_buf.data).collect();
//! esp_screenshot::write_framebuffer(
//!     &mut SerialWriter,
//!     320,
//!     240,
//!     esp_screenshot::RGBL,
//!     &pixel_bytes,
//! )
//! .ok();
//! ```
#![no_std]
// Tests run on the host and need std collections.
#[cfg(test)]
extern crate std;

use core::fmt::{Result, Write};

/// FourCC for RGB565 little-endian (2 bytes per pixel), matching
/// `ESP_COLOR_FOURCC_RGB16` of the esp_lcd_screenshot component.
pub const RGBL: &str = "RGBL";

/// Base64 alphabet shared with the host-side decoder.
const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Payload chunk in framebuffer bytes per line: 1530 bytes = 510 base64
/// groups = 2040 base64 characters per line. All buffers live on the
/// stack, so dumping works without an allocator.
const BYTES_PER_CHUNK: usize = 1530;

/// Writes the framebuffer between `FRAMEBUFFER_BEGIN` / `FRAMEBUFFER_END`
/// markers, base64-encoded in fixed-size chunks.
///
/// `pixel_bytes` must contain exactly `width * height * bpp` bytes for the
/// given FourCC (e.g. 2 bytes per pixel for [`RGBL`]). The chunk size is
/// arbitrary — the host-side parser reassembles the payload from all
/// `FB_BASE64` lines.
pub fn write_framebuffer<W: Write>(
    out: &mut W,
    width: usize,
    height: usize,
    fourcc: &str,
    pixel_bytes: &[u8],
) -> Result {
    writeln!(out, "FRAMEBUFFER_BEGIN {} {} {}", width, height, fourcc)?;

    // (BYTES_PER_CHUNK / 3) * 4 base64 characters per line, stack-only.
    let mut encoded = [0u8; (BYTES_PER_CHUNK / 3) * 4];
    for chunk in pixel_bytes.chunks(BYTES_PER_CHUNK) {
        let groups = chunk.chunks_exact(3);
        let remainder = groups.remainder();
        let mut len = 0;
        for group in groups {
            let n = ((group[0] as u32) << 16) | ((group[1] as u32) << 8) | group[2] as u32;
            encoded[len] = BASE64_ALPHABET[(n >> 18) as usize & 63];
            encoded[len + 1] = BASE64_ALPHABET[(n >> 12) as usize & 63];
            encoded[len + 2] = BASE64_ALPHABET[(n >> 6) as usize & 63];
            encoded[len + 3] = BASE64_ALPHABET[n as usize & 63];
            len += 4;
        }
        // Pad the final partial group of the last chunk.
        if !remainder.is_empty() {
            let b0 = remainder[0] as u32;
            let b1 = remainder.get(1).copied().unwrap_or(0) as u32;
            let n = (b0 << 16) | (b1 << 8);
            encoded[len] = BASE64_ALPHABET[(n >> 18) as usize & 63];
            encoded[len + 1] = BASE64_ALPHABET[(n >> 12) as usize & 63];
            encoded[len + 2] = if remainder.len() > 1 {
                BASE64_ALPHABET[(n >> 6) as usize & 63]
            } else {
                b'='
            };
            encoded[len + 3] = b'=';
            len += 4;
        }
        out.write_str("FB_BASE64 ")?;
        out.write_str(core::str::from_utf8(&encoded[..len]).unwrap_or_default())?;
        out.write_str("\r\n")?;
    }

    writeln!(out, "FRAMEBUFFER_END")
}

/// Maps pixel colors to their little-endian `u16` bytes, for RGB565
/// framebuffers ([`RGBL`] FourCC).
#[cfg(feature = "embedded-graphics")]
pub fn rgb565_le_bytes<C>(pixels: &[C]) -> impl Iterator<Item = u8> + '_
where
    C: Copy + embedded_graphics::pixelcolor::IntoStorage<Storage = u16>,
{
    pixels
        .iter()
        .copied()
        .flat_map(|px| px.into_storage().to_le_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::string::String;
    use std::vec::Vec;

    /// Collects what the writer produced into a String.
    fn dump(width: usize, height: usize, pixel_bytes: &[u8]) -> String {
        struct Sink(String);
        impl Write for Sink {
            fn write_str(&mut self, s: &str) -> Result {
                self.0.push_str(s);
                Ok(())
            }
        }
        let mut sink = Sink(String::new());
        write_framebuffer(&mut sink, width, height, RGBL, pixel_bytes).unwrap();
        sink.0
    }

    #[test]
    fn frame_layout_matches_protocol() {
        let output = dump(2, 1, &[0x00, 0x00, 0xFF, 0xFF]);
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines[0], "FRAMEBUFFER_BEGIN 2 1 RGBL");
        // 4 bytes fit into a single payload line; the final partial
        // group is padded.
        assert_eq!(lines[1], "FB_BASE64 AAD//w==");
        assert_eq!(lines[2], "FRAMEBUFFER_END");
    }

    #[test]
    fn full_chunk_roundtrip_against_host_decoder() {
        // One full chunk plus a remainder exercises both code paths.
        let raw: std::vec::Vec<u8> = (0..(BYTES_PER_CHUNK + 4) as u16)
            .map(|i| i.to_le_bytes())
            .flatten()
            .collect();
        let output = dump(10, 10, &raw);
        let payload: String = output
            .lines()
            .filter_map(|l| l.strip_prefix("FB_BASE64 "))
            .collect();

        // Reference decoder: the host tool's algorithm.
        assert_eq!(payload.len() % 4, 0);
        let decoded: std::vec::Vec<u8> = payload
            .as_bytes()
            .chunks(4)
            .flat_map(|group| {
                let mut c = [0u32; 4];
                for (slot, &ch) in c.iter_mut().zip(group) {
                    if ch != b'=' {
                        *slot = BASE64_ALPHABET
                            .iter()
                            .position(|&a| a == ch)
                            .unwrap() as u32;
                    }
                }
                let n = (c[0] << 18) | (c[1] << 12) | (c[2] << 6) | c[3];
                let len = group.iter().filter(|&&ch| ch != b'=').count();
                (0..len.saturating_sub(1)).map(move |i| ((n >> (16 - 8 * i)) & 0xFF) as u8)
            })
            .collect();
        assert_eq!(decoded, raw);
    }
}
