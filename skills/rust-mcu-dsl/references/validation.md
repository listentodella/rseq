# Validation and Safety Checklist

## Contents

- Host Tests
- Parser and Compiler Robustness
- Target Runtime Tests
- Hardware Checks
- Release Checklist

## Host Tests

Create tests at several levels:

- Parser tests for accepted and rejected syntax.
- Snapshot or golden tests for diagnostics.
- Lowering tests from AST to IR/bytecode/table.
- Simulator tests for expected device calls and timing.
- Artifact round-trip tests for encode/decode compatibility.

Prefer small examples that represent real embedded tasks. Keep at least one end-to-end test from `.dsl` source to emitted artifact to simulated execution.

## Parser and Compiler Robustness

Use fuzzing or property tests when the DSL parser or binary decoder accepts untrusted input. Good targets:

- Text parser never panics.
- Binary artifact decoder never panics.
- Validator rejects out-of-range indexes, jump targets, stack depths, and invalid peripheral references.
- Disassembler handles every valid opcode and reports unknown opcodes cleanly.

For Rust projects, consider `cargo fuzz`, `proptest`, or table-driven tests depending on project complexity.

## Target Runtime Tests

Keep runtime tests host-runnable when possible by building `dsl-runtime` with `std` only for tests:

```rust
#![cfg_attr(not(test), no_std)]
```

Use a fake `Device` implementation to assert calls and errors. Test:

- Artifact header rejection.
- Program counter bounds.
- Stack overflow and underflow.
- Fuel exhaustion.
- Device error propagation.
- Yield/resume behavior.

If the runtime must stay pure `no_std`, use `cargo test -p dsl-runtime --features std-test` or a local pattern already used by the repository.

## Hardware Checks

When hardware is available, run the narrowest meaningful smoke test:

- Build the firmware for the target.
- Flash it using the repo's normal tool.
- Observe RTT, serial, semihosting, LED, GPIO, CAN, UART, or other expected output.
- Confirm that invalid artifacts fail safely.

If hardware is not available, state that validation stopped at build/test/simulation and list the exact commands run.

## Release Checklist

Before calling a Rust MCU DSL implementation done, verify:

- The DSL syntax and execution semantics are documented in code or tests.
- Host compiler rejects invalid scripts before target execution.
- Artifact format has magic/version/length/checksum or an equivalent guard.
- MCU runtime is `no_std` or the firmware target explicitly supports allocation.
- All runtime buffers have fixed maximum sizes.
- Loops, waits, recursion, and event handling are bounded.
- Endianness and integer widths are explicit.
- Host simulation and target runtime agree on behavior.
- Firmware integration exposes only the device capabilities the DSL needs.
- Test commands and hardware status are reported to the user.
