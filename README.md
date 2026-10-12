# embedded-screenshot

FrameBuffer screenshot dumping over any `embedded_io::Write` byte sink
(serial console, USB, TCP, ...), compatible with the
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
| `tool/` | host, std | `embedded-screenshot-decoder` (crate `embedded-screenshot-decoder`): serial log → PNG/PPM |

## Used by

- [esp32-conways-game-of-life-rs](https://github.com/georgik/esp32-conways-game-of-life-rs) —
  Conway's Game of Life on ESP32-S3-BOX-3 (Bevy ECS, no_std), dumps the
  framebuffer over the serial console with the `screenshot` feature.

## Using the library in a board crate

Add the dependency and a feature gate to the board's `Cargo.toml`:

```toml
[dependencies]
embedded-screenshot = { path = "../embedded-screenshot-rs/embedded", optional = true, features = ["embedded-graphics", "esp-println"] }

[features]
screenshot = ["dep:embedded-screenshot"]
```

In `main.rs`, dump the framebuffer after it has been rendered. The writer is
generic over [`embedded_io::Write`](https://docs.rs/embedded-io), so any
byte sink works — the built-in `ConsolePrinter` forwards to the `esp-println`
console (UART or USB serial JTAG, whichever `esp-println` is configured for):

```rust
fn render(...) {
    // ... render the framebuffer and flush it to the display ...

    #[cfg(feature = "screenshot")]
    {
        let pixel_bytes: alloc::vec::Vec<u8> =
            embedded_screenshot::rgb565_le_bytes(frame_buf.data.as_ref()).collect();
        embedded_screenshot::write_framebuffer(
            &mut embedded_screenshot::ConsolePrinter,
            320, 240,
            embedded_screenshot::RGBL,
            &pixel_bytes,
        ).ok();
    }
}
```

Other `embedded_io::Write` sinks drop in without a shim, which makes
sinks beyond the serial console possible — a second UART, a USB CDC stack,
or a TCP socket for screenshots over the network:

```rust
let mut uart = Uart::new(peripherals.UART1, Config::default())?;
embedded_screenshot::write_framebuffer(&mut uart, 320, 240, RGBL, &pixel_bytes)?;
```

### Blocking vs async

`write_framebuffer` is blocking and works in any app, including plain
super-loop firmware without an executor. Executor-driven apps (embassy &
co) can enable the `async` feature and use the awaitable twin, which shares
the same encoder core:

```toml
features = ["embedded-graphics", "async"]
```

```rust
embedded_screenshot::write_framebuffer_async(&mut uart, 320, 240, RGBL, &pixel_bytes).await?;
```

Build with `cargo b -r --features screenshot`. Without the feature, the
production binary keeps its minimal footprint.

## Capturing and decoding

Install the decoder once with `cargo install` — either from GitHub (no
checkout needed):

```bash
cargo install --git https://github.com/georgik/embedded-screenshot-rs embedded-screenshot-decoder
# or pin to a commit for reproducible builds:
cargo install --git https://github.com/georgik/embedded-screenshot-rs --rev <commit-sha> embedded-screenshot-decoder
```

or from a local clone:

```bash
cargo install --path tool
```

Then flash and monitor the board, capture the serial output, and decode:

```bash
cd boards/<board> && cargo run -r --features screenshot 2>&1 | tee /tmp/serial.log
# Ctrl+C once the dump has been emitted, then:
embedded-screenshot-decoder /tmp/serial.log --output screenshot
```

Outputs `screenshot.png` and `screenshot.ppm`.

The decoder also accepts stdin (`embedded-screenshot-decoder -`) and tolerates
unrelated log noise around the `FRAMEBUFFER_BEGIN`/`FRAMEBUFFER_END` block.

## Notes

- The library is allocator-free: base64 encoding happens in fixed stack
  buffers (2040 characters per line).
- A full 320×240 RGB565 frame is ~150 KB, which takes ~18 s at 115200 baud —
  dump once per boot or raise the UART baud rate.
- Framebuffer content with RNG-seeded scenes is not deterministic; for
  golden-image tests, seed the scene from a constant.
