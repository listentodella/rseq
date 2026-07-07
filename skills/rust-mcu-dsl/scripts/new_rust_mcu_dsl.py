#!/usr/bin/env python3
"""Scaffold a Rust workspace for a host DSL compiler plus no_std MCU runtime."""

from __future__ import annotations

import argparse
import re
from pathlib import Path


def kebab(name: str) -> str:
    value = re.sub(r"[^a-zA-Z0-9]+", "-", name.strip()).strip("-").lower()
    value = re.sub(r"-{2,}", "-", value)
    if not value:
        raise SystemExit("Project name must contain at least one letter or digit")
    return value


def crate_ident(crate_name: str) -> str:
    return crate_name.replace("-", "_")


def write(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("name", help="DSL project name, for example gpio-script")
    parser.add_argument("--output", default=".", help="Directory where the project folder is created")
    args = parser.parse_args()

    name = kebab(args.name)
    root = Path(args.output).expanduser().resolve() / name
    if root.exists():
        raise SystemExit(f"Refusing to overwrite existing directory: {root}")

    core = f"{name}-core"
    compiler = f"{name}-compiler"
    runtime = f"{name}-runtime"
    cli = f"{name}-cli"
    core_ident = crate_ident(core)
    compiler_ident = crate_ident(compiler)

    write(
        root / "Cargo.toml",
        f"""[workspace]
members = [
    "crates/{core}",
    "crates/{compiler}",
    "crates/{runtime}",
    "crates/{cli}",
]
resolver = "2"
""",
    )
    write(
        root / f"crates/{core}/Cargo.toml",
        f"""[package]
name = "{core}"
version = "0.1.0"
edition = "2021"

[features]
default = []
""",
    )
    write(
        root / f"crates/{core}/src/lib.rs",
        """#![no_std]

pub const MAGIC: &[u8; 4] = b"DSL0";
pub const FORMAT_VERSION: u8 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Opcode {
    Nop = 0x00,
    SetOutput = 0x01,
    DelayMs = 0x02,
    Halt = 0xff,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Instruction {
    pub opcode: Opcode,
    pub a: u8,
    pub b: u8,
    pub imm: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecodeError {
    BadMagic,
    BadVersion,
    Truncated,
    UnknownOpcode(u8),
}
""",
    )
    write(
        root / f"crates/{runtime}/Cargo.toml",
        f"""[package]
name = "{runtime}"
version = "0.1.0"
edition = "2021"

[dependencies]
{core} = {{ path = "../{core}" }}
""",
    )
    write(
        root / f"crates/{runtime}/src/lib.rs",
        f"""#![cfg_attr(not(test), no_std)]

use {core_ident}::MAGIC;

pub trait Device {{
    type Error;

    fn set_output(&mut self, pin: u8, high: bool) -> Result<(), Self::Error>;
    fn delay_ms(&mut self, ms: u16) -> Result<(), Self::Error>;
}}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Step {{
    Running,
    Done,
}}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error<E> {{
    Device(E),
    BadMagic,
    PcOutOfBounds,
    FuelExhausted,
    UnknownOpcode(u8),
}}

pub struct Vm {{
    pc: usize,
    fuel: u32,
}}

impl Vm {{
    pub const fn new(fuel: u32) -> Self {{
        Self {{ pc: 0, fuel }}
    }}

    pub fn step<D: Device>(&mut self, program: &[u8], device: &mut D) -> Result<Step, Error<D::Error>> {{
        if !program.starts_with(MAGIC) {{
            return Err(Error::BadMagic);
        }}
        if self.fuel == 0 {{
            return Err(Error::FuelExhausted);
        }}
        self.fuel -= 1;

        let body = &program[MAGIC.len()..];
        let op = *body.get(self.pc).ok_or(Error::PcOutOfBounds)?;
        self.pc += 1;

        match op {{
            0x00 => Ok(Step::Running),
            0x01 => {{
                let pin = *body.get(self.pc).ok_or(Error::PcOutOfBounds)?;
                let high = *body.get(self.pc + 1).ok_or(Error::PcOutOfBounds)? != 0;
                self.pc += 2;
                device.set_output(pin, high).map_err(Error::Device)?;
                Ok(Step::Running)
            }}
            0x02 => {{
                let lo = *body.get(self.pc).ok_or(Error::PcOutOfBounds)?;
                let hi = *body.get(self.pc + 1).ok_or(Error::PcOutOfBounds)?;
                self.pc += 2;
                device.delay_ms(u16::from_le_bytes([lo, hi])).map_err(Error::Device)?;
                Ok(Step::Running)
            }}
            0xff => Ok(Step::Done),
            other => Err(Error::UnknownOpcode(other)),
        }}
    }}
}}
""",
    )
    write(
        root / f"crates/{compiler}/Cargo.toml",
        f"""[package]
name = "{compiler}"
version = "0.1.0"
edition = "2021"

[dependencies]
{core} = {{ path = "../{core}" }}
""",
    )
    write(
        root / f"crates/{compiler}/src/lib.rs",
        f"""use {core_ident}::MAGIC;

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct CompileError {{
    pub line: usize,
    pub message: String,
}}

impl CompileError {{
    fn new(line: usize, message: impl Into<String>) -> Self {{
        Self {{
            line,
            message: message.into(),
        }}
    }}
}}

pub fn compile(source: &str) -> Result<Vec<u8>, CompileError> {{
    let mut out = Vec::from(MAGIC.as_slice());
    for (line_no, raw) in source.lines().enumerate() {{
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {{
            continue;
        }}
        let parts: Vec<_> = line.split_whitespace().collect();
        match parts.as_slice() {{
            ["nop"] => out.push(0x00),
            ["set", pin, value] => {{
                out.push(0x01);
                out.push(
                    pin.parse()
                        .map_err(|_| CompileError::new(line_no + 1, "invalid pin"))?,
                );
                out.push(match *value {{
                    "high" | "on" | "true" => 1,
                    "low" | "off" | "false" => 0,
                    _ => return Err(CompileError::new(line_no + 1, "expected high/low")),
                }});
            }}
            ["delay", ms] => {{
                out.push(0x02);
                let ms = ms
                    .parse::<u16>()
                    .map_err(|_| CompileError::new(line_no + 1, "invalid delay"))?;
                out.extend_from_slice(&ms.to_le_bytes());
            }}
            ["halt"] => out.push(0xff),
            _ => return Err(CompileError::new(line_no + 1, format!("unknown statement: {{line}}"))),
        }}
    }}
    Ok(out)
}}

#[cfg(test)]
mod tests {{
    use super::*;

    #[test]
    fn compiles_blink() {{
        let bytes = compile("set 13 high\\ndelay 250\\nhalt\\n").unwrap();
        assert!(bytes.starts_with(MAGIC));
        assert_eq!(bytes.last(), Some(&0xff));
    }}
}}
""",
    )
    write(
        root / f"crates/{cli}/Cargo.toml",
        f"""[package]
name = "{cli}"
version = "0.1.0"
edition = "2021"

[dependencies]
{compiler} = {{ path = "../{compiler}" }}
""",
    )
    write(
        root / f"crates/{cli}/src/main.rs",
        f"""use std::path::PathBuf;

fn main() {{
    if let Err(err) = run() {{
        eprintln!("error: {{err}}");
        std::process::exit(1);
    }}
}}

fn run() -> Result<(), String> {{
    let mut args = std::env::args_os().skip(1);
    let input = args
        .next()
        .map(PathBuf::from)
        .ok_or_else(|| "usage: {cli} <input.dsl> --output <output.bin>".to_string())?;
    let flag = args.next().ok_or_else(|| "missing --output".to_string())?;
    if flag != "--output" && flag != "-o" {{
        return Err("expected --output or -o".to_string());
    }}
    let output = args
        .next()
        .map(PathBuf::from)
        .ok_or_else(|| "missing output path".to_string())?;

    let source = std::fs::read_to_string(&input).map_err(|err| format!("read input: {{err}}"))?;
    let bytes = {compiler_ident}::compile(&source)
        .map_err(|err| format!("line {{}}: {{}}", err.line, err.message))?;
    std::fs::write(&output, bytes).map_err(|err| format!("write output: {{err}}"))?;
    Ok(())
}}
""",
    )
    write(
        root / "examples/blink.dsl",
        """set 13 high
delay 250
set 13 low
delay 250
halt
""",
    )
    print(root)


if __name__ == "__main__":
    main()
