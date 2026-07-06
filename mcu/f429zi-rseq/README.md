# rseq ↔ MCU docking over USB CDC (Nucleo boards)

This Zephyr app (C + Rust) runs the **rseq-link frame protocol** over the
board's USB CDC-ACM port, so the host-side `rseq-cli` can ship a compiled rseq
program to the MCU, the MCU executes it against the board's **real SPI/I2C/I3C
bridge**, and the resulting bus operations are streamed back as Trace frames.
Chip-specific knowledge lives in the rseq script and chip YAML, not in this
firmware.

Verified end-to-end on hardware:

```
$ rseq-cli -f examples/qmi8660_reset.rseq --serial /dev/cu.usbmodem314201 --baud 115200
Dispatching to MCU over serial (/dev/cu.usbmodem314201 @ 115200 baud)...
✓ Loaded 56 byte(s)
Exec status: Ok
Bus operations (in execution order):
  Step 1: Select spi bus
  Step 2: Read 1 bytes from 0x00000002 → [0x06]   # bus_probe WHOAMI
  Step 3: Write [0x98] → 0x0000007b   # UI.RESET
  Step 4: Delay 50000 μs
  Step 5: Read 1 bytes from 0x00000002 → [0x06]   # UI.WHOAMI
  Step 6: Delay 100 μs
```

The full rseq-link lockstep (Load→Ack, Exec→Ack→Trace*→Result, plus
Reset/Ping/Stop) runs over the CDC port per the spec in
`crates/rseq-link/README.md`.

## Architecture

Borrows the proven C+Rust-on-Zephyr scaffold from `mcu/rseq-rs` and replaces its
custom protocol with the rseq-link stack, reusing the rseq crates directly:

- `src/lib.rs` (Rust, `#![no_std]` + `alloc`, the `rustapp` staticlib):
  - `CdcTransport` — implements `rseq_link::Transport` over the CDC-UART FFI
    (`rust_uart_read` blocks for the first byte then drains; `rust_uart_write`
    poll-outs under a mutex).
  - `PhysicalBus` — implements `rseq_vm::Bus` over the SPI/I2C/I3C FFI. Startup
    only checks which board buses are present; it does not probe any chip ID or
    hard-code any device address. The DSL switches at runtime with `bus!(spi)`,
    `bus!(i2c, addr)`, `bus!(i3c)`, or the generic `bus_probe!(...)`
    instruction that tries DSL-provided candidates.
    The rseq DSL encodes a plain 8-bit register number as the `u32` address, so
    `addr & 0xff` is the register. `bus!(spi)` uses the common 8-bit register
    SPI convention `reg|0x80` (read) / `reg&0x7f` (write), with CS managed
    around each transceive. `bus!(i2c, 0x6a)` sets the 7-bit device address for
    subsequent I2C write-read/write operations. Bare `bus!(i2c)` is rejected by
    the host compiler because the generic firmware cannot know a chip-specific
    default address.
  - `mcu_loop` — no_std port of `rseq-mcu-sim`'s loop: `FrameDecoder` → dispatch
    Load/Exec/Reset/Ping/Stop → reply Ack/Trace(via `TracingBus`)/Result/Pong.
  - `rust_main` — `rust_usb_enable` → `rust_uart_init` → `PhysicalBus::new` →
    `mcu_loop(CdcTransport, bus, &STOP)`. The `zephyr` crate supplies the global
    allocator + panic handler; `rust_printk` (FFI) is used for console output.
- `src/zephyr_cdc_ffi.c` (C) — new-stack USB CDC init (`rust_usb_enable`, one
  CDC port) + the CDC-UART FFI (RX irq→`K_MSGQ`→blocking read, TX `uart_poll_out`)
  + `rust_kernel_delay_us` + `rust_printk`.
- `src/zephyr_bus_ffi.c` (C, from rseq-rs) — SPI transceive + CS + I2C FFI,
  devicetree-bound through stable aliases:
  `rseq-spi`, `rseq-i2c`, `rseq-int1`.
- `app.overlay` — common transport only: one `cdc_acm_uart0` node under
  `&zephyr_udc0`.
- `boards/<board>.overlay` — board wiring: USB UDC label when needed, Arduino
  SPI/I2C aliases, DMA wiring, and the `rseq-int1` GPIO.
- `prj.conf` — `CONFIG_RUST`/`RUST_ALLOC`, new USB CDC stack, `SPI`/`I2C`/`GPIO`,
  `HWINFO`, console+`printk` on board UART, `MAIN_STACK_SIZE=24576` (VM scratch
  + TracingBus ~4 kiB buf during EXEC).
- `CMakeLists.txt` — `rust_cargo_application()` (the lang-rust module's `main.c`
  provides `main()`→`rust_main()`) + the two C FFI sources.

## Build

Zephyr automatically picks `boards/<board>.overlay` when it matches `-b`.
The firmware is built by board/hardware topology only. It does not need a
bus/address overlay: the script chooses `bus!(spi)`, `bus!(i2c, addr)`, or
`bus_probe!(...)` at runtime.

```sh
export ZEPHYR_BASE=/Volumes/tp7100s/work/zephyr/zephyrproject/zephyr
export ZEPHYR_SDK_INSTALL_DIR=/Volumes/tp7100s/work/zephyr/zephyr-sdk-1.0.1
export ZEPHYR_TOOLCHAIN_VARIANT=zephyr
source /Volumes/tp7100s/work/zephyr/zephyrproject/.venv/bin/activate
cd /Volumes/tp7100s/work/zephyr/zephyrproject
```

F429ZI:

```sh
west build -b nucleo_f429zi \
  -s /Volumes/tp7100s/work/rseq/mcu/f429zi-rseq \
  -d /Volumes/tp7100s/work/rseq/mcu/f429zi-rseq/build-f429zi \
  --pristine
```

F401RE:

```sh
west build -b nucleo_f401re \
  -s /Volumes/tp7100s/work/rseq/mcu/f429zi-rseq \
  -d /Volumes/tp7100s/work/rseq/mcu/f429zi-rseq/build-f401re \
  --pristine
```

Expected boot log:

```text
rseq: PhysicalBus::new start
rseq: irq init ok
rseq: default bus -> spi
rseq: physical bus ready
```

Artifacts live under the selected build directory as
`zephyr/zephyr.{elf,bin,hex}`.

## Flash + run

Flash the ELF via J-Link (gdb), then the host enumerates `rseq MCU CDC`
(`0483:5740`) as a single CDC port:

```gdb
target extended-remote <jlink-host>:3333
load <build-dir>/zephyr/zephyr.elf
monitor reset
monitor go
detach
```

Console/`printk` logs are on the board UART console. The rseq-link transport is
the **USB CDC** device (e.g. `/dev/cu.usbmodem*` on macOS).

Then drive it from the host:

```sh
cargo run -p rseq-cli --features serial -- \
  -f examples/qmi8660_reset.rseq \
  --serial /dev/cu.usbmodem314201 --baud 115200
```

If an IRQ script is already running and streaming reports, stop the background
handler without reflashing:

```sh
cargo run -p rseq-cli --features serial -- \
  --serial /dev/cu.usbmodem314201 --baud 115200 \
  --stop
```

`Stop` clears registered IRQ handlers and pending flags. `Reset` clears those
and the loaded main bytecode.

### IRQ smoke test

`wait!(int1, timeout_ms)` maps to VM pin `0`, which this firmware wires through
the `rseq-int1` devicetree alias.
The Zephyr GPIO ISR only gives a semaphore; the rseq VM resumes in normal
thread context, emits an IRQ trace, reads the interrupt-status snapshot declared
by the chip YAML, and runs the matching `irq!(int1)` arm inline.

Host-only dispatch check:

```sh
cargo run -p rseq-cli -- \
  -f examples/qmi8660_irq.rseq \
  --execute --fire int1=0x41
```

Hardware run after the target device INT1 line is connected to the board's
`rseq-int1` pin and the chip has
been configured to assert INT1:

```sh
cargo run -p rseq-cli --features serial -- \
  -f examples/qmi8660_irq.rseq \
  --serial /dev/cu.usbmodem314201 --baud 115200
```

Expected trace shape: `IRQ pin 0 fired`, then a read from `0x58` (the
read-clear interrupt snapshot), followed by the register operations from the
matching `on(...)` arm.

## Wiring

- **F429ZI USB CDC (transport)**: OTG-FS on PA11(D-)/PA12(D+), enabled by the
  upstream board DTS as `zephyr_udc0`.
- **F401RE USB CDC (transport)**: the overlay enables `usbotg_fs` as
  `zephyr_udc0` on PA11/PA12. Confirm your board/wiring exposes those pins to
  the host USB connection; ST-Link VCP alone is not the CDC data channel.
- **Arduino SPI**: SCK/MISO/MOSI = PA5/PA6/PA7. CS comes from the board DTS
  `arduino_spi` `cs-gpios` entry: F429ZI uses PD14, F401RE uses PB6.
- **Arduino I2C**: I2C1 SCL/SDA = PB8/PB9 on both currently supported boards.
- **Target INT1**: `rseq-int1` is active-high edge-to-active. F429ZI uses PD15
  (Arduino D9); F401RE uses PC7 (Arduino D9).

## Notes

- One CDC port only — OTG-FS has 4 bidirectional endpoints and each CDC-ACM
  needs 2, so two CDC-ACM instances don't fit (see the prior single-CDC notes).
  The rseq transport is the single CDC; logs go to the board UART console.
- `mcu_loop` is a no_std port of `rseq-mcu-sim`'s loop (blocking-read contract,
  `&AtomicBool` stop); the protocol logic is identical and covered by the
  `rseq-mcu-sim --self-test` host test.
