use clap::Parser;
use rseq::{compile, decompile, parse};
use rseq_vm::Vm;
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
}

fn main() {
    let cli = Cli::parse();

    if cli.decompile {
        let data = if let Some(file) = cli.file {
            std::fs::read(file).expect("Failed to read bytecode file")
        } else {
            eprintln!("Please provide a bytecode file with --file");
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
        let src = if let Some(file) = cli.file {
            let content = std::fs::read_to_string(file).expect("Failed to read rseq file");
            println!("Original rseq content:\n{}", content);
            content
        } else {
            let default = "write!(0x10, 0xaa, 100);";
            println!("Using default rseq content:\n{}", default);
            default.to_string()
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

        println!("\nCompiling to bytecode...");
        let bytecode = match compile(&program) {
            Ok(b) => {
                println!("✓ Compiled successfully ({} bytes)", b.len());
                println!("Bytecode: {:02x?}", b);
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

        println!("\nStatements:");
        for stmt in &program.stmts {
            match stmt {
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
                        let delay_str = if let Some(d) = delay_us {
                            format!(", delay: {} μs", d)
                        } else {
                            "".to_string()
                        };
                        println!(
                            "  let {} = read from {}: {} bytes{}",
                            name, addr_str, len_str, delay_str
                        );
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
                    let delay_str = if let Some(d) = delay_us {
                        format!(", delay: {} μs", d)
                    } else {
                        "".to_string()
                    };
                    println!("  write {} to {} address{}", val_str, addr_str, delay_str);
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
                    println!("Delay count: {} us", bus.get_delay_count());

                    let mem = bus.get_memory();
                    if !mem.is_empty() {
                        println!("Written memory:");
                        for (addr, val) in mem {
                            println!("  0x{addr:08x} → 0x{val:02x}");
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

#[cfg(test)]
mod tests {
    use super::*;

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

        assert_eq!(*bus.get_memory().get(&0x40).unwrap(), 0x01);
        assert_eq!(*bus.get_memory().get(&0x41).unwrap(), 0x02);
        assert_eq!(*bus.get_memory().get(&0x42).unwrap(), 0x03);
        assert_eq!(*bus.get_memory().get(&0x100).unwrap(), 0xaa);
        assert_eq!(bus.get_delay_count(), 500);
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
}
