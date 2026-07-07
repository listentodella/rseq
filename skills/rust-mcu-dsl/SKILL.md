---
name: rust-mcu-dsl
description: Design and implement custom domain-specific languages in Rust for embedded systems, including parsers, AST/IR design, static validation, bytecode or code generation, host tooling, and no_std MCU runtimes. Use when Codex is asked to create or modify a Rust DSL, compile scripts/configuration into firmware artifacts, run a DSL interpreter on a microcontroller, target embedded-hal/RTIC/Embassy/bare-metal firmware, or bridge PC-side DSL tooling with lower-machine MCU execution.
---

# Rust MCU DSL

## Overview

Build Rust-based DSL toolchains that can be authored on a host and executed safely on an MCU. Keep host-side compiler/parser code expressive, keep target-side runtime `no_std`, bounded, deterministic, and testable.

## Workflow

1. Clarify the DSL job: control logic, device configuration, test sequencing, protocol frames, signal processing, state machines, or another embedded domain.
2. Separate host and target responsibilities early:
   - Host: parse, type-check, optimize, simulate, emit Rust/C/bytecode/binary tables.
   - MCU: run prevalidated bytecode/tables or generated code with bounded memory and timing.
3. Choose an execution model before implementing syntax:
   - Generated Rust when behavior is fixed at build time and performance matters.
   - Compact bytecode when scripts must be updateable without reflashing the full firmware.
   - Static data tables when the DSL mainly configures states, registers, schedules, or messages.
4. Implement the smallest vertical slice first: one DSL construct, parser, validation, emitted artifact, MCU runtime call, and host test.
5. Validate with normal Rust tests on host, then with `no_std`/embedded checks, then hardware or emulator smoke tests when available.

## Architecture Defaults

Prefer a workspace layout like:

```text
crates/
  dsl-core/       # no_std AST/IR/shared types, no allocator by default
  dsl-compiler/   # std host parser, diagnostics, validation, emitters
  dsl-runtime/    # no_std MCU interpreter/runtime
  dsl-cli/        # std CLI for compile/simulate/inspect
firmware/         # board application consuming dsl-runtime artifacts
examples/         # .dsl scripts and generated artifacts
```

Keep `dsl-core` shared and conservative: `#![no_std]`, fixed-width integers, explicit endianness, stable binary formats, no hidden heap allocation. Let `dsl-compiler` use `std`, parser libraries, rich diagnostics, filesystem I/O, and snapshots.

## Implementation Guidance

Use parser libraries on the host when they fit the syntax:

- Use `winnow`, `chumsky`, `pest`, or `nom` for textual DSLs.
- Use `serde` plus `postcard`, `ron`, `toml`, or `yaml` when the DSL is primarily structured configuration.
- Use Rust macros only when the DSL should live inside Rust source and compile with the firmware crate.

For MCU execution, avoid recursive interpreters, unbounded loops, host-sized `usize` in serialized data, dynamic dispatch in hot paths, and fallible allocation at runtime. Prefer fixed instruction limits, explicit stack sizes, `heapless` containers, compile-time maximums, checked arithmetic, and deterministic error codes.

## References

Read only the reference needed for the task:

- `references/architecture.md`: crate layout, DSL representation choices, and host/target boundaries.
- `references/mcu-runtime.md`: `no_std` runtime patterns, bytecode design, memory/timing constraints, and firmware integration.
- `references/validation.md`: test strategy, fuzzing/simulation, safety checks, and release checklist.

## Scripts

Use `scripts/new_rust_mcu_dsl.py` to scaffold a Rust workspace for a host compiler plus `no_std` MCU runtime:

```bash
python3 /path/to/rust-mcu-dsl/scripts/new_rust_mcu_dsl.py my-dsl --output .
```

Read the generated files and adapt names, dependencies, target board crates, and firmware integration to the user's existing project.

## Done Criteria

Finish with a clear statement of the DSL syntax, host compiler command, emitted artifact format, target runtime API, memory limits, timing assumptions, and tests run. If hardware execution was not performed, say exactly what was verified instead.
