# rseq ↔ MCU docking over USB CDC (Nucleo F429ZI)

This Zephyr app (C + Rust) runs the **rseq-link frame protocol** over the
board's USB CDC-ACM port, so the host-side `rseq-cli` can ship a compiled rseq
program to the MCU, the MCU executes it against a **real QMI8660 IMU over SPI**,
and the resulting bus operations are streamed back as Trace frames.

Verified end-to-end on hardware:

```
$ rseq-cli -f examples/qmi8660_reset.rseq --serial /dev/cu.usbmodem314201 --baud 115200
Dispatching to MCU over serial (/dev/cu.usbmodem314201 @ 115200 baud)...
✓ Loaded 29 byte(s)
Exec status: Ok
Bus operations (in execution order):
  Step 1: Write [0x98] → 0x0000007b   # UI.RESET
  Step 2: Delay 50000 μs
  Step 3: Read 1 bytes from 0x00000002 → [0x00]   # UI.WHOAMI
  Step 4: Delay 100 μs
```

The full rseq-link lockstep (Load→Ack, Exec→Ack→Trace*→Result, plus Reset/Ping)
runs over the CDC port per the spec in `crates/rseq-link/README.md`. (The WHOAMI
read returns `0x00` here because the IMU wasn't powered on the SPI header — the
SPI transaction itself succeeds with no BusError; connect the QMI8660 to get
`0x06`.)

## Architecture

Borrows the proven C+Rust-on-Zephyr scaffold from `mcu/rseq-rs` and replaces its
custom protocol with the rseq-link stack, reusing the rseq crates directly:

- `src/lib.rs` (Rust, `#![no_std]` + `alloc`, the `rustapp` staticlib):
  - `CdcTransport` — implements `rseq_link::Transport` over the CDC-UART FFI
    (`rust_uart_read` blocks for the first byte then drains; `rust_uart_write`
    poll-outs under a mutex).
  - `ImuSpiBus` — implements `rseq_vm::Bus` over the SPI FFI. The rseq DSL
    encodes a plain 8-bit register number as the `u32` address, so `addr & 0xff`
    is the register; QMI8660 SPI convention `reg|0x80` (read) / `reg&0x7f`
    (write) lives here, with CS managed around each transceive.
  - `mcu_loop` — no_std port of `rseq-mcu-sim`'s loop: `FrameDecoder` → dispatch
    Load/Exec/Reset/Ping → reply Ack/Trace(via `TracingBus`)/Result/Pong.
  - `rust_main` — `rust_usb_enable` → `rust_uart_init` → `ImuSpiBus::new` →
    `mcu_loop(CdcTransport, bus, &STOP)`. The `zephyr` crate supplies the global
    allocator + panic handler; `rust_printk` (FFI) is used for console output.
- `src/zephyr_cdc_ffi.c` (C) — new-stack USB CDC init (`rust_usb_enable`, one
  CDC port) + the CDC-UART FFI (RX irq→`K_MSGQ`→blocking read, TX `uart_poll_out`)
  + `rust_kernel_delay_us` + `rust_printk`.
- `src/zephyr_bus_ffi.c` (C, from rseq-rs) — SPI transceive + CS + I2C FFI,
  devicetree-bound to `arduino_spi` / `spi_probe_dev` / CS.
- `app.overlay` — one `cdc_acm_uart0` node under `&zephyr_udc0` + the
  `spi_probe_dev` child on `&arduino_spi`.
- `prj.conf` — `CONFIG_RUST`/`RUST_ALLOC`, new USB CDC stack, `SPI`/`I2C`/`GPIO`,
  `HWINFO`, console+`printk` on USART3, `MAIN_STACK_SIZE=16384` (VM 4 KiB scratch
  + TracingBus ~4 kiB buf during EXEC).
- `CMakeLists.txt` — `rust_cargo_application()` (the lang-rust module's `main.c`
  provides `main()`→`rust_main()`) + the two C FFI sources.

## Build

```sh
export ZEPHYR_BASE=/Volumes/tp7100s/work/zephyr/zephyrproject/zephyr
export ZEPHYR_SDK_INSTALL_DIR=/Volumes/tp7100s/work/zephyr/zephyr-sdk-1.0.1
export ZEPHYR_TOOLCHAIN_VARIANT=zephyr
source /Volumes/tp7100s/work/zephyr/zephyrproject/.venv/bin/activate
cd /Volumes/tp7100s/work/zephyr/zephyrproject
west build -b nucleo_f429zi -s /Volumes/tp7100s/work/rseq/mcu/f429zi-rseq \
  -d /Volumes/tp7100s/work/rseq/mcu/f429zi-rseq/build --pristine
```

Artifacts: `build/zephyr/zephyr.{elf,bin,hex}` (FLASH ~97 KiB / 2 MiB, RAM ~34 KiB / 192 KiB).

## Flash + run

Flash the ELF via J-Link (gdb), then the host enumerates `rseq F429ZI CDC`
(`0483:5740`) as a single CDC port:

```gdb
target extended-remote <jlink-host>:3333
load build/zephyr/zephyr.elf
monitor reset
monitor go
detach
```

Console/`printk` logs are on **USART3** (ST-Link VCP). The rseq-link transport
is the **USB CDC** device (e.g. `/dev/cu.usbmodem*` on macOS).

Then drive it from the host:

```sh
cargo run -p rseq-cli --features serial -- \
  -f examples/qmi8660_reset.rseq \
  --serial /dev/cu.usbmodem314201 --baud 115200
```

### IRQ smoke test

`wait!(int1, timeout_ms)` maps to VM pin `0`, which this firmware wires to PB8.
The Zephyr GPIO ISR only gives a semaphore; the rseq VM resumes in normal
thread context, emits an IRQ trace, reads the QMI8660 interrupt-status snapshot,
and runs the matching `irq!(int1)` arm inline.

Host-only dispatch check:

```sh
cargo run -p rseq-cli -- \
  -f examples/qmi8660_irq.rseq \
  --execute --fire int1=0x41
```

Hardware run after the QMI8660 INT1 line is connected to PB8 and the chip has
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

- **USB CDC (transport)**: OTG-FS on PA11(D-)/PA12(D+), enabled by the board
  DTS as `zephyr_udc0`. Self-powered via ST-Link USB (CN1).
- **IMU (QMI8660)** on Arduino SPI: SCK/MOSI/MISO = PA7/PA6/PA5 (`arduino_spi`
  = SPI1), CS = PD10 (`arduino_spi` cs-gpios[0]). Connect + power the IMU for
  real register reads (e.g. WHOAMI = 0x06).
- **IMU INT1**: connect the QMI8660 INT1 output to PB8. The overlay declares
  `rseq_int1` as active-high and the firmware uses `GPIO_INT_EDGE_TO_ACTIVE`.

## Notes

- One CDC port only — OTG-FS has 4 bidirectional endpoints and each CDC-ACM
  needs 2, so two CDC-ACM instances don't fit (see the prior single-CDC notes).
  The rseq transport is the single CDC; logs go to USART3.
- `mcu_loop` is a no_std port of `rseq-mcu-sim`'s loop (blocking-read contract,
  `&AtomicBool` stop); the protocol logic is identical and covered by the
  `rseq-mcu-sim --self-test` host test.
