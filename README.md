# esp-screenshot

FrameBuffer screenshot dumping over the serial console, compatible with the
Espressif [`esp_lcd_screenshot`](https://components.espressif.com/components/espressif/esp_lcd_screenshot)
component. No camera, no golden files on the device — the framebuffer already
in RAM is dumped as text:

```text
FRAMEBUFFER_BEGIN 320 240 RGBL
FB_BASE64 <base64 payload chunk>
...
FRAMEBUFFER_END
```

`RGBL` = RGB565 little-endian, 2 bytes per pixel. Because the payload is
plain text, any host-side tool written for `esp_lcd_screenshot` can decode
it unchanged.

## Layout

| Directory | Target | Purpose |
|-----------|--------|---------|
| `embedded/` | `no_std` (core only) | Library crate linked into board applications |
| `tool/` | host, std | `screenshot-decoder`: serial log → PNG/PPM |

## Using the library in a board crate

Add the dependency and a feature gate to the board's `Cargo.toml`:

```toml
[dependencies]
esp-screenshot = { path = "../esp-screenshot/embedded", optional = true, features = ["embedded-graphics"] }

[features]
screenshot = ["dep:esp-screenshot"]
```

In `main.rs`, dump the framebuffer after it has been rendered:

```rust
#[cfg(feature = "screenshot")]
struct SerialWriter;

#[cfg(feature = "screenshot")]
impl core::fmt::Write for SerialWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        esp_println::print!("{s}");
        Ok(())
    }
}

fn render(...) {
    // ... render the framebuffer and flush it to the display ...

    #[cfg(feature = "screenshot")]
    {
        let pixel_bytes: alloc::vec::Vec<u8> =
            esp_screenshot::rgb565_le_bytes(frame_buf.data.as_ref()).collect();
        esp_screenshot::write_framebuffer(
            &mut SerialWriter,
            320, 240,
            esp_screenshot::RGBL,
            &pixel_bytes,
        ).ok();
    }
}
```

Build with `cargo b -r --features screenshot`. Without the feature, the
production binary keeps its minimal footprint.

## Capturing and decoding

Flash and monitor the board, capture the serial output, then decode:

```bash
cd boards/<board> && cargo run -r --features screenshot 2>&1 | tee /tmp/serial.log
# Ctrl+C once the dump has been emitted, then:
../esp-screenshot/tool/target/release/screenshot-decoder /tmp/serial.log --output screenshot
```

Outputs `screenshot.png` and `screenshot.ppm`.

The decoder also accepts stdin (`screenshot-decoder -`) and tolerates
unrelated log noise around the `FRAMEBUFFER_BEGIN`/`FRAMEBUFFER_END` block.

## Notes

- The library is allocator-free: base64 encoding happens in fixed stack
  buffers (2040 characters per line).
- A full 320×240 RGB565 frame is ~150 KB, which takes ~18 s at 115200 baud —
  dump once per boot or raise the UART baud rate.
- Framebuffer content with RNG-seeded scenes is not deterministic; for
  golden-image tests, seed the scene from a constant.
