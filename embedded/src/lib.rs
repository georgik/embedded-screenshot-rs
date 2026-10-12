//! Dump a framebuffer in the format of the
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
//! The crate is sink-agnostic: [`write_framebuffer`] is generic over
//! [`embedded_io::Write`], so the framebuffer can go to a UART, a USB serial
//! JTAG port, a TCP socket or anywhere else the ecosystem can write bytes to.
//!
//! With `esp-println`, enable the `esp-println` feature and use the built-in
//! [`ConsolePrinter`] sink — no shim required:
//!
//! ```ignore
//! // Rgb565 framebuffer stored as little-endian u16 pixels:
//! let pixel_bytes: alloc::vec::Vec<u8> =
//!     esp_screenshot::rgb565_le_bytes(&frame_buf.data).collect();
//! esp_screenshot::write_framebuffer(
//!     &mut esp_screenshot::ConsolePrinter,
//!     320,
//!     240,
//!     esp_screenshot::RGBL,
//!     &pixel_bytes,
//! )
//! .ok();
//! ```
//!
//! Any other [`embedded_io::Write`] sink works just as well, e.g. a UART:
//!
//! ```ignore
//! let mut uart = Uart::new(peripherals.UART1, Config::default())?;
//! esp_screenshot::write_framebuffer(&mut uart, 320, 240, RGBL, &pixel_bytes)?;
//! ```
//!
//! # Blocking vs async
//!
//! [`write_framebuffer`] works in any app, including plain super-loop
//! firmware without an executor. For executor-driven apps (embassy & co),
//! enable the `async` feature and use [`write_framebuffer_async`] instead;
//! both share the same encoder core.
#![no_std]
// Tests run on the host and need std collections.
#[cfg(test)]
extern crate std;

use embedded_io::Write as IoWrite;

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

/// Longest protocol line: the `FB_BASE64 ` prefix, the base64 payload and
/// the CRLF terminator.
const LINE_BUF_LEN: usize = "FB_BASE64 ".len() + (BYTES_PER_CHUNK / 3) * 4 + 2;

/// Encodes the framebuffer into protocol lines, one at a time, into a
/// caller-provided stack buffer. Shared by the blocking and async writers.
struct LineEncoder<'a> {
    width: usize,
    height: usize,
    fourcc: &'a str,
    remaining: &'a [u8],
    header_pending: bool,
    end_pending: bool,
}

impl<'a> LineEncoder<'a> {
    fn new(width: usize, height: usize, fourcc: &'a str, pixel_bytes: &'a [u8]) -> Self {
        Self {
            width,
            height,
            fourcc,
            remaining: pixel_bytes,
            header_pending: true,
            end_pending: true,
        }
    }

    /// Writes the next protocol line into `buf` and returns the emitted
    /// slice, or `None` once the frame is complete.
    fn next_line<'b>(&mut self, buf: &'b mut [u8; LINE_BUF_LEN]) -> Option<&'b [u8]> {
        if self.header_pending {
            self.header_pending = false;
            let mut len = 0;
            len += push_literal(buf, b"FRAMEBUFFER_BEGIN ");
            len += push_number(buf, len, self.width);
            buf[len] = b' ';
            len += 1;
            len += push_number(buf, len, self.height);
            buf[len] = b' ';
            len += 1;
            buf[len..len + self.fourcc.len()].copy_from_slice(self.fourcc.as_bytes());
            len += self.fourcc.len();
            len += push_literal(&mut buf[len..], b"\r\n");
            return Some(&buf[..len]);
        }

        if self.remaining.is_empty() {
            if self.end_pending {
                self.end_pending = false;
                let len = push_literal(buf, b"FRAMEBUFFER_END\r\n");
                return Some(&buf[..len]);
            }
            return None;
        }
        let split = self.remaining.len().min(BYTES_PER_CHUNK);
        let (chunk, rest) = self.remaining.split_at(split);
        self.remaining = rest;

        let (groups, remainder) = chunk.as_chunks::<3>();
        let mut len = push_literal(buf, b"FB_BASE64 ");
        for (src, dst) in groups.iter().zip(buf[len..].as_chunks_mut::<4>().0) {
            let n = ((src[0] as u32) << 16) | ((src[1] as u32) << 8) | src[2] as u32;
            dst[0] = BASE64_ALPHABET[(n >> 18) as usize & 63];
            dst[1] = BASE64_ALPHABET[(n >> 12) as usize & 63];
            dst[2] = BASE64_ALPHABET[(n >> 6) as usize & 63];
            dst[3] = BASE64_ALPHABET[n as usize & 63];
            len += 4;
        }
        // Pad the final partial group of the last chunk.
        if !remainder.is_empty() {
            let b0 = remainder[0] as u32;
            let b1 = remainder.get(1).copied().unwrap_or(0) as u32;
            let n = (b0 << 16) | (b1 << 8);
            buf[len] = BASE64_ALPHABET[(n >> 18) as usize & 63];
            buf[len + 1] = BASE64_ALPHABET[(n >> 12) as usize & 63];
            buf[len + 2] = if remainder.len() > 1 {
                BASE64_ALPHABET[(n >> 6) as usize & 63]
            } else {
                b'='
            };
            buf[len + 3] = b'=';
            len += 4;
        }
        len += push_literal(&mut buf[len..], b"\r\n");
        Some(&buf[..len])
    }
}

/// Copies `literal` to the front of `buf`, returning its length.
fn push_literal(buf: &mut [u8], literal: &[u8]) -> usize {
    buf[..literal.len()].copy_from_slice(literal);
    literal.len()
}

/// Writes the decimal digits of `n` at `buf[offset..]`, returning the number
/// of digits written.
fn push_number(buf: &mut [u8], offset: usize, n: usize) -> usize {
    let mut digits = [0u8; 20];
    let mut count = 0;
    let mut n = n;
    loop {
        digits[count] = b'0' + (n % 10) as u8;
        n /= 10;
        count += 1;
        if n == 0 {
            break;
        }
    }
    for (i, digit) in digits[..count].iter().rev().enumerate() {
        buf[offset + i] = *digit;
    }
    count
}

/// Writes the framebuffer between `FRAMEBUFFER_BEGIN` / `FRAMEBUFFER_END`
/// markers, base64-encoded in fixed-size chunks. Blocking: works in any app,
/// including plain super-loop firmware.
///
/// `pixel_bytes` must contain exactly `width * height * bpp` bytes for the
/// given FourCC (e.g. 2 bytes per pixel for [`RGBL`]). The chunk size is
/// arbitrary — the host-side parser reassembles the payload from all
/// `FB_BASE64` lines.
pub fn write_framebuffer<W: IoWrite>(
    out: &mut W,
    width: usize,
    height: usize,
    fourcc: &str,
    pixel_bytes: &[u8],
) -> Result<(), W::Error> {
    let mut encoder = LineEncoder::new(width, height, fourcc, pixel_bytes);
    let mut buf = [0u8; LINE_BUF_LEN];
    while let Some(line) = encoder.next_line(&mut buf) {
        out.write_all(line)?;
    }
    Ok(())
}

/// Async counterpart of [`write_framebuffer`] for executor-driven apps.
#[cfg(feature = "async")]
pub async fn write_framebuffer_async<W: embedded_io_async::Write>(
    out: &mut W,
    width: usize,
    height: usize,
    fourcc: &str,
    pixel_bytes: &[u8],
) -> Result<(), W::Error> {
    let mut encoder = LineEncoder::new(width, height, fourcc, pixel_bytes);
    let mut buf = [0u8; LINE_BUF_LEN];
    while let Some(line) = encoder.next_line(&mut buf) {
        out.write_all(line).await?;
    }
    Ok(())
}

/// Ready-made [`embedded_io::Write`] sink that forwards bytes to the
/// `esp-println` console, which auto-selects between UART and USB serial
/// JTAG on most chips. Requires the `esp-println` feature.
///
/// Note: `esp-println` must be configured with a console backend (its
/// `auto`, `uart` or `jtag-serial` feature) — which is already the case in
/// apps that use its `print!`/`println!` macros.
#[cfg(feature = "esp-println")]
pub struct ConsolePrinter;

#[cfg(feature = "esp-println")]
impl embedded_io::ErrorType for ConsolePrinter {
    type Error = core::convert::Infallible;
}

#[cfg(feature = "esp-println")]
impl IoWrite for ConsolePrinter {
    fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        esp_println::Printer::write_bytes(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        esp_println::Printer::write_bytes(&[]);
        Ok(())
    }
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
    use embedded_io::ErrorType;
    use std::string::String;
    use std::vec;
    use std::vec::Vec;

    /// Collects what the writer produced into a byte buffer.
    struct Sink(Vec<u8>);

    impl ErrorType for Sink {
        type Error = core::convert::Infallible;
    }

    impl IoWrite for Sink {
        fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
            self.0.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    /// Minimal `block_on` so the async writer can be tested without a
    /// dependency on an executor.
    fn block_on<F: Future>(fut: F) -> F::Output {
        use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

        fn noop(_: *const ()) {}
        fn clone(p: *const ()) -> RawWaker {
            RawWaker::new(p, &VTABLE)
        }
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);

        let waker = unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), &VTABLE)) };
        let mut cx = Context::from_waker(&waker);
        let mut fut = std::pin::pin!(fut);
        loop {
            match fut.as_mut().poll(&mut cx) {
                Poll::Ready(value) => return value,
                // The test sink never yields Pending; spinning keeps the
                // helper executor-agnostic.
                Poll::Pending => std::hint::spin_loop(),
            }
        }
    }

    #[cfg(feature = "async")]
    impl embedded_io_async::Write for Sink {
        async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
            IoWrite::write(self, buf)
        }

        async fn flush(&mut self) -> Result<(), Self::Error> {
            IoWrite::flush(self)
        }
    }

    fn dump(width: usize, height: usize, pixel_bytes: &[u8]) -> String {
        let mut sink = Sink(Vec::new());
        write_framebuffer(&mut sink, width, height, RGBL, pixel_bytes).unwrap();
        String::from_utf8(sink.0).unwrap()
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

    #[cfg(feature = "async")]
    #[test]
    fn async_writer_matches_blocking_writer() {
        let raw: Vec<u8> = (0..300u16).flat_map(|i| i.to_le_bytes()).collect();

        let mut sync_sink = Sink(Vec::new());
        write_framebuffer(&mut sync_sink, 150, 1, RGBL, &raw).unwrap();

        let mut async_sink = Sink(Vec::new());
        block_on(write_framebuffer_async(&mut async_sink, 150, 1, RGBL, &raw)).unwrap();

        assert_eq!(sync_sink.0, async_sink.0);
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
                        *slot = BASE64_ALPHABET.iter().position(|&a| a == ch).unwrap() as u32;
                    }
                }
                let n = (c[0] << 18) | (c[1] << 12) | (c[2] << 6) | c[3];
                let len = group.iter().filter(|&&ch| ch != b'=').count();
                (0..len.saturating_sub(1)).map(move |i| ((n >> (16 - 8 * i)) & 0xFF) as u8)
            })
            .collect();
        assert_eq!(decoded, raw);
    }

    #[test]
    fn base64_padding_edge_cases() {
        // 1 byte -> "AA==", 2 bytes -> "AAA=", 3 bytes -> no padding.
        assert_eq!(payload_lines(dump(1, 1, &[0x00])), ["AA=="]);
        assert_eq!(payload_lines(dump(1, 1, &[0x00, 0x00])), ["AAA="]);
        assert_eq!(payload_lines(dump(1, 1, &[0x00, 0x00, 0x00])), ["AAAA"]);
    }

    #[test]
    fn empty_payload_emits_no_chunk_lines() {
        let output = dump(0, 0, &[]);
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines, ["FRAMEBUFFER_BEGIN 0 0 RGBL", "FRAMEBUFFER_END"]);
    }

    #[test]
    fn exact_chunk_multiple_is_a_single_line() {
        let raw: Vec<u8> = vec![0xA5u8; BYTES_PER_CHUNK];
        let lines = payload_lines(dump(100, 100, &raw));
        assert_eq!(lines.len(), 1);
        // 1530 bytes = 510 groups = 2040 chars, none of them padding.
        assert_eq!(lines[0].len(), 2040);
        assert!(!lines[0].contains('='));
    }

    #[test]
    fn multi_chunk_line_count_and_reassembly() {
        // Two full chunks plus 5 leftover bytes -> three payload lines.
        let raw: Vec<u8> = vec![0x3Cu8; BYTES_PER_CHUNK * 2 + 5];
        let lines = payload_lines(dump(10, 10, &raw));
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].len(), 2040);
        assert_eq!(lines[1].len(), 2040);
        // 5 bytes = 1 full group + 2 leftover bytes -> 8 chars incl. one '='.
        assert_eq!(lines[2].len(), 8);
        assert!(lines[2].ends_with('='));
        assert_eq!(lines[2].matches('=').count(), 1);
        assert_eq!(lines[2].len(), 8);
    }

    #[test]
    fn header_formats_large_dimensions() {
        let output = dump(123456, 7890, &[0; 0]);
        assert!(
            output.starts_with("FRAMEBUFFER_BEGIN 123456 7890 RGBL\r\n"),
            "unexpected header: {output}"
        );
    }

    /// Returns just the base64 payload of the `FB_BASE64` lines.
    fn payload_lines(output: String) -> Vec<String> {
        output
            .lines()
            .filter_map(|l| l.strip_prefix("FB_BASE64 "))
            .map(String::from)
            .collect()
    }

    #[cfg(feature = "embedded-graphics")]
    #[test]
    fn rgb565_le_bytes_produces_little_endian_pairs() {
        use embedded_graphics::pixelcolor::Rgb565;

        // 0xF800 (red) -> storage 0xF800 -> LE bytes [0x00, 0xF8].
        // 0x001F (blue) -> LE bytes [0x1F, 0x00].
        let pixels = [Rgb565::new(31, 0, 0), Rgb565::new(0, 0, 31)];
        let bytes: Vec<u8> = rgb565_le_bytes(&pixels).collect();
        assert_eq!(bytes, [0x00, 0xF8, 0x1F, 0x00]);
    }
}
