# Rust DSL Architecture for MCU Targets

## Contents

- Host/Target Split
- Representation Choices
- Crate Boundaries
- Diagnostics and Tooling
- Versioning

## Host/Target Split

Design the DSL as a toolchain, not just a parser. Keep the authoring experience on the host and the execution burden on the MCU.

Host-side responsibilities:

- Parse text, macros, or structured config.
- Produce useful diagnostics with spans and suggestions.
- Validate names, units, ranges, timing budgets, state transitions, and target capabilities.
- Lower syntax into a compact IR.
- Simulate behavior and emit test traces.
- Emit generated Rust, bytecode, or static tables.

Target-side responsibilities:

- Consume only prevalidated artifacts.
- Check artifact magic, version, length, checksum, and target compatibility.
- Execute with fixed memory, bounded loops, and deterministic errors.
- Expose a small HAL-facing API for GPIO, timers, ADC, CAN, UART, SPI, I2C, PWM, or app-specific actions.

## Representation Choices

Choose generated Rust when:

- The DSL is compiled together with firmware.
- The script changes rarely.
- Maximum speed, static typing, and link-time optimization matter.
- Firmware size growth is acceptable.

Choose bytecode when:

- Scripts must be replaceable in flash/EEPROM/external storage.
- A stable runtime VM is preferable to regenerating firmware.
- You can define strict instruction, stack, and memory limits.

Choose static tables when:

- The DSL describes state machines, schedules, protocol frames, register writes, calibration curves, or routing rules.
- Runtime behavior can be implemented as a simple table walker.
- Human-readable syntax is mostly a safer front end for data.

Avoid allowing arbitrary user expressions on the MCU unless there is a bounded evaluator with explicit fuel, stack depth, and numeric semantics.

## Crate Boundaries

Prefer a Rust workspace with these crates:

- `dsl-core`: shared `no_std` types, opcodes, artifact headers, error enums, IR structs, optional `serde` derives behind a feature.
- `dsl-compiler`: `std` parser, validator, optimizer, diagnostics, emitters, golden tests.
- `dsl-runtime`: `no_std` interpreter or table executor, HAL trait surface, artifact loader.
- `dsl-cli`: command-line entry point for compile, check, simulate, disassemble, and emit.
- `firmware`: board-specific app that wires `dsl-runtime` to concrete HAL drivers.

Make `dsl-runtime` depend on `dsl-core`, not on `dsl-compiler`. Keep parser crates and filesystem code out of firmware.

## Diagnostics and Tooling

Use span-aware diagnostics for textual DSLs. Include:

- Source location and offending token.
- Domain-specific explanation.
- Target constraint, such as max stack, max duration, unavailable pin, unsupported peripheral, or flash budget.
- Suggested correction when obvious.

Add CLI subcommands according to need:

- `check`: parse and validate without emitting.
- `build`: emit bytecode/table/generated Rust.
- `sim`: run the DSL against a host mock of the MCU API.
- `dump`: inspect binary artifacts and versions.

## Versioning

Put explicit metadata in emitted artifacts:

- Magic bytes.
- Format version.
- Target family or board profile identifier.
- Endianness when binary data is shared.
- Header length, payload length, CRC/checksum.
- Compiler version or Git hash when available.

Reject incompatible artifacts on target before executing them.
