use clap::Parser;
use rseq::{
    ChipRegistry, compile_with_base_detailed, decompile, parse_detailed, resolve_chip_path,
};
use rseq_vm::Vm;
use std::ops::Range;
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
        let source_name = cli.file.as_deref().unwrap_or("<default>");
        let program = match parse_detailed(&src) {
            Ok(p) => {
                println!("✓ Parsed successfully");
                p
            }
            Err(errors) => {
                for error in errors {
                    emit_diagnostic(
                        source_name,
                        &src,
                        error.span,
                        "could not parse rseq source",
                        &error.message,
                        Some("check the macro syntax and punctuation near this location"),
                    );
                }
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
        let bytecode = match compile_with_base_detailed(&program, base_dir.as_deref()) {
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
            Err(diag) => {
                emit_diagnostic(
                    source_name,
                    &src,
                    diag.span,
                    "could not compile rseq source",
                    &diag.message,
                    diag.help.as_deref(),
                );
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
                        rseq::Value::FieldMap(_) => "unknown".to_string(),
                    };
                    println!("    Action: Write {} to address {}", val_str, addr_str);
                    if let Some(d) = delay_us {
                        println!("    Delay: {} μs after write", d);
                    }
                }
                rseq::Stmt::Update {
                    target,
                    val,
                    delay_us,
                } => {
                    match val {
                        rseq::Value::Number(n) => {
                            println!("    Action: Update {target} = {n} (read-modify-write)");
                        }
                        rseq::Value::FieldMap(entries) => {
                            let fields: Vec<String> = entries
                                .iter()
                                .map(|(name, value)| format!("{name}={value}"))
                                .collect();
                            println!(
                                "    Action: Update {target} {{{}}} (read-modify-write)",
                                fields.join(", ")
                            );
                        }
                        _ => println!("    Action: Update {target} (read-modify-write)"),
                    }
                    if let Some(d) = delay_us {
                        println!("    Delay: {} μs after update", d);
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

fn emit_diagnostic(
    source_name: &str,
    source: &str,
    byte_span: Range<usize>,
    title: &str,
    label: &str,
    help: Option<&str>,
) {
    use ariadne::{Color, Label, Report, ReportKind, Source};

    let span = byte_span_to_char_span(source, byte_span);
    let source_id = source_name.to_string();
    let report_span = (source_id.clone(), span.clone());
    let mut builder = Report::build(ReportKind::Error, report_span.clone())
        .with_message(title)
        .with_label(
            Label::new(report_span)
                .with_message(label)
                .with_color(Color::Red),
        );

    if let Some(help) = help {
        builder = builder.with_help(help);
    }

    let report = builder.finish();
    if let Err(err) = report.eprint((source_id, Source::from(source.to_string()))) {
        eprintln!("{title}: {label}");
        if let Some(help) = help {
            eprintln!("help: {help}");
        }
        eprintln!("failed to render diagnostic: {err}");
    }
}

fn byte_span_to_char_span(source: &str, span: Range<usize>) -> Range<usize> {
    let start = source
        .char_indices()
        .take_while(|(idx, _)| *idx < span.start)
        .count();
    let mut end = source
        .char_indices()
        .take_while(|(idx, _)| *idx < span.end)
        .count();
    if end <= start {
        end = start + 1;
    }
    start..end
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
        let program = rseq::parse(src).unwrap();
        let bytecode = compile(&program).unwrap();

        let mut bus = MockBus::new();
        let mut vm = Vm::new(&mut bus, &bytecode);
        vm.run().unwrap();

        let ops = bus.ops();
        assert_eq!(ops.len(), 3);
        assert!(
            matches!(&ops[0], mock::BusOp::Write { addr: 0x40, data } if data == &[0x01, 0x02, 0x03])
        );
        assert!(matches!(&ops[1], mock::BusOp::Delay { us: 500 }));
        assert!(matches!(&ops[2], mock::BusOp::Write { addr: 0x100, data } if data == &[0xaa]));
    }

    #[test]
    fn test_decompile() {
        let src = r"
        write!(0x40, [0x01, 0x02, 0x03], 500);
        write!(0x100, 0xaa);
        ";
        let program = rseq::parse(src).unwrap();
        let bytecode = compile(&program).unwrap();

        let decompiled = decompile(&bytecode).unwrap();
        assert!(decompiled.contains("write!(0x40, [0x01, 0x02, 0x03], 500);"));
        assert!(decompiled.contains("write!(0x100, 0xaa);"));
    }

    #[test]
    fn test_update_rmw_on_mock_bus() {
        use rseq::compile_with_base;
        use std::path::PathBuf;

        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../qmi8660.yaml");
        let base = path.parent().unwrap();
        let src = r#"
        chip!("qmi8660.yaml");
        write!(UI.COMM_CTL, 0x2A);
        update!(UI.COMM_CTL.cs_pu_dis, 1);
        "#;
        let program = rseq::parse(src).unwrap();
        let bytecode = compile_with_base(&program, Some(base)).unwrap();

        let mut bus = MockBus::new();
        let mut vm = Vm::new(&mut bus, &bytecode);
        vm.run().unwrap();

        // 0x2A | bit0 = 0x2B
        assert_eq!(*bus.memory().get(&0x0B).unwrap(), 0x2B);
        let ops = bus.ops();
        assert!(matches!(&ops[1], mock::BusOp::Read { addr: 0x0B, .. }));
        assert!(matches!(&ops[2], mock::BusOp::Write { addr: 0x0B, data } if data == &[0x2B]));
    }

    #[test]
    fn test_byte_span_to_char_span_handles_non_ascii_prefix() {
        let src = "备注\nupdate!(UI.WHOAMI.value, 0x08);";
        let byte_start = src.find("update!").unwrap();
        let byte_end = src.len();
        let span = byte_span_to_char_span(src, byte_start..byte_end);
        let snippet: String = src.chars().skip(span.start).take(span.len()).collect();

        assert_eq!(snippet, "update!(UI.WHOAMI.value, 0x08);");
    }
}
