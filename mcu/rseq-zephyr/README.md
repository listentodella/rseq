# rseq ↔ MCU docking over board-selected transport (Nucleo boards)

This Zephyr app (C + Rust) runs the **rseq-link frame protocol** over the
board-selected byte stream, so the host-side `rseq-cli` can ship a compiled
rseq program to the MCU, the MCU executes it against the board's **real
SPI/I2C/I3C bridge**, and the resulting bus operations are streamed back as
Trace frames. Chip-specific knowledge lives in the rseq script and chip YAML,
not in this firmware.

Verified end-to-end on hardware:

```
$ rseq-cli -f examples/qmi8660_reset.rseq --serial /dev/cu.usbmodem314201 --baud 230400
Dispatching to MCU over serial (/dev/cu.usbmodem314201 @ 230400 baud)...
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
Reset/Ping/Stop) runs over the selected transport per the spec in
`crates/rseq-link/README.md`.

## Architecture

Borrows the proven C+Rust-on-Zephyr scaffold from `mcu/rseq-rs` and replaces its
custom protocol with the rseq-link stack, reusing the rseq crates directly:

- `src/lib.rs` (Rust, `#![no_std]` + `alloc`, the `rustapp` staticlib):
  - `ZephyrTransport` — implements `rseq_link::Transport` over the selected
    Zephyr UART-like transport (`rust_transport_read` drains the RX message
    queue; `rust_transport_write` poll-outs under a mutex).
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
  - `rust_main` — `rust_transport_init` → `PhysicalBus::new` →
    `mcu_loop(ZephyrTransport, bus, &STOP)`. The `zephyr` crate supplies the
    global allocator + panic handler; `rust_printk` (FFI) is used for console
    output.
- `src/zephyr_transport_ffi.c` (C) — optional new-stack USB CDC init for boards
  using `CONFIG_RSEQ_TRANSPORT_USB_CDC`, the common UART-like transport FFI
  (RX irq→`K_MSGQ`; hardware UART TX irq→ring buffer; USB CDC TX
  `uart_poll_out`) + `rust_kernel_delay_us` + `rust_printk`.
- `src/zephyr_bus_ffi.c` (C, from rseq-rs) — SPI transceive + CS + I2C FFI,
  devicetree-bound through stable aliases:
  `rseq-spi`, `rseq-i2c`, `rseq-int1`.
- `app.overlay` — intentionally empty common overlay; board overlays select the
  transport through `/chosen { rseq,transport = &...; }`.
- `boards/<board>.overlay` — board wiring: selected rseq transport, Arduino
  SPI/I2C aliases, DMA wiring, and the `rseq-int1` GPIO.
- `prj.conf` — common `CONFIG_RUST`/`RUST_ALLOC`, `SPI`/`I2C`/`GPIO`, `HWINFO`,
  `MAIN_STACK_SIZE=24576` (VM scratch + TracingBus ~4 KiB buf during EXEC).
  `boards/<board>.conf` selects USB/UART transport and log backend.
- `CMakeLists.txt` — `rust_cargo_application()` (the lang-rust module's `main.c`
  provides `main()`→`rust_main()`) + the two C FFI sources.

## Build

Zephyr automatically picks `boards/<board>.overlay` and
`boards/<board>.conf` when they match `-b`. The firmware is built by
board/hardware topology only. It does not need a bus/address overlay: the script
chooses `bus!(spi)`, `bus!(i2c, addr)`, or `bus_probe!(...)` at runtime.

| Board | rseq-link transport | Console/log backend |
| --- | --- | --- |
| `nucleo_f429zi` | Target USB CDC ACM (`rseq MCU CDC`) | USART3 / ST-LINK VCP |
| `nucleo_f401re` | USART2 / ST-LINK VCP | RTT over SWD |
| Future boards with target USB | USB CDC ACM | Board UART or RTT |
| Future boards without target USB | ST-LINK VCP or external UART | RTT/SWO/none |

```sh
export RSEQ_ROOT=/path/to/rseq
export ZEPHYR_PROJECT=/path/to/zephyrproject
export ZEPHYR_BASE=$ZEPHYR_PROJECT/zephyr
export ZEPHYR_SDK_INSTALL_DIR=/path/to/zephyr-sdk
export ZEPHYR_TOOLCHAIN_VARIANT=zephyr
source $ZEPHYR_PROJECT/.venv/bin/activate
cd $ZEPHYR_PROJECT
```

F429ZI:

```sh
west build -b nucleo_f429zi \
  -s $RSEQ_ROOT/mcu/rseq-zephyr \
  -d $RSEQ_ROOT/mcu/rseq-zephyr/build-f429zi \
  --pristine
```

F401RE:

```sh
west build -b nucleo_f401re \
  -s $RSEQ_ROOT/mcu/rseq-zephyr \
  -d $RSEQ_ROOT/mcu/rseq-zephyr/build-f401re \
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

Flash the ELF via J-Link/OpenOCD/probe-rs as usual:

```gdb
target extended-remote <jlink-host>:3333
load <build-dir>/zephyr/zephyr.elf
monitor reset
monitor go
detach
```

The host still uses the `--serial` option for both profiles:

- F429ZI: select the `rseq MCU CDC` USB CDC port, for example
  `/dev/cu.usbmodem*`.
- F401RE: select the ST-LINK VCP port, for example `/dev/cu.usbmodem*`,
  `/dev/ttyACM*`, or `COMx` depending on host OS.

Console/`printk` logs are separate from the rseq-link transport:

- F429ZI logs are on the board UART console / ST-LINK VCP.
- F401RE logs are on RTT over SWD. Keep the ST-LINK VCP clean for binary
  rseq-link frames.

Then drive it from the host:

```sh
cargo run -p rseq-cli --features serial -- \
  -f examples/qmi8660_reset.rseq \
  --serial /dev/cu.usbmodem314201 --baud 230400
```

If an IRQ script is already running and streaming reports, stop the background
handler without reflashing:

```sh
cargo run -p rseq-cli --features serial -- \
  --serial /dev/cu.usbmodem314201 --baud 230400 \
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
  --serial /dev/cu.usbmodem314201 --baud 230400
```

Expected trace shape: `IRQ pin 0 fired`, then a read from `0x58` (the
read-clear interrupt snapshot), followed by the register operations from the
matching `on(...)` arm.

## Wiring

- **F429ZI USB CDC (transport)**: OTG-FS on PA11(D-)/PA12(D+), enabled by the
  upstream board DTS as `zephyr_udc0`; `boards/nucleo_f429zi.overlay` creates
  `cdc_acm_uart0` and chooses it as `rseq,transport`.
- **F401RE UART (transport)**: USART2 PA2/PA3 through ST-LINK VCP. The board
  profile disables UART console/log output so this port carries only rseq-link
  frames.
- **Arduino SPI**: SCK/MISO/MOSI = PA5/PA6/PA7. CS comes from the board DTS
  `arduino_spi` `cs-gpios` entry: F429ZI uses PD14, F401RE uses PB6.
- **Arduino I2C**: I2C1 SCL/SDA = PB8/PB9 on both currently supported boards.
- **Target INT1**: `rseq-int1` is active-high edge-to-active. F429ZI uses PD15
  (Arduino D9); F401RE uses PC7 (Arduino D9).

## Notes

- The rseq-link transport must stay raw binary. Do not bind Zephyr shell,
  console, or logging to the same UART/CDC device.
- Host tools default to 230400 baud to match the F401RE UART profile. F429ZI's
  USB CDC profile accepts the same setting; the line coding is not the
  bottleneck for the target USB transport.
- F401RE intentionally does not use PA11/PA12 USB OTG by default. If you add an
  external target USB connector later, create a separate board profile/overlay
  that selects USB CDC.
- `mcu_loop` is a no_std port of `rseq-mcu-sim`'s loop (blocking-read contract,
  `&AtomicBool` stop); the protocol logic is identical and covered by the
  `rseq-mcu-sim --self-test` host test.
