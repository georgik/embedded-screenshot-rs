//! Decode an `esp_lcd_screenshot` serial dump into a PNG/PPM image.
//!
//! Reads a captured serial log (file, or stdin when the path is `-`),
//! extracts the base64 framebuffer between the FRAMEBUFFER_BEGIN /
//! FRAMEBUFFER_END markers, converts the pixels to RGB888, and writes
//! a PNG plus a binary PPM.
//!
//! Protocol reference: Espressif `esp_lcd_screenshot` component.
//! Fourcc `RGBL` = RGB565 little-endian, 2 bytes per pixel.
//!
//! Usage:
//! ```text
//! screenshot-decoder serial.log --output screenshot
//! espflash monitor | screenshot-decoder - --output screenshot
//! ```

use std::io::Read;

/// Bytes per pixel per fourcc, as in the component's pytest fixture.
fn fourcc_bytes_per_pixel(fourcc: &str) -> Option<usize> {
    match fourcc {
        "RGBL" | "RGBE" => Some(2), // RGB565 little/big endian
        "RGB3" | "BGR3" => Some(3), // RGB888 / BGR888
        "BA24" => Some(4),          // BGRA8888
        _ => None,
    }
}

/// Only used by the roundtrip test to emit logs the same way the
/// firmware does.
#[cfg(test)]
const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_decode_char(c: u8) -> Option<u32> {
    match c {
        b'A'..=b'Z' => Some((c - b'A') as u32),
        b'a'..=b'z' => Some((c - b'a' + 26) as u32),
        b'0'..=b'9' => Some((c - b'0' + 52) as u32),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// Strict base64 decoder: rejects unpadded input, matching the
/// `validate=True` behavior of the component's pytest fixture.
fn base64_decode(input: &str) -> Option<Vec<u8>> {
    let bytes = input.trim_end().as_bytes();
    if bytes.is_empty() || !bytes.len().is_multiple_of(4) {
        return None;
    }
    let padding = bytes.iter().rev().take_while(|&&c| c == b'=').count();
    if padding > 2 || bytes[..bytes.len() - padding].contains(&b'=') {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for group in bytes.chunks(4) {
        // Positional 24-bit word; padding chars contribute zero bits
        // and only the bytes covered by real data get pushed below.
        let mut c = [0u32; 4];
        for (slot, &ch) in c.iter_mut().zip(group) {
            if ch != b'=' {
                *slot = base64_decode_char(ch)?;
            }
        }
        let n = (c[0] << 18) | (c[1] << 12) | (c[2] << 6) | c[3];
        out.push((n >> 16) as u8);
        if group[2] != b'=' {
            out.push((n >> 8) as u8);
        }
        if group[3] != b'=' {
            out.push(n as u8);
        }
    }
    Some(out)
}

#[derive(Clone)]
struct FrameMetadata {
    width: u32,
    height: u32,
    fourcc: String,
}

/// Scan log lines for a FRAMEBUFFER_BEGIN ... FRAMEBUFFER_END block,
/// ignoring any unrelated serial noise around it.
fn extract_payload(lines: &[String]) -> Result<(FrameMetadata, Vec<u8>), String> {
    let mut metadata: Option<FrameMetadata> = None;
    let mut payload = String::new();
    for line in lines {
        let line = line.trim();
        if let Some(meta) = &metadata {
            if line == "FRAMEBUFFER_END" {
                let raw =
                    base64_decode(&payload).ok_or_else(|| "Invalid base64 payload".to_string())?;
                let bytes_pp = fourcc_bytes_per_pixel(&meta.fourcc)
                    .ok_or_else(|| format!("Unsupported fourcc: {}", meta.fourcc))?;
                let expected = meta.width as usize * meta.height as usize * bytes_pp;
                if raw.len() != expected {
                    return Err(format!(
                        "Expected {expected} decoded bytes, got {}",
                        raw.len()
                    ));
                }
                return Ok((meta.clone(), raw));
            }
            if let Some(rest) = line.strip_prefix("FB_BASE64 ") {
                payload.push_str(rest.trim_end());
            }
        } else if let Some(rest) = line.strip_prefix("FRAMEBUFFER_BEGIN ") {
            let parts: Vec<&str> = rest.split_whitespace().collect();
            if parts.len() != 3 {
                return Err(format!("Invalid framebuffer metadata line: {line}"));
            }
            metadata = Some(FrameMetadata {
                width: parts[0]
                    .parse()
                    .map_err(|_| format!("Invalid width: {}", parts[0]))?,
                height: parts[1]
                    .parse()
                    .map_err(|_| format!("Invalid height: {}", parts[1]))?,
                fourcc: parts[2].to_string(),
            });
        }
    }
    Err("No complete FRAMEBUFFER_BEGIN...FRAMEBUFFER_END block in input".to_string())
}

/// Convert raw framebuffer bytes to RGB888.
fn to_rgb888(fourcc: &str, raw: &[u8]) -> Result<Vec<u8>, String> {
    match fourcc {
        "RGBL" => {
            let mut out = Vec::with_capacity(raw.len() / 2 * 3);
            for px in raw.as_chunks::<2>().0 {
                let value = u16::from_le_bytes([px[0], px[1]]);
                out.push((((value >> 11) & 0x1F) << 3) as u8);
                out.push((((value >> 5) & 0x3F) << 2) as u8);
                out.push(((value & 0x1F) << 3) as u8);
            }
            Ok(out)
        }
        "RGB3" => Ok(raw.to_vec()),
        "BGR3" => {
            let mut out = raw.to_vec();
            for px in out.as_chunks_mut::<3>().0 {
                px.swap(0, 2);
            }
            Ok(out)
        }
        other => Err(format!("Unsupported fourcc: {other}")),
    }
}

fn write_ppm(path: &str, width: u32, height: u32, rgb888: &[u8]) -> std::io::Result<()> {
    std::fs::write(
        path,
        format!("P6\n{width} {height}\n255\n")
            .into_bytes()
            .iter()
            .copied()
            .chain(rgb888.iter().copied())
            .collect::<Vec<u8>>(),
    )
}

fn main() {
    let mut args = std::env::args().skip(1);
    let input_path = args.next().unwrap_or_else(|| {
        eprintln!("Usage: screenshot-decoder <serial.log|-> [--output <prefix>]");
        std::process::exit(2);
    });
    let output_prefix = match (args.next().as_deref(), args.next().as_deref()) {
        (Some("--output"), Some(prefix)) => prefix.to_string(),
        _ => "screenshot".to_string(),
    };

    let mut input = String::new();
    if input_path == "-" {
        std::io::stdin()
            .read_to_string(&mut input)
            .unwrap_or_else(|err| {
                eprintln!("Failed to read stdin: {err}");
                std::process::exit(1);
            });
    } else {
        std::fs::File::open(&input_path)
            .and_then(|mut f| f.read_to_string(&mut input))
            .unwrap_or_else(|err| {
                eprintln!("Failed to read {input_path}: {err}");
                std::process::exit(1);
            });
    }
    // Lossy read is enough for ASCII protocol lines; drop invalid bytes.
    let lines: Vec<String> = String::from_utf8_lossy(input.as_bytes())
        .lines()
        .map(ToString::to_string)
        .collect();

    let (metadata, raw) = match extract_payload(&lines) {
        Ok(result) => result,
        Err(err) => {
            eprintln!("Error: {err}");
            std::process::exit(1);
        }
    };

    let rgb888 = to_rgb888(&metadata.fourcc, &raw).unwrap_or_else(|err| {
        eprintln!("Error: {err}");
        std::process::exit(1);
    });

    let (width, height) = (metadata.width, metadata.height);
    image::save_buffer(
        format!("{output_prefix}.png"),
        &rgb888,
        width,
        height,
        image::ColorType::Rgb8,
    )
    .unwrap_or_else(|err| eprintln!("Failed to write {output_prefix}.png: {err}"));
    println!("Wrote {output_prefix}.png");

    let ppm_path = format!("{output_prefix}.ppm");
    write_ppm(&ppm_path, width, height, &rgb888)
        .unwrap_or_else(|err| eprintln!("Failed to write {ppm_path}: {err}"));
    println!("Wrote {ppm_path}");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip: build a fake serial log the same way the firmware
    /// emits it (RGBL, 765-pixel payload lines) and decode it back.
    #[test]
    fn roundtrip_rgb565le() {
        let width: u32 = 320;
        let height: u32 = 240;
        let raw: Vec<u8> = (0..(width as usize * height as usize))
            .map(|i| (i % 65536) as u16)
            .flat_map(u16::to_le_bytes)
            .collect();

        // Encode with the same chunking as the firmware side.
        const PIXELS_PER_CHUNK: usize = 765;
        let mut log = vec!["FRAMEBUFFER_BEGIN 320 240 RGBL".to_string()];
        for chunk in raw.chunks(PIXELS_PER_CHUNK * 2) {
            let mut line = String::from("FB_BASE64 ");
            for group in chunk.chunks(3) {
                let b0 = group[0] as u32;
                let b1 = group.get(1).copied().unwrap_or(0) as u32;
                let b2 = group.get(2).copied().unwrap_or(0) as u32;
                let n = (b0 << 16) | (b1 << 8) | b2;
                line.push(BASE64_ALPHABET[(n >> 18) as usize & 63] as char);
                line.push(BASE64_ALPHABET[(n >> 12) as usize & 63] as char);
                line.push(if group.len() > 1 {
                    BASE64_ALPHABET[(n >> 6) as usize & 63] as char
                } else {
                    '='
                });
                line.push(if group.len() > 2 {
                    BASE64_ALPHABET[n as usize & 63] as char
                } else {
                    '='
                });
            }
            log.push(line);
        }
        log.push("FRAMEBUFFER_END".to_string());
        log.push("I (500) trailing log noise".to_string());

        let (metadata, decoded) = extract_payload(&log).expect("payload extraction failed");
        assert_eq!((metadata.width, metadata.height), (width, height));
        assert_eq!(metadata.fourcc, "RGBL");
        assert_eq!(decoded, raw);

        let rgb888 = to_rgb888("RGBL", &decoded).unwrap();
        assert_eq!(rgb888.len(), width as usize * height as usize * 3);
        // Pixel 0 -> u16 0 -> RGB565 black expanded to RGB888 zeros.
        assert_eq!(&rgb888[..3], &[0, 0, 0]);
    }

    #[test]
    fn base64_rejects_unpadded_and_bad_input() {
        assert!(base64_decode("AAAA").is_some());
        assert!(base64_decode("AAAAA").is_none()); // length not multiple of 4
        assert!(base64_decode("A===").is_none()); // invalid padding layout
        assert!(base64_decode("AA A").is_none()); // invalid character
    }

    #[test]
    fn base64_known_vectors() {
        // "foobar" classic vector, split across chunks with padding.
        assert_eq!(base64_decode("Zm9vYmFy").unwrap(), b"foobar");
        assert_eq!(base64_decode("Zm9vYg==").unwrap(), b"foob");
        assert_eq!(base64_decode("Zm9vYw==").unwrap(), b"fooc".to_vec());
        assert_eq!(base64_decode("Zm9v").unwrap(), b"foo");
        // Any base64 alphabet character may appear in a padded group.
        assert_eq!(base64_decode("/w==").unwrap(), vec![0xFF]);
        assert_eq!(base64_decode("//8=").unwrap(), vec![0xFF, 0xFF]);
    }

    #[test]
    fn rgb565le_to_rgb888_known_colors() {
        // 0xFFFF (RGB565 white) expands to (248, 252, 248), not pure white.
        assert_eq!(
            to_rgb888("RGBL", &[0xFF, 0xFF]).unwrap(),
            vec![248, 252, 248]
        );
        // 0x0000 black stays black.
        assert_eq!(to_rgb888("RGBL", &[0x00, 0x00]).unwrap(), vec![0, 0, 0]);
        // Pure red: 0xF800 -> (248, 0, 0).
        assert_eq!(to_rgb888("RGBL", &[0x00, 0xF8]).unwrap(), vec![248, 0, 0]);
    }

    #[test]
    fn parser_ignores_noise_and_requires_end_marker() {
        let noise = vec![
            "I (100) boot:".to_string(),
            "FRAMEBUFFER_BEGIN 2 1 RGBL".to_string(),
            "FB_BASE64 ////".to_string(),
        ];
        assert!(extract_payload(&noise).is_err()); // no FRAMEBUFFER_END

        let complete = vec![
            "I (100) boot:".to_string(),
            "FRAMEBUFFER_BEGIN 3 1 RGBL".to_string(),
            "FB_BASE64 AAAA".to_string(),
            "FB_BASE64 AAAA".to_string(),
            "FRAMEBUFFER_END".to_string(),
        ];
        let (meta, raw) = extract_payload(&complete).unwrap();
        assert_eq!((meta.width, meta.height), (3, 1));
        assert_eq!(raw.len(), 6); // 3 px * 2 B
    }
}
