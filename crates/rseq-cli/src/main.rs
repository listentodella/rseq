use clap::Parser;
use rseq::{
    CompiledProgram, Manifest, ProgramUnit, compile_program_units, decompile, parse_detailed,
};
use rseq_vm::Vm;
use std::ops::Range;
use std::path::{Path, PathBuf};
pub mod mock;
use mock::MockBus;

struct ParsedSource {
    name: String,
    source: String,
    base_dir: Option<PathBuf>,
    program: rseq::Program,
}

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[arg(short, long)]
    file: Vec<String>,

    #[arg(short, long)]
    manifest: Option<String>,

    #[arg(short, long)]
    run: Vec<String>,

    #[arg(short, long)]
    decompile: bool,

    #[arg(short, long)]
    execute: bool,

    /// 模拟一次中断：--fire <pin>=<status>，status 为状态快照值（十六进制 0x.. 或十进制）。
    /// 可重复，例如 --fire int1=0x41 --fire int2=0x02。
    #[arg(long)]
    fire: Vec<String>,

    #[arg(short, long)]
    output: Option<String>,

    #[arg(short = 'x', long)]
    hex: Option<String>,

    /// 通过串口把字节码下发到真实 MCU 并收集回传轨迹:--serial /dev/ttyUSB0
    #[arg(long)]
    serial: Option<String>,

    /// 串口波特率(默认 115200)。
    #[arg(long, default_value_t = 115_200)]
    baud: u32,
}

fn main() {
    let cli = Cli::parse();

    if cli.decompile {
        if cli.manifest.is_some() || !cli.run.is_empty() {
            eprintln!("--decompile cannot be combined with --manifest or --run");
            std::process::exit(1);
        }

        let data = if let Some(hex_str) = &cli.hex {
            parse_hex_string(hex_str).expect("Failed to parse hex string")
        } else if cli.file.len() == 1 {
            let file = &cli.file[0];
            std::fs::read(file).expect("Failed to read bytecode file")
        } else {
            eprintln!(
                "Please provide either one bytecode file with --file or a hex string with --hex"
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
        let sources = load_sources(&cli).unwrap_or_else(|err| {
            eprintln!("{err}");
            std::process::exit(1);
        });

        println!("\nParsing rseq...");
        let mut parsed_sources = Vec::with_capacity(sources.len());
        for (name, source, base_dir) in sources {
            match parse_detailed(&source) {
                Ok(program) => {
                    println!("✓ Parsed {name} successfully");
                    parsed_sources.push(ParsedSource {
                        name,
                        source,
                        base_dir,
                        program,
                    });
                }
                Err(errors) => {
                    for error in errors {
                        emit_diagnostic(
                            &name,
                            &source,
                            error.span,
                            "could not parse rseq source",
                            &error.message,
                            Some("check the macro syntax and punctuation near this location"),
                        );
                    }
                    std::process::exit(1);
                }
            }
        }

        println!("\nCompiling to bytecode...");
        let program_units = parsed_sources
            .iter()
            .map(|source| ProgramUnit {
                program: &source.program,
                base_dir: source.base_dir.as_deref(),
            })
            .collect::<Vec<_>>();
        let CompiledProgram {
            main: bytecode,
            irqs,
            irq_bytecodes,
        } = match compile_program_units(&program_units) {
            Ok(compiled) => {
                let b = &compiled.main;
                println!("✓ Compiled successfully ({} bytes)", b.len());
                println!("Bytecode (vec): {:02x?}", b);
                let hex_str: String = b
                    .iter()
                    .map(|byte| format!("{:02x}", byte))
                    .collect::<Vec<_>>()
                    .join(" ");
                println!("Bytecode (hex): {}", hex_str);
                if let Some(path) = &cli.output {
                    std::fs::write(path, b).expect("Failed to write bytecode");
                    println!("Saved bytecode to {}", path);
                }
                compiled
            }
            Err(diag) => {
                let source = &parsed_sources[diag.unit];
                emit_diagnostic(
                    &source.name,
                    &source.source,
                    diag.span,
                    "could not compile rseq source",
                    &diag.message,
                    diag.help.as_deref(),
                );
                std::process::exit(1);
            }
        };

        if !irqs.is_empty() {
            println!(
                "\nInterrupt handlers (auto-response mode — MCU runs on every trigger):"
            );
            for vector in &irqs {
                println!(
                    "  irq!({}) — read {} byte(s) @ 0x{:08x}{}:",
                    vector.pin,
                    vector.snapshot_len,
                    vector.snapshot_addr,
                    if vector.read_clear {
                        " (read-clears)"
                    } else {
                        ""
                    }
                );
                for arm in &vector.arms {
                    println!("    on({}) when status & 0x{:x}:", arm.event, arm.mask);
                    println!("        inline body: {} statement(s)", arm.body.len());
                }
                if let Some(bc) = irq_bytecodes.get(&vector.pin) {
                    println!("    Segment bytecode: {} bytes", bc.len());
                }
            }
        }

        println!("\nStatements (in order):");
        let mut step = 1;
        for source in &parsed_sources {
            if parsed_sources.len() > 1 {
                println!("  Source: {}", source.name);
            }
            for stmt in &source.program.stmts {
                println!("  Step {}:", step);
                step += 1;
                match stmt {
                    rseq::Stmt::Chip { path } => {
                        println!("    Action: Load chip dictionary from {path}");
                    }
                    rseq::Stmt::Let { name, expr } => {
                        println!("    Action: Bind {} = {}", name, format_expr(expr));
                        if let rseq::Expr::Read {
                            delay_us: Some(d), ..
                        } = expr
                        {
                            println!("    Delay: {} μs after read", d);
                        }
                    }
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
                                        rseq::Value::Number(n) => {
                                            s.push_str(&format!("0x{:02x}", n))
                                        }
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
                    rseq::Stmt::Irq { pin, arms } => {
                        let events: Vec<&str> = arms.iter().map(|arm| arm.event.as_str()).collect();
                        println!(
                            "    Action: Interrupt handler on {pin} dispatching {} event(s): {}",
                            arms.len(),
                            events.join(", ")
                        );
                    }
                    rseq::Stmt::Wait { pin, timeout_ms } => {
                        println!("    Action: Wait for interrupt on {pin} ({timeout_ms} ms)");
                    }
                    rseq::Stmt::Repeat { count, body } => {
                        println!(
                            "    Action: Repeat body ({} statement(s)) {} time(s)",
                            body.len(),
                            count
                        );
                    }
                    rseq::Stmt::Read {
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
                            rseq::Value::Number(n) => format!("{}", n),
                            _ => "unknown".to_string(),
                        };
                        println!("    Action: Read {len_str} byte(s) from {addr_str}");
                        if let Some(d) = delay_us {
                            println!("    Delay: {} μs after read", d);
                        }
                    }
                    rseq::Stmt::If { cond, then, else_ } => {
                        println!(
                            "    Action: If ({}) → {} stmt(s)",
                            format_expr(cond),
                            then.len()
                        );
                        if !else_.is_empty() {
                            println!("      Else: {} stmt(s)", else_.len());
                        }
                    }
                    rseq::Stmt::Print { msg, vars } => {
                        if vars.is_empty() {
                            println!("    Action: Print {msg:?}");
                        } else {
                            println!("    Action: Print {msg:?} vars({})", vars.join(", "));
                        }
                    }
                }
            }
        }

        if cli.execute {
            println!("\nExecuting in MockBus...");
            let mut bus = MockBus::new();
            if !cli.fire.is_empty() {
                for spec in &cli.fire {
                    let (pin, status) = parse_fire_spec(spec).unwrap_or_else(|err| {
                        eprintln!("{err}");
                        std::process::exit(1);
                    });
                    let vector = irqs.iter().find(|irq| irq.pin == pin).unwrap_or_else(|| {
                        eprintln!("--fire references unknown irq pin '{pin}'");
                        std::process::exit(1);
                    });
                    let len = vector.snapshot_len as usize;
                    let bytes = status.to_le_bytes();
                    bus.load(vector.snapshot_addr, &bytes[..len]);
                    println!(
                        "Injected irq!({}) snapshot 0x{status:08x} at 0x{:08x} ({} byte(s))",
                        vector.pin, vector.snapshot_addr, vector.snapshot_len
                    );
                }
            }
            let mut vm = Vm::new(&mut bus, &bytecode);

            match vm.run() {
                Ok(_) => {
                    println!("✓ Execution completed successfully");

                    print_bus_ops(bus.ops());
                }
                Err(e) => {
                    eprintln!("Execution error: {e:?}");
                    std::process::exit(1);
                }
            }
        } else {
            println!("\nUse --execute to run in MockBus");
        }

        if let Some(path) = &cli.serial {
            #[cfg(feature = "serial")]
            run_over_serial(path, cli.baud, &bytecode, &irq_bytecodes);
            #[cfg(not(feature = "serial"))]
            {
                eprintln!(
                    "--serial {path} 需要以 `serial` feature 编译 \
                     (cargo run -p rseq-cli --features serial -- ... --serial ...)"
                );
                std::process::exit(2);
            }
        }
    }
}

/// 经串口把字节码下发到真实 MCU,用 HostLink 收集回传的 Trace 并打印。
#[cfg(feature = "serial")]
fn run_over_serial(path: &str, baud: u32, bytecode: &[u8], irq_bytecodes: &std::collections::HashMap<String, Vec<u8>>) {
    use rseq::link::HostLink;
    use rseq_link::wire::{SEG_KIND_MAIN, SEG_KIND_IRQ_INT1};

    println!("\nDispatching to MCU over serial ({path} @ {baud} baud)...");
    let transport = match rseq_link::SerialTransport::open(path, baud) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("open serial {path} failed: {e}");
            std::process::exit(1);
        }
    };
    let mut host = HostLink::new(transport);
    host.set_exec_timeout(std::time::Duration::from_secs(30));

    // 构造多段 LOAD
    let mut segments: Vec<(u8, &[u8])> = vec![(SEG_KIND_MAIN, bytecode)];
    if let Some(int1_bc) = irq_bytecodes.get("int1") {
        segments.push((SEG_KIND_IRQ_INT1, int1_bc.as_slice()));
        println!("  + INT1 interrupt handler ({} bytes)", int1_bc.len());
    }

    if let Err(e) = host.load_segments(&segments) {
        eprintln!("LOAD failed: {e}");
        std::process::exit(1);
    }
    println!("✓ Loaded {} byte(s)", bytecode.len());
    match host.exec() {
        Ok(res) => {
            println!("Exec status: {:?}", res.status);
            print_bus_ops(&res.traces);
        }
        Err(e) => {
            eprintln!("EXEC failed: {e}");
            std::process::exit(1);
        }
    }
}

/// 按执行顺序打印总线操作(MockBus 回放与串口回传 Trace 共用)。
fn print_bus_ops(ops: &[rseq::trace::BusOp]) {
    if ops.is_empty() {
        println!("No bus operations recorded");
        return;
    }
    println!("Bus operations (in execution order):");
    for (step, op) in ops.iter().enumerate() {
        match op {
            rseq::trace::BusOp::Write { addr, data } => {
                let bytes: String = data
                    .iter()
                    .map(|b| format!("0x{b:02x}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                println!("  Step {}: Write [{bytes}] → 0x{addr:08x}", step + 1);
            }
            rseq::trace::BusOp::Read { addr, data } => {
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
            rseq::trace::BusOp::Delay { us } => {
                println!("  Step {}: Delay {us} μs", step + 1);
            }
            rseq::trace::BusOp::Log { msg } => {
                println!("  Step {}: print {msg:?}", step + 1);
            }
            rseq::trace::BusOp::Irq { pin } => {
                println!("  Step {}: IRQ pin {pin} fired", step + 1);
            }
        }
    }
}

fn parse_fire_spec(spec: &str) -> Result<(String, u32), String> {
    let (pin, status) = spec
        .split_once('=')
        .ok_or_else(|| format!("invalid --fire '{spec}', expected <pin>=<status>"))?;
    if pin.is_empty() {
        return Err(format!("invalid --fire '{spec}', pin is empty"));
    }
    let status =
        parse_u32_arg(status).map_err(|e| format!("invalid --fire status in '{spec}': {e}"))?;
    Ok((pin.to_string(), status))
}

fn parse_u32_arg(text: &str) -> Result<u32, std::num::ParseIntError> {
    let trimmed = text.trim();
    if let Some(hex) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        u32::from_str_radix(hex, 16)
    } else {
        trimmed.parse::<u32>()
    }
}

fn load_sources(cli: &Cli) -> Result<Vec<(String, String, Option<PathBuf>)>, String> {
    if let Some(manifest_path) = &cli.manifest {
        if !cli.file.is_empty() {
            return Err("--manifest cannot be combined with --file".to_string());
        }

        let manifest_source = std::fs::read_to_string(manifest_path)
            .map_err(|e| format!("Failed to read manifest {manifest_path}: {e}"))?;
        let manifest = Manifest::parse(&manifest_source)
            .map_err(|e| format!("Failed to parse manifest {manifest_path}: {e}"))?;
        let manifest_base = Path::new(manifest_path)
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));

        let selected = manifest
            .selected_sequences(&cli.run)
            .map_err(|e| format!("Invalid manifest selection: {e}"))?;
        let mut sources = Vec::new();

        if let Some(chip) = &manifest.chip {
            let source = format!("chip!(\"{}\");\n", escape_rseq_string(chip));
            println!("Manifest chip source from {manifest_path}: {chip}");
            sources.push((
                format!("{manifest_path}#chip"),
                source,
                Some(manifest_base.clone()),
            ));
        }

        for sequence in selected {
            let path = manifest_base.join(&sequence.file);
            let display_name = sequence
                .name
                .as_deref()
                .map(|name| format!("{} ({name})", sequence.id))
                .unwrap_or_else(|| sequence.id.clone());
            let content = std::fs::read_to_string(&path)
                .map_err(|e| format!("Failed to read sequence {display_name}: {e}"))?;
            println!(
                "Original rseq content from {} [{}]:\n{}",
                path.display(),
                display_name,
                content
            );
            let base = path.parent().map(Path::to_path_buf);
            sources.push((path.display().to_string(), content, base));
        }

        return Ok(sources);
    }

    if !cli.run.is_empty() {
        return Err("--run requires --manifest".to_string());
    }

    let mut sources = Vec::new();
    if cli.file.is_empty() {
        let default = "write!(0x10, 0xaa, 100);".to_string();
        println!("Using default rseq content:\n{}", default);
        sources.push(("<default>".to_string(), default, None));
    } else {
        for file in &cli.file {
            let content = std::fs::read_to_string(file)
                .map_err(|e| format!("Failed to read rseq file {file}: {e}"))?;
            println!("Original rseq content from {file}:\n{}", content);
            let base = Path::new(file).parent().map(Path::to_path_buf);
            sources.push((file.clone(), content, base));
        }
    }

    Ok(sources)
}

fn escape_rseq_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn format_expr(expr: &rseq::Expr) -> String {
    match expr {
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
            let mut s = format!("read!({addr_str}, {len_str}");
            if let Some(d) = delay_us {
                s.push_str(&format!(", {d}"));
            }
            s.push(')');
            s
        }
        rseq::Expr::Number(n) => format!("0x{n:x}"),
        rseq::Expr::Ident(name) => name.clone(),
        rseq::Expr::Binary { op, lhs, rhs } => {
            format!("({} {} {})", format_expr(lhs), op, format_expr(rhs))
        }
        rseq::Expr::Unary { op, expr } => {
            format!("{}{}", op, format_expr(expr))
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
            matches!(&ops[0], rseq::trace::BusOp::Write { addr: 0x40, data } if data == &[0x01, 0x02, 0x03])
        );
        assert!(matches!(&ops[1], rseq::trace::BusOp::Delay { us: 500 }));
        assert!(
            matches!(&ops[2], rseq::trace::BusOp::Write { addr: 0x100, data } if data == &[0xaa])
        );
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
        assert!(matches!(
            &ops[1],
            rseq::trace::BusOp::Read { addr: 0x0B, .. }
        ));
        assert!(
            matches!(&ops[2], rseq::trace::BusOp::Write { addr: 0x0B, data } if data == &[0x2B])
        );
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
