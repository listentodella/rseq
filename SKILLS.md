# rseq Skills

This repository vendors the skills used for rseq development under `skills/` so a clone of the repository is self-contained. These files are intended for coding agents and maintainers; they are not Rust crate dependencies.

## Bundled Skills

- `skills/rseq-project`: rseq-specific project entry point. Start here for work in this repository.
- `skills/rust-mcu-dsl`: Rust DSL and no_std MCU VM design workflow.
- `skills/gpui-component`: GPUI component usage and layout guidance.
- `skills/serial`: serial/CDC monitor, logging, hex, send, scan, and TCP forwarding workflows.
- `skills/openocd`: OpenOCD flash/debug/GDB/ITM/semihosting workflows.
- `skills/probe-rs`: probe-rs flash/debug/RTT workflows.
- `skills/jlink`: J-Link flash/debug/RTT/SWO workflows.
- `skills/workflow`: build/flash/debug/observe orchestration.

## How To Use

When working in this repo, read:

```text
skills/rseq-project/SKILL.md
```

Then follow its pointers to the more specific bundled skills as needed.

To install these skills into a Codex user skill directory, copy the desired folders from `skills/` into your Codex skills directory. Keep folder names unchanged.

Example:

```bash
mkdir -p "${CODEX_HOME:-$HOME/.codex}/skills"
cp -R skills/rseq-project "${CODEX_HOME:-$HOME/.codex}/skills/"
cp -R skills/rust-mcu-dsl "${CODEX_HOME:-$HOME/.codex}/skills/"
cp -R skills/gpui-component "${CODEX_HOME:-$HOME/.codex}/skills/"
```

Install hardware helper skills only on machines that need them:

```bash
cp -R skills/serial skills/openocd skills/probe-rs skills/jlink skills/workflow \
  "${CODEX_HOME:-$HOME/.codex}/skills/"
```

## Portability

The bundled skills use repository-relative paths, `<placeholder>` values, or `<skill-dir>` placeholders. They should not require the original developer's local filesystem layout.

Some hardware skills include scripts and example configs copied from the original tool skills. Adjust executable paths and board/chip settings in your local environment or `.embeddedskills/config.json`.
