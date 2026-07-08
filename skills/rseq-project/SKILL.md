---
name: rseq-project
description: Project-specific development workflow for the rseq Rust DSL, host tools, GPUI workbench, Zephyr MCU firmware bridge, serial report debugging, register metadata, capture/replay, and cross-platform maintenance. Use when working inside this repository or when a task mentions rseq, .rseq scripts, report_format!, report!, qmi8660.yaml, rseq-cli, rseq-tui, rseq-gpui, rseq-lsp, or the MCU no_std VM.
---

# rseq Project Skill

Use this skill as the project entry point. When a task also matches a more specific bundled skill, read that skill next:

- DSL/compiler/VM work: `../rust-mcu-dsl/SKILL.md`
- GPUI component work: `../gpui-component/SKILL.md`
- GPUI desktop packaging/release bundles: `../packaging/SKILL.md`
- Serial/CDC/report observation: `../serial/SKILL.md`
- OpenOCD/GDB debug: `../openocd/SKILL.md`
- probe-rs or J-Link debug: `../probe-rs/SKILL.md` or `../jlink/SKILL.md`
- Build/flash/debug/observe orchestration: `../workflow/SKILL.md`

## Repository Map

- `crates/rseq`: DSL parser, AST, compiler, bytecode model.
- `crates/rseq-runtime`: no_std MCU VM/runtime support.
- `crates/rseq-link`: host/MCU wire frame protocol.
- `crates/rseq-host`: host-side compile/load/session/report/register metadata utilities.
- `crates/rseq-cli`: command-line host tool.
- `crates/rseq-tui`: terminal UI.
- `crates/rseq-gpui`: GPUI desktop workbench.
- `crates/rseq-lsp`: rseq language server and editor intelligence.
- `crates/rseq-mcu-sim`: MCU simulator/mock link tests.
- `mcu/f429zi-rseq`: Zephyr firmware bridge.
- `examples`: `.rseq` scripts and capture samples.
- `qmi8660.yaml`: example chip metadata used by host tools.

## Architecture Rules

- Treat `.rseq` as the executable configuration source.
- Treat chip YAML as metadata for register names, bit fields, no-dump rules, decode hints, and UI details.
- Keep MCU firmware a generic I2C/SPI/I3C bridge and VM runner. Do not add chip-specific sensor logic to firmware.
- Put reusable host behavior in `crates/rseq-host` when CLI, TUI, and GPUI all need it.
- Keep serial and `HostLink` work off the GPUI main thread.
- Avoid heavy parsing, file I/O, serial I/O, and full-source analysis in GPUI render functions.

## Report And FIFO Rules

`report!` frames should include kind, frame_id, timestamp, payload length, and payload bytes.

Host tools should use these for loss detection, out-of-order detection, timestamp delta statistics, structured decode, capture, and replay.

Example report format:

```rseq
report_format!(FIFO_RAW, i16_le, {
    fields: [gx, gy, gz, ax, ay, az, temp],
    gyro_fields: [gx, gy, gz],
    accel_fields: [ax, ay, az],
    temp_field: temp,
    accel_fs_g: 16,
    gyro_fs_dps: 4096,
    temp_lsb_per_c: 256,
    temp_offset_c: 25,
    output: physical_f32,
});
```

Use `output: raw_i16` for raw integer display.
Use `output: physical_f32` for accelerometer `m/s^2`, gyro `rad/s`, and temperature Celsius.

## Sequences Editor Lessons

The GPUI Sequences text editor currently prioritizes stable editing latency. Do not reintroduce a highlight preview that:

- scans the full source on every render
- scans the full source once per visible line
- creates one GPUI element per token for the whole file
- runs chip YAML analysis every render

If syntax highlighting is reimplemented:

1. Prefer the editor's internal highlighter after fixing semantic-token range conversion.
2. Cache tokenization by source version and chip metadata version.
3. Render only visible lines.
4. Tokenize once per source version, not once per line.

## Hardware Debug Rules

For serial/CDC report debugging:

```bash
cargo run -p rseq-cli --features serial -- \
  --watch \
  --serial <serial-port> \
  --baud 115200
```

For GPUI hardware testing:

```bash
cargo run -p rseq-gpui --features serial -- \
  --serial <serial-port> \
  --baud 115200 \
  --chip qmi8660.yaml \
  -f examples/qmi8660_fifo.rseq
```

For Zephyr firmware:

```bash
source <zephyr-venv>/bin/activate
cd mcu/f429zi-rseq
west build -b <board-name>
west flash
```

Use `.fish` activation only when the current shell is fish:

```fish
source <zephyr-venv>/bin/activate.fish
```

## Validation

For parser/compiler/runtime changes:

```bash
cargo test -p rseq
cargo test -p rseq-host
cargo test -p rseq-mcu-sim
```

For host UI changes:

```bash
cargo fmt --all --check
cargo test -p rseq-tui --features serial
cargo test -p rseq-gpui --features serial
cargo check -p rseq-gpui --features serial
```

For cross-platform path or newline changes:

```bash
cargo test --workspace
```

Also verify Windows-style invocation when relevant:

```powershell
cargo run -p rseq-cli -- -f .\examples\qmi8660_fifo.rseq
```

## Done Criteria

Report:

- files changed
- behavior changed
- commands run
- whether hardware was tested
- remaining risk

For MCU/DSL changes, also report syntax impact, emitted bytecode/runtime impact, target memory/timing assumptions, and host-vs-MCU responsibilities.
