use clap::Parser;
use rseq::{compile_with_base, decompile, parse, resolve_chip_path, ChipRegistry};
use rseq_vm::Vm;
use std::path::Path;
pub mod mock;
use mock::MockBus;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[arg(short, long)]
    file: Option<String>,

    #[arg(short, long)]
    decompile: bool,

    #[arg(short, long)]
    execute: bool,

    #[arg(short, long)]
    output: Option<String>,

    #[arg(short = 'x', long)]
    hex: Option<String>,
}

fn main() {
    let cli = Cli::parse();

    if cli.decompile {
        let data = if let Some(hex_str) = &cli.hex {
            parse_hex_string(hex_str).expect("Failed to parse hex string")
        } else if let Some(file) = cli.file {
            std::fs::read(file).expect("Failed to read bytecode file")
        } else {
            eprintln!(
                "Please provide either a bytecode file with --file or a hex string with --hex"
            );
            std::process::exit(1);
        };

        println!("Decompiling bytecode...");
        match decompile(&data) {
            Ok(output) => {
                println!("Decompiled rseq:\n{}", output);
            }
            Err(e) => {
                eprintln!("Decompile error: {:?}", e);
                std::process::exit(1);
            }
        }
    } else {
        let (src, base_dir) = if let Some(file) = &cli.file {
            let content = std::fs::read_to_string(file).expect("Failed to read rseq file");
            println!("Original rseq content:\n{}", content);
            let base = Path::new(file).parent().map(Path::to_path_buf);
            (content, base)
        } else {
            let default = "write!(0x10, 0xaa, 100);";
            println!("Using default rseq content:\n{}", default);
            (default.to_string(), None)
        };

        println!("\nParsing rseq...");
        let program = match parse(&src) {
            Ok(p) => {
                println!("✓ Parsed successfully");
                p
            }
            Err(e) => {
                eprintln!("Parse error: {e:?}");
                std::process::exit(1);
            }
        };

        let mut chip_registry = ChipRegistry::default();
        for stmt in &program.stmts {
            if let rseq::Stmt::Chip { path } = stmt {
                let chip_path = resolve_chip_path(path, base_dir.as_deref());
                match chip_registry.load_file(&chip_path) {
                    Ok(()) => {
                        let chip = chip_registry.chips().last().expect("chip loaded");
                        let reg_count: usize = chip.pages.iter().map(|p| p.registers.len()).sum();
                        println!(
                            "✓ Loaded chip '{}' from {} ({} pages, {} registers)",
                            chip.sensor,
                            chip_path.display(),
                            chip.pages.len(),
                            reg_count
                        );
                    }
                    Err(e) => {
                        eprintln!("Chip load error: {e}");
                        std::process::exit(1);
                    }
                }
            }
        }

        println!("\nCompiling to bytecode...");
        let bytecode = match compile_with_base(&program, base_dir.as_deref()) {
            Ok(b) => {
                println!("✓ Compiled successfully ({} bytes)", b.len());
                println!("Bytecode (vec): {:02x?}", b);
                let hex_str: String = b
                    .iter()
                    .map(|byte| format!("{:02x}", byte))
                    .collect::<Vec<_>>()
                    .join(" ");
                println!("Bytecode (hex): {}", hex_str);
                if let Some(path) = &cli.output {
                    std::fs::write(path, &b).expect("Failed to write bytecode");
                    println!("Saved bytecode to {}", path);
                }
                b
            }
            Err(e) => {
                eprintln!("Compile error: {e:?}");
                std::process::exit(1);
            }
        };

        println!("\nStatements (in order):");
        for (idx, stmt) in program.stmts.iter().enumerate() {
            println!("  Step {}:", idx + 1);
            match stmt {
                rseq::Stmt::Chip { path } => {
                    println!("    Action: Load chip dictionary from {path}");
                }
                rseq::Stmt::Let { name, expr } => match expr {
                    rseq::Expr::Read {
                        addr,
                        len,
                        delay_us,
                    } => {
                        let addr_str = match addr {
                            rseq::Value::Number(n) => format!("0x{:x}", n),
                            rseq::Value::Ident(s) => s.clone(),
                            _ => "unknown".to_string(),
                        };
                        let len_str = match len {
                            rseq::Value::Number(n) => n.to_string(),
                            rseq::Value::Ident(s) => s.clone(),
                            _ => "unknown".to_string(),
                        };
                        println!(
                            "    Action: Read {} bytes from address {}",
                            len_str, addr_str
                        );
                        println!("    Bind to: {}", name);
                        if let Some(d) = delay_us {
                            println!("    Delay: {} μs after read", d);
                        }
                    }
                },
                rseq::Stmt::Write {
                    addr,
                    val,
                    delay_us,
                } => {
                    let addr_str = match addr {
                        rseq::Value::Number(n) => format!("0x{:x}", n),
                        rseq::Value::Ident(s) => s.clone(),
                        _ => "unknown".to_string(),
                    };
                    let val_str = match val {
                        rseq::Value::Number(n) => format!("0x{:02x}", n),
                        rseq::Value::Array(arr) => {
                            let mut s = "[".to_string();
                            for (i, v) in arr.iter().enumerate() {
                                if i > 0 {
                                    s.push_str(", ");
                                }
                                match v {
                                    rseq::Value::Number(n) => s.push_str(&format!("0x{:02x}", n)),
                                    _ => s.push_str("unknown"),
                                }
                            }
                            s.push(']');
                            s
                        }
                        rseq::Value::Ident(s) => s.clone(),
                    };
                    println!("    Action: Write {} to address {}", val_str, addr_str);
                    if let Some(d) = delay_us {
                        println!("    Delay: {} μs after write", d);
                    }
                }
            }
        }

        if cli.execute {
            println!("\nExecuting in MockBus...");
            let mut bus = MockBus::new();
            let mut vm = Vm::new(&mut bus, &bytecode);

            match vm.run() {
                Ok(_) => {
                    println!("✓ Execution completed successfully");

                    let ops = bus.ops();
                    if ops.is_empty() {
                        println!("No bus operations recorded");
                    } else {
                        println!("Bus operations (in execution order):");
                        for (step, op) in ops.iter().enumerate() {
                            match op {
                                mock::BusOp::Write { addr, data } => {
                                    let bytes: String = data
                                        .iter()
                                        .map(|b| format!("0x{b:02x}"))
                                        .collect::<Vec<_>>()
                                        .join(", ");
                                    println!("  Step {}: Write [{bytes}] → 0x{addr:08x}", step + 1);
                                }
                                mock::BusOp::Read { addr, data } => {
                                    let bytes: String = data
                                        .iter()
                                        .map(|b| format!("0x{b:02x}"))
                                        .collect::<Vec<_>>()
                                        .join(", ");
                                    println!(
                                        "  Step {}: Read {} bytes from 0x{addr:08x} → [{bytes}]",
                                        step + 1,
                                        data.len()
                                    );
                                }
                                mock::BusOp::Delay { us } => {
                                    println!("  Step {}: Delay {us} μs", step + 1);
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    eprintln!("Execution error: {e:?}");
                    std::process::exit(1);
                }
            }
        } else {
            println!("\nUse --execute to run in MockBus");
        }
    }
}

fn parse_hex_string(hex_str: &str) -> Result<Vec<u8>, String> {
    hex_str
        .split_whitespace()
        .map(|s| {
            u8::from_str_radix(s, 16)
                .map_err(|e| format!("Failed to parse hex byte '{}': {}", s, e))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rseq::compile;

    #[test]
    fn test_compile_and_run() {
        let src = r"
        write!(0x40, [0x01, 0x02, 0x03], 500);
        write!(0x100, 0xaa);
        ";
        let program = parse(src).unwrap();
        let bytecode = compile(&program).unwrap();

        let mut bus = MockBus::new();
        let mut vm = Vm::new(&mut bus, &bytecode);
        vm.run().unwrap();

        let ops = bus.ops();
        assert_eq!(ops.len(), 3);
        assert!(matches!(&ops[0], mock::BusOp::Write { addr: 0x40, data } if data == &[0x01, 0x02, 0x03]));
        assert!(matches!(&ops[1], mock::BusOp::Delay { us: 500 }));
        assert!(matches!(&ops[2], mock::BusOp::Write { addr: 0x100, data } if data == &[0xaa]));
    }

    #[test]
    fn test_decompile() {
        let src = r"
        write!(0x40, [0x01, 0x02, 0x03], 500);
        write!(0x100, 0xaa);
        ";
        let program = parse(src).unwrap();
        let bytecode = compile(&program).unwrap();

        let decompiled = decompile(&bytecode).unwrap();
        assert!(decompiled.contains("write!(0x40, [0x01, 0x02, 0x03], 500);"));
        assert!(decompiled.contains("write!(0x100, 0xaa);"));
    }

    #[test]
    fn test_parse_hex_string() {
        let hex_str = "02 40 00 00 00 03 00 00 00 f4 01 00 00 01 02 03 ff";
        let parsed = parse_hex_string(hex_str).unwrap();
        assert_eq!(parsed.len(), 17);
        assert_eq!(parsed[0], 0x02);
        assert_eq!(parsed[16], 0xff);
    }
}
