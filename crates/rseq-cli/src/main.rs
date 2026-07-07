use clap::Parser;
use rseq::{
    CompiledProgram, Manifest, ProgramUnit, compile_program_units, decompile, parse_detailed,
};
use rseq_vm::Vm;
use std::fmt::Write as _;
use std::io::{Read as _, Write as _};
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

    /// 通过本地串口把字节码下发到真实 MCU 并收集回传轨迹:--serial /dev/ttyUSB0
    #[arg(long, conflicts_with = "tcp")]
    serial: Option<String>,

    /// 通过 TCP 字节流连接远端转发的 MCU CDC/UART:--tcp 10.2.8.42:5657
    #[arg(long, conflicts_with = "serial")]
    tcp: Option<String>,

    /// 只监听已运行 MCU 主动回传的 Trace/Report,可选解析 -f/--manifest 里的 report_format!,不发送 LOAD/EXEC/PING。
    #[arg(long, alias = "observe-only", alias = "rx-only")]
    watch: bool,

    /// 向 MCU 发送 Stop 控制帧，清除后台 IRQ handler/report 流；不下发新字节码。
    #[arg(long)]
    stop: bool,

    /// 通过已有 Reset 控制帧清空 MCU 已加载程序与 IRQ handler；不下发新字节码。
    #[arg(long)]
    reset_mcu: bool,

    /// 向 MCU 发送 Ping 控制帧探活；不下发新字节码。
    #[arg(long)]
    ping: bool,

    /// watch/replay 时保存收到的 report。扩展名 .csv 写解码样本，.bin 写可 replay 的二进制捕获。
    #[arg(long)]
    save: Option<PathBuf>,

    /// 从 --save 生成的 .bin 文件离线回放 report，不访问 MCU。
    #[arg(long)]
    replay: Option<PathBuf>,

    /// 每 N 条 report 打印一次累计健康统计；0 表示关闭。
    #[arg(long, default_value_t = 100)]
    stats_every: u64,

    /// 串口波特率(默认 115200)。
    #[arg(long, default_value_t = 115_200)]
    baud: u32,
}

#[derive(Debug, Clone, Copy)]
enum Endpoint<'a> {
    Serial(&'a str),
    Tcp(&'a str),
}

impl<'a> Endpoint<'a> {
    fn label(self, baud: u32) -> String {
        match self {
            Self::Serial(path) => format!("serial {path} @ {baud} baud"),
            Self::Tcp(addr) => format!("tcp {addr}"),
        }
    }
}

impl Cli {
    fn endpoint(&self) -> Option<Endpoint<'_>> {
        self.serial
            .as_deref()
            .map(Endpoint::Serial)
            .or_else(|| self.tcp.as_deref().map(Endpoint::Tcp))
    }
}

fn main() {
    let cli = Cli::parse();

    if let Some(path) = &cli.replay {
        let report_decoders = load_watch_report_decoders(&cli);
        let mut save = open_save_sink(cli.save.as_deref()).unwrap_or_else(|err| {
            eprintln!("{err}");
            std::process::exit(1);
        });
        run_replay(path, report_decoders, &mut save, cli.stats_every);
        return;
    }

    if cli.stop || cli.reset_mcu || cli.ping {
        let Some(endpoint) = cli.endpoint() else {
            eprintln!("--stop/--reset-mcu/--ping require --serial <port> or --tcp <host:port>");
            std::process::exit(2);
        };

        match endpoint {
            Endpoint::Serial(path) => {
                #[cfg(feature = "serial")]
                {
                    run_control_serial(path, cli.baud, cli.stop, cli.reset_mcu, cli.ping);
                    return;
                }
                #[cfg(not(feature = "serial"))]
                {
                    eprintln!(
                        "control commands over --serial {path} 需要以 `serial` feature 编译 \
                         (cargo run -p rseq-cli --features serial -- ...)"
                    );
                    std::process::exit(2);
                }
            }
            Endpoint::Tcp(addr) => {
                run_control_tcp(addr, cli.stop, cli.reset_mcu, cli.ping);
                return;
            }
        }
    }

    if cli.watch {
        let Some(endpoint) = cli.endpoint() else {
            eprintln!("--watch requires --serial <port> or --tcp <host:port>");
            std::process::exit(2);
        };

        if watch_ignores_control_options(&cli) {
            println!(
                "Watch mode: ignoring compile/execute control options and sending no control frames."
            );
        }
        let report_decoders = load_watch_report_decoders(&cli);
        let mut save = open_save_sink(cli.save.as_deref()).unwrap_or_else(|err| {
            eprintln!("{err}");
            std::process::exit(1);
        });
        match endpoint {
            Endpoint::Serial(path) => {
                #[cfg(feature = "serial")]
                {
                    run_watch_serial(path, cli.baud, report_decoders, &mut save, cli.stats_every);
                    return;
                }
                #[cfg(not(feature = "serial"))]
                {
                    eprintln!(
                        "--watch --serial {path} 需要以 `serial` feature 编译 \
                         (cargo run -p rseq-cli --features serial -- ... --watch --serial ...)"
                    );
                    std::process::exit(2);
                }
            }
            Endpoint::Tcp(addr) => {
                run_watch_tcp(addr, report_decoders, &mut save, cli.stats_every);
                return;
            }
        }
    }

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
        let report_decoders = report_decoders_from_sources(&parsed_sources).unwrap_or_else(|err| {
            eprintln!("{err}");
            std::process::exit(1);
        });

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
            println!("\nInterrupt handlers (auto-response mode — MCU runs on every trigger):");
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
                    rseq::Stmt::Bus { kind, arg } => match arg {
                        Some(arg) => {
                            println!("    Action: Select {kind} bus arg=0x{arg:x}");
                        }
                        None => {
                            println!("    Action: Select {kind} bus");
                        }
                    },
                    rseq::Stmt::BusProbe { kind, options } => {
                        let opts = options
                            .iter()
                            .map(|(name, value)| format!("{name}={}", format_value(value)))
                            .collect::<Vec<_>>()
                            .join(", ");
                        println!("    Action: Probe {kind} bus ({opts})");
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
                    rseq::Stmt::Report { kind, values } => {
                        let kind = match kind {
                            rseq::Value::Number(n) => format!("0x{n:x}"),
                            rseq::Value::Ident(name) => name.clone(),
                            _ => "unknown".to_string(),
                        };
                        let args = values
                            .iter()
                            .map(format_expr)
                            .collect::<Vec<_>>()
                            .join(", ");
                        if args.is_empty() {
                            println!("    Action: Report event kind={kind}");
                        } else {
                            println!("    Action: Report event kind={kind} args({args})");
                        }
                    }
                    rseq::Stmt::ReportFormat {
                        kind,
                        decoder,
                        options,
                    } => {
                        let kind = match kind {
                            rseq::Value::Number(n) => format!("0x{n:x}"),
                            rseq::Value::Ident(name) => name.clone(),
                            _ => "unknown".to_string(),
                        };
                        let opts = options
                            .iter()
                            .map(|(name, value)| format!("{name}={value}"))
                            .collect::<Vec<_>>()
                            .join(", ");
                        if opts.is_empty() {
                            println!("    Action: Report format kind={kind} decoder={decoder}");
                        } else {
                            println!(
                                "    Action: Report format kind={kind} decoder={decoder} options({opts})"
                            );
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

        if let Some(endpoint) = cli.endpoint() {
            let mut save = open_save_sink(cli.save.as_deref()).unwrap_or_else(|err| {
                eprintln!("{err}");
                std::process::exit(1);
            });
            match endpoint {
                Endpoint::Serial(path) => {
                    #[cfg(feature = "serial")]
                    {
                        run_over_serial(
                            path,
                            cli.baud,
                            &bytecode,
                            &irq_bytecodes,
                            report_decoders,
                            &mut save,
                            cli.stats_every,
                        );
                    }
                    #[cfg(not(feature = "serial"))]
                    {
                        eprintln!(
                            "--serial {path} 需要以 `serial` feature 编译 \
                             (cargo run -p rseq-cli --features serial -- ... --serial ...)"
                        );
                        std::process::exit(2);
                    }
                }
                Endpoint::Tcp(addr) => {
                    run_over_tcp(
                        addr,
                        &bytecode,
                        &irq_bytecodes,
                        report_decoders,
                        &mut save,
                        cli.stats_every,
                    );
                }
            }
        }
    }
}

/// 经串口把字节码下发到真实 MCU,用 HostLink 收集回传的 Trace 并打印。
#[cfg(feature = "serial")]
fn run_over_serial(
    path: &str,
    baud: u32,
    bytecode: &[u8],
    irq_bytecodes: &std::collections::HashMap<String, Vec<u8>>,
    report_decoders: ReportDecoderRegistry,
    save: &mut SaveSink,
    stats_every: u64,
) {
    let transport = match rseq_link::SerialTransport::open(path, baud) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("open serial {path} failed: {e}");
            std::process::exit(1);
        }
    };
    run_over_transport(
        &Endpoint::Serial(path).label(baud),
        transport,
        bytecode,
        irq_bytecodes,
        report_decoders,
        save,
        stats_every,
    );
}

fn run_over_tcp(
    addr: &str,
    bytecode: &[u8],
    irq_bytecodes: &std::collections::HashMap<String, Vec<u8>>,
    report_decoders: ReportDecoderRegistry,
    save: &mut SaveSink,
    stats_every: u64,
) {
    let transport = match rseq_link::TcpTransport::connect(addr) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("open tcp {addr} failed: {e}");
            std::process::exit(1);
        }
    };
    run_over_transport(
        &Endpoint::Tcp(addr).label(0),
        transport,
        bytecode,
        irq_bytecodes,
        report_decoders,
        save,
        stats_every,
    );
}

fn run_over_transport<T: rseq_link::Transport>(
    label: &str,
    transport: T,
    bytecode: &[u8],
    irq_bytecodes: &std::collections::HashMap<String, Vec<u8>>,
    report_decoders: ReportDecoderRegistry,
    save: &mut SaveSink,
    stats_every: u64,
) {
    use rseq::link::HostLink;
    use rseq_link::wire::{SEG_KIND_IRQ_INT1, SEG_KIND_MAIN};

    println!("\nDispatching to MCU over {label}...");
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

    if !irq_bytecodes.is_empty() {
        println!("\nObserving report events. Press Ctrl-C to stop.");
        observe_reports_forever(&mut host, &report_decoders, save, stats_every);
    }
}

#[cfg(feature = "serial")]
fn run_watch_serial(
    path: &str,
    baud: u32,
    report_decoders: ReportDecoderRegistry,
    save: &mut SaveSink,
    stats_every: u64,
) {
    let transport = match rseq_link::SerialTransport::open_observing(path, baud) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("open serial {path} failed: {e}");
            std::process::exit(1);
        }
    };
    run_watch_transport(
        &Endpoint::Serial(path).label(baud),
        transport,
        report_decoders,
        save,
        stats_every,
    );
}

fn run_watch_tcp(
    addr: &str,
    report_decoders: ReportDecoderRegistry,
    save: &mut SaveSink,
    stats_every: u64,
) {
    let transport = match rseq_link::TcpTransport::connect_observing(addr) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("open tcp {addr} failed: {e}");
            std::process::exit(1);
        }
    };
    run_watch_transport(
        &Endpoint::Tcp(addr).label(0),
        transport,
        report_decoders,
        save,
        stats_every,
    );
}

fn run_watch_transport<T: rseq_link::Transport>(
    label: &str,
    transport: T,
    report_decoders: ReportDecoderRegistry,
    save: &mut SaveSink,
    stats_every: u64,
) {
    use rseq::link::HostLink;

    println!("\nWatching MCU reports over {label}...");
    if !report_decoders.is_empty() {
        println!(
            "Loaded {} report decoder(s) from local DSL metadata.",
            report_decoders.len()
        );
    }
    println!("No LOAD/EXEC/PING frames will be sent. Press Ctrl-C to stop.");

    let mut host = HostLink::new(transport);
    observe_reports_forever(&mut host, &report_decoders, save, stats_every);
}

#[cfg(feature = "serial")]
fn run_control_serial(path: &str, baud: u32, stop: bool, reset_mcu: bool, ping: bool) {
    let transport = match rseq_link::SerialTransport::open(path, baud) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("open serial {path} failed: {e}");
            std::process::exit(1);
        }
    };
    run_control_transport(
        &Endpoint::Serial(path).label(baud),
        transport,
        stop,
        reset_mcu,
        ping,
    );
}

fn run_control_tcp(addr: &str, stop: bool, reset_mcu: bool, ping: bool) {
    let transport = match rseq_link::TcpTransport::connect(addr) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("open tcp {addr} failed: {e}");
            std::process::exit(1);
        }
    };
    run_control_transport(
        &Endpoint::Tcp(addr).label(0),
        transport,
        stop,
        reset_mcu,
        ping,
    );
}

fn run_control_transport<T: rseq_link::Transport>(
    label: &str,
    transport: T,
    stop: bool,
    reset_mcu: bool,
    ping: bool,
) {
    use rseq::link::HostLink;

    println!("\nSending control frame(s) over {label}...");
    let mut host = HostLink::new(transport);

    if ping {
        if let Err(e) = host.ping() {
            eprintln!("PING failed: {e}");
            std::process::exit(1);
        }
        println!("✓ PONG");
    }
    if stop {
        if let Err(e) = host.stop_reports() {
            eprintln!("STOP failed: {e}");
            std::process::exit(1);
        }
        println!("✓ Stopped background IRQ/report stream");
    }
    if reset_mcu {
        if let Err(e) = host.reset() {
            eprintln!("RESET failed: {e}");
            std::process::exit(1);
        }
        println!("✓ Reset MCU rseq program state");
    }
}

fn observe_reports_forever<T: rseq_link::Transport>(
    host: &mut rseq::link::HostLink<T>,
    report_decoders: &ReportDecoderRegistry,
    save: &mut SaveSink,
    stats_every: u64,
) {
    let mut state = ReportObserveState::default();
    loop {
        match host.observe_next_trace(std::time::Duration::from_secs(1)) {
            Ok(Some(op)) => {
                print_observed_report(&op, &mut state, report_decoders, save, stats_every);
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!("observe failed: {e}");
                std::process::exit(1);
            }
        }
    }
}

#[derive(Default)]
struct ReportObserveState {
    seq: u64,
    last_frame_id: Option<u32>,
    last_timestamp_us: Option<u64>,
    next_fifo_sample_index: u64,
    frame_gap_total: u64,
    frame_reset_count: u64,
    timestamp_rewind_count: u64,
    fifo_len_mismatch_count: u64,
    fifo_partial_count: u64,
    fifo_partial_bytes: u64,
    fifo_data_bytes: u64,
    fifo_sample_count: u64,
    reports_by_kind: std::collections::HashMap<u32, u64>,
}

struct ReportObserveInfo {
    seq: u64,
    meta: Option<rseq::trace::ReportMeta>,
    frame_gap: Option<u32>,
    frame_reset: Option<(u32, u32)>,
    dt_us: Option<u64>,
    timestamp_rewind: Option<(u64, u64)>,
}

impl ReportObserveState {
    fn next(&mut self, meta: Option<rseq::trace::ReportMeta>) -> ReportObserveInfo {
        self.seq += 1;

        let mut frame_gap = None;
        let mut frame_reset = None;
        let mut dt_us = None;
        let mut timestamp_rewind = None;

        if let Some(meta) = meta {
            if let Some(prev) = self.last_frame_id {
                if meta.frame_id == prev.wrapping_add(1) {
                    // expected path
                } else if meta.frame_id > prev {
                    frame_gap = Some(meta.frame_id - prev - 1);
                    self.frame_gap_total += u64::from(meta.frame_id - prev - 1);
                } else {
                    frame_reset = Some((prev, meta.frame_id));
                    self.frame_reset_count += 1;
                }
            }
            self.last_frame_id = Some(meta.frame_id);

            if meta.timestamp_valid() {
                if let Some(prev) = self.last_timestamp_us {
                    if meta.timestamp_us >= prev {
                        dt_us = Some(meta.timestamp_us - prev);
                    } else {
                        timestamp_rewind = Some((prev, meta.timestamp_us));
                        self.timestamp_rewind_count += 1;
                    }
                }
                self.last_timestamp_us = Some(meta.timestamp_us);
            }
        }

        ReportObserveInfo {
            seq: self.seq,
            meta,
            frame_gap,
            frame_reset,
            dt_us,
            timestamp_rewind,
        }
    }

    fn reserve_fifo_samples(&mut self, count: usize) -> u64 {
        let base = self.next_fifo_sample_index;
        self.next_fifo_sample_index = self.next_fifo_sample_index.saturating_add(count as u64);
        base
    }

    fn note_report_kind(&mut self, kind: u32) {
        *self.reports_by_kind.entry(kind).or_insert(0) += 1;
    }

    fn note_fifo_health(
        &mut self,
        fifo_len: Option<u32>,
        data_len: usize,
        decoded: Option<&I16LeFifoDecode>,
    ) {
        self.fifo_data_bytes = self.fifo_data_bytes.saturating_add(data_len as u64);
        if let Some(len) = fifo_len {
            if len as usize != data_len {
                self.fifo_len_mismatch_count += 1;
            }
        }
        if let Some(decoded) = decoded {
            self.fifo_sample_count = self
                .fifo_sample_count
                .saturating_add(decoded.samples.len() as u64);
            if decoded.trailing_bytes != 0 {
                self.fifo_partial_count += 1;
                self.fifo_partial_bytes += decoded.trailing_bytes as u64;
            }
        }
    }

    fn maybe_print_summary(&self, every: u64) {
        if every == 0 || self.seq == 0 || self.seq % every != 0 {
            return;
        }
        let mut kinds = self
            .reports_by_kind
            .iter()
            .map(|(kind, count)| format!("{}={count}", report_kind_label(*kind)))
            .collect::<Vec<_>>();
        kinds.sort();
        println!(
            "report health: total={} kinds=[{}] dropped={} frame_resets={} ts_rewinds={} fifo_bytes={} fifo_samples={} fifo_len_mismatch={} fifo_partial_reports={} fifo_partial_bytes={}",
            self.seq,
            kinds.join(", "),
            self.frame_gap_total,
            self.frame_reset_count,
            self.timestamp_rewind_count,
            self.fifo_data_bytes,
            self.fifo_sample_count,
            self.fifo_len_mismatch_count,
            self.fifo_partial_count,
            self.fifo_partial_bytes
        );
    }
}

fn print_observed_report(
    op: &rseq::trace::BusOp,
    state: &mut ReportObserveState,
    report_decoders: &ReportDecoderRegistry,
    save: &mut SaveSink,
    stats_every: u64,
) {
    if let rseq::trace::BusOp::Report { meta, kind, args } = op {
        let info = state.next(*meta);
        state.note_report_kind(*kind);
        if *kind == rseq::REPORT_KIND_FIFO_RAW {
            print_fifo_raw_report(&info, args, state, report_decoders.get(*kind));
        } else {
            print_named_report(&info, *kind, args);
        }
        if let Err(err) = save.write_report(&info, *kind, args, report_decoders.get(*kind)) {
            eprintln!("save failed: {err}");
        }
        state.maybe_print_summary(stats_every);
    }
}

fn print_fifo_raw_report(
    info: &ReportObserveInfo,
    args: &[rseq::trace::ReportArg],
    state: &mut ReportObserveState,
    decoder: Option<&ReportDecoder>,
) {
    let fifo_len = args.iter().find_map(|arg| match arg {
        rseq::trace::ReportArg::U32(v) => Some(*v),
        _ => None,
    });
    let data = args.iter().find_map(|arg| match arg {
        rseq::trace::ReportArg::Bytes(bytes) => Some(bytes.as_slice()),
        _ => None,
    });

    match data {
        Some(bytes) => {
            let hex = bytes
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<Vec<_>>()
                .join(" ");
            match decoder {
                Some(ReportDecoder::I16Le(decoder)) => {
                    let decoded = decode_i16_le_fifo_samples(bytes, decoder);
                    state.note_fifo_health(fifo_len, bytes.len(), Some(&decoded));
                    let sample_base = state.reserve_fifo_samples(decoded.samples.len());
                    let decode_summary = format_i16_le_fifo_decode(sample_base, &decoded, decoder);
                    let health = format_fifo_raw_health(fifo_len, bytes.len(), &decoded);
                    match fifo_len {
                        Some(len) => println!(
                            "FIFO_RAW #{}{}: fifo_len={len} data_len={} samples={}{} data=[{hex}]",
                            info.seq,
                            format_report_watch_meta(info),
                            bytes.len(),
                            decoded.samples.len(),
                            health
                        ),
                        None => println!(
                            "FIFO_RAW #{}{}: data_len={} samples={}{} data=[{hex}]",
                            info.seq,
                            format_report_watch_meta(info),
                            bytes.len(),
                            decoded.samples.len(),
                            health
                        ),
                    }
                    if !decode_summary.is_empty() {
                        println!("  {decode_summary}");
                    }
                }
                None => {
                    state.note_fifo_health(fifo_len, bytes.len(), None);
                    match fifo_len {
                        Some(len) => println!(
                            "FIFO_RAW #{}{}: fifo_len={len} data_len={} data=[{hex}]",
                            info.seq,
                            format_report_watch_meta(info),
                            bytes.len()
                        ),
                        None => println!(
                            "FIFO_RAW #{}{}: data_len={} data=[{hex}]",
                            info.seq,
                            format_report_watch_meta(info),
                            bytes.len()
                        ),
                    }
                }
            }
        }
        None => {
            println!(
                "FIFO_RAW #{}{}: missing raw bytes arg ({args:?})",
                info.seq,
                format_report_watch_meta(info)
            );
        }
    }
}

const FIFO_DECODE_PREVIEW_SAMPLES: usize = 8;
const DEFAULT_QMI8660_ACCEL_FULL_SCALE_G: f64 = 16.0;
const DEFAULT_QMI8660_GYRO_FULL_SCALE_DPS: f64 = 4096.0;
const DEFAULT_TEMP_LSB_PER_C: f64 = 1.0;
const DEFAULT_TEMP_OFFSET_C: f64 = 0.0;
const STANDARD_GRAVITY_MPS2: f64 = 9.80665;
const I16_FULL_SCALE_COUNTS: f64 = 32768.0;
const DEFAULT_REPORT_OUTPUT_MODE: ReportOutputMode = ReportOutputMode::PhysicalF32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReportOutputMode {
    PhysicalF32,
    RawI16,
}

impl ReportOutputMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::PhysicalF32 => "physical_f32",
            Self::RawI16 => "raw_i16",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum ReportDecoder {
    I16Le(I16LeReportDecoder),
}

#[derive(Debug, Clone, PartialEq)]
struct I16LeReportDecoder {
    label: String,
    fields: Vec<String>,
    accel_fields: Vec<String>,
    gyro_fields: Vec<String>,
    temp_field: Option<String>,
    accel_fs_g: f64,
    gyro_fs_dps: f64,
    temp_lsb_per_c: f64,
    temp_offset_c: f64,
    output: ReportOutputMode,
}

impl I16LeReportDecoder {
    fn sample_bytes(&self) -> usize {
        self.fields.len() * 2
    }

    fn validate(&self) -> Result<(), String> {
        if self.fields.is_empty() {
            return Err("i16_le report decoder requires non-empty fields".to_string());
        }
        let mut seen = std::collections::HashSet::new();
        for field in &self.fields {
            if !seen.insert(field) {
                return Err(format!("duplicate report field '{field}'"));
            }
        }
        for field in self.gyro_fields.iter().chain(self.accel_fields.iter()) {
            if !seen.contains(field) {
                return Err(format!(
                    "scaled report field '{field}' is not present in fields"
                ));
            }
        }
        if let Some(field) = &self.temp_field {
            if !seen.contains(field) {
                return Err(format!("temp field '{field}' is not present in fields"));
            }
            if !self.temp_lsb_per_c.is_finite() || self.temp_lsb_per_c <= 0.0 {
                return Err("temp_lsb_per_c must be greater than zero".to_string());
            }
            if !self.temp_offset_c.is_finite() {
                return Err("temp_offset_c must be finite".to_string());
            }
        }
        Ok(())
    }
}

impl ReportDecoder {
    fn validate(&self) -> Result<(), String> {
        match self {
            Self::I16Le(decoder) => decoder.validate(),
        }
    }
}

#[derive(Debug, Clone, Default)]
struct ReportDecoderRegistry {
    by_kind: std::collections::HashMap<u32, ReportDecoder>,
}

impl ReportDecoderRegistry {
    fn insert(&mut self, kind: u32, decoder: ReportDecoder) {
        self.by_kind.insert(kind, decoder);
    }

    fn get(&self, kind: u32) -> Option<&ReportDecoder> {
        self.by_kind.get(&kind)
    }

    fn is_empty(&self) -> bool {
        self.by_kind.is_empty()
    }

    fn len(&self) -> usize {
        self.by_kind.len()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct I16LeFieldValue {
    field_index: usize,
    value: i16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct I16LeFifoSample {
    values: Vec<I16LeFieldValue>,
}

impl I16LeFifoSample {
    fn value_by_name(&self, decoder: &I16LeReportDecoder, name: &str) -> Option<i16> {
        self.values
            .iter()
            .find(|value| decoder.fields[value.field_index] == name)
            .map(|value| value.value)
    }
}

fn accel_raw_to_m_s2(raw: i16, full_scale_g: f64) -> f64 {
    raw as f64 * full_scale_g * STANDARD_GRAVITY_MPS2 / I16_FULL_SCALE_COUNTS
}

fn gyro_raw_to_rad_s(raw: i16, full_scale_dps: f64) -> f64 {
    raw as f64 * full_scale_dps / I16_FULL_SCALE_COUNTS * std::f64::consts::PI / 180.0
}

fn temp_raw_to_c(raw: i16, lsb_per_c: f64, offset_c: f64) -> f64 {
    raw as f64 / lsb_per_c + offset_c
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct I16LeFifoDecode {
    samples: Vec<I16LeFifoSample>,
    trailing_bytes: usize,
}

fn decode_i16_le_fifo_samples(data: &[u8], decoder: &I16LeReportDecoder) -> I16LeFifoDecode {
    let sample_bytes = decoder.sample_bytes();
    if sample_bytes == 0 {
        return I16LeFifoDecode {
            samples: Vec::new(),
            trailing_bytes: data.len(),
        };
    }

    let mut samples = Vec::with_capacity(data.len() / sample_bytes);
    for chunk in data.chunks_exact(sample_bytes) {
        let values = chunk
            .chunks_exact(2)
            .enumerate()
            .map(|(field_index, bytes)| I16LeFieldValue {
                field_index,
                value: i16::from_le_bytes([bytes[0], bytes[1]]),
            })
            .collect();
        samples.push(I16LeFifoSample { values });
    }

    I16LeFifoDecode {
        samples,
        trailing_bytes: data.len() % sample_bytes,
    }
}

fn format_scaled_fields(
    sample: &I16LeFifoSample,
    decoder: &I16LeReportDecoder,
    fields: &[String],
    convert: impl Fn(i16) -> f64,
) -> String {
    let mut out = String::new();
    for (idx, field) in fields.iter().enumerate() {
        if idx != 0 {
            out.push(',');
        }
        match sample.value_by_name(decoder, field) {
            Some(raw) => {
                let value = convert(raw);
                let _ = write!(out, "{field}={value:.3}");
            }
            None => {
                let _ = write!(out, "{field}=missing");
            }
        }
    }
    out
}

fn format_raw_fields(
    sample: &I16LeFifoSample,
    decoder: &I16LeReportDecoder,
    excluded_fields: &[String],
) -> String {
    let mut out = String::new();
    let mut wrote = false;
    for value in &sample.values {
        let field = &decoder.fields[value.field_index];
        if excluded_fields.iter().any(|excluded| excluded == field) {
            continue;
        }
        if wrote {
            out.push(',');
        }
        wrote = true;
        let _ = write!(out, "{field}={}", value.value);
    }
    out
}

fn scaled_field_names(decoder: &I16LeReportDecoder) -> Vec<String> {
    let mut fields = decoder.gyro_fields.clone();
    for field in &decoder.accel_fields {
        if !fields.iter().any(|existing| existing == field) {
            fields.push(field.clone());
        }
    }
    if let Some(field) = &decoder.temp_field {
        if !fields.iter().any(|existing| existing == field) {
            fields.push(field.clone());
        }
    }
    fields
}

fn format_fifo_raw_health(
    fifo_len: Option<u32>,
    data_len: usize,
    decoded: &I16LeFifoDecode,
) -> String {
    let mut out = String::new();
    if let Some(fifo_len) = fifo_len {
        if fifo_len as usize != data_len {
            let _ = write!(out, " len_mismatch=status:{fifo_len},data:{data_len}");
        }
    }
    if decoded.trailing_bytes != 0 {
        let _ = write!(out, " partial_bytes={}", decoded.trailing_bytes);
    }
    out
}

fn format_i16_le_fifo_decode(
    sample_base: u64,
    decoded: &I16LeFifoDecode,
    decoder: &I16LeReportDecoder,
) -> String {
    if decoded.samples.is_empty() {
        return String::new();
    }

    let mut out = format!("decoded({} {}", decoder.label, decoder.output.as_str());
    if decoder.output == ReportOutputMode::PhysicalF32 {
        if !decoder.gyro_fields.is_empty() {
            out.push_str(" gyro_rad_s");
        }
        if !decoder.accel_fields.is_empty() {
            out.push_str(" acc_m_s2");
        }
        if decoder.temp_field.is_some() {
            out.push_str(" temp_c");
        }
    }
    out.push_str("): ");
    let scaled_fields = scaled_field_names(decoder);

    for (idx, sample) in decoded
        .samples
        .iter()
        .take(FIFO_DECODE_PREVIEW_SAMPLES)
        .enumerate()
    {
        if idx != 0 {
            out.push_str("; ");
        }
        let sample_index = sample_base + idx as u64;
        let _ = write!(out, "[{sample_index}]");
        match decoder.output {
            ReportOutputMode::PhysicalF32 => {
                if !decoder.gyro_fields.is_empty() {
                    let gyro = format_scaled_fields(sample, decoder, &decoder.gyro_fields, |raw| {
                        gyro_raw_to_rad_s(raw, decoder.gyro_fs_dps)
                    });
                    let _ = write!(out, " gyro=({gyro})");
                }
                if !decoder.accel_fields.is_empty() {
                    let accel =
                        format_scaled_fields(sample, decoder, &decoder.accel_fields, |raw| {
                            accel_raw_to_m_s2(raw, decoder.accel_fs_g)
                        });
                    let _ = write!(out, " acc=({accel})");
                }
                if let Some(field) = &decoder.temp_field {
                    match sample.value_by_name(decoder, field) {
                        Some(raw) => {
                            let temp_c =
                                temp_raw_to_c(raw, decoder.temp_lsb_per_c, decoder.temp_offset_c);
                            let _ = write!(out, " temp=({field}={temp_c:.3})");
                        }
                        None => {
                            let _ = write!(out, " temp=({field}=missing)");
                        }
                    }
                }
                let raw = format_raw_fields(sample, decoder, &scaled_fields);
                if !raw.is_empty() {
                    let _ = write!(out, " raw=({raw})");
                }
            }
            ReportOutputMode::RawI16 => {
                let raw = format_raw_fields(sample, decoder, &[]);
                let _ = write!(out, " raw=({raw})");
            }
        }
    }
    if decoded.samples.len() > FIFO_DECODE_PREVIEW_SAMPLES {
        let _ = write!(
            out,
            "; ... +{} samples",
            decoded.samples.len() - FIFO_DECODE_PREVIEW_SAMPLES
        );
    }
    out
}

fn print_named_report(info: &ReportObserveInfo, kind: u32, args: &[rseq::trace::ReportArg]) {
    let label = report_kind_label(kind);
    let vals = format_report_args(args);
    if vals.is_empty() {
        println!("{label} #{}{}", info.seq, format_report_watch_meta(info));
    } else {
        println!(
            "{label} #{}{}: {vals}",
            info.seq,
            format_report_watch_meta(info)
        );
    }
}

fn format_report_watch_meta(info: &ReportObserveInfo) -> String {
    let mut out = String::new();
    if let Some(meta) = info.meta {
        let _ = write!(out, " frame_id={}", meta.frame_id);
        if meta.timestamp_valid() {
            let _ = write!(out, " ts_us={}", meta.timestamp_us);
            if let Some(dt) = info.dt_us {
                let _ = write!(out, " dt_us={dt}");
            }
        }
    }
    if let Some(gap) = info.frame_gap {
        let _ = write!(out, " frame_gap={gap}");
    }
    if let Some((prev, current)) = info.frame_reset {
        let _ = write!(out, " frame_id_reset={prev}->{current}");
    }
    if let Some((prev, current)) = info.timestamp_rewind {
        let _ = write!(out, " ts_rewind={prev}->{current}");
    }
    out
}

const CAPTURE_MAGIC: &[u8] = b"RSEQCAP1\n";

enum SaveSink {
    None,
    Csv(CsvSave),
    Bin(BinSave),
}

impl SaveSink {
    fn write_report(
        &mut self,
        info: &ReportObserveInfo,
        kind: u32,
        args: &[rseq::trace::ReportArg],
        decoder: Option<&ReportDecoder>,
    ) -> Result<(), String> {
        match self {
            Self::None => Ok(()),
            Self::Csv(save) => save.write_report(info, kind, args, decoder),
            Self::Bin(save) => save.write_report(info, kind, args),
        }
    }
}

fn open_save_sink(path: Option<&Path>) -> Result<SaveSink, String> {
    let Some(path) = path else {
        return Ok(SaveSink::None);
    };
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("csv") => CsvSave::create(path).map(SaveSink::Csv),
        Some("bin") => BinSave::create(path).map(SaveSink::Bin),
        Some(ext) => Err(format!(
            "--save {} has unsupported extension '.{ext}', expected .csv or .bin",
            path.display()
        )),
        None => Err(format!(
            "--save {} must end with .csv or .bin",
            path.display()
        )),
    }
}

struct CsvSave {
    file: std::fs::File,
}

impl CsvSave {
    fn create(path: &Path) -> Result<Self, String> {
        let mut file = std::fs::File::create(path)
            .map_err(|e| format!("failed to create CSV {}: {e}", path.display()))?;
        writeln!(
            file,
            "seq,kind,frame_id,timestamp_us,dt_us,fifo_len,data_len,sample_index,field,raw_i16,value,unit,data_hex,args"
        )
        .map_err(|e| format!("failed to write CSV header {}: {e}", path.display()))?;
        println!("Saving decoded reports to CSV {}", path.display());
        Ok(Self { file })
    }

    fn write_report(
        &mut self,
        info: &ReportObserveInfo,
        kind: u32,
        args: &[rseq::trace::ReportArg],
        decoder: Option<&ReportDecoder>,
    ) -> Result<(), String> {
        if kind == rseq::REPORT_KIND_FIFO_RAW {
            if let (Some(bytes), Some(ReportDecoder::I16Le(decoder))) =
                (first_report_bytes(args), decoder)
            {
                return self.write_i16_le_fifo(info, kind, args, bytes, decoder);
            }
        }
        let data_hex = first_report_bytes(args).map(bytes_hex).unwrap_or_default();
        let args_summary = format_report_args(args);
        self.write_row(
            info,
            kind,
            first_report_u32(args),
            first_report_bytes(args).map_or(0, |bytes| bytes.len()),
            None,
            "",
            "",
            "",
            "",
            &data_hex,
            &args_summary,
        )
    }

    fn write_i16_le_fifo(
        &mut self,
        info: &ReportObserveInfo,
        kind: u32,
        args: &[rseq::trace::ReportArg],
        bytes: &[u8],
        decoder: &I16LeReportDecoder,
    ) -> Result<(), String> {
        let decoded = decode_i16_le_fifo_samples(bytes, decoder);
        let fifo_len = first_report_u32(args);
        if decoded.samples.is_empty() {
            return self.write_row(
                info,
                kind,
                fifo_len,
                bytes.len(),
                None,
                "",
                "",
                "",
                "",
                &bytes_hex(bytes),
                &format_report_args(args),
            );
        }

        for (sample_index, sample) in decoded.samples.iter().enumerate() {
            for value in &sample.values {
                let field = &decoder.fields[value.field_index];
                let (display, unit) = i16_field_display(value.value, field, decoder);
                self.write_row(
                    info,
                    kind,
                    fifo_len,
                    bytes.len(),
                    Some(sample_index as u64),
                    field,
                    &value.value.to_string(),
                    &format!("{display:.9}"),
                    unit,
                    "",
                    "",
                )?;
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn write_row(
        &mut self,
        info: &ReportObserveInfo,
        kind: u32,
        fifo_len: Option<u32>,
        data_len: usize,
        sample_index: Option<u64>,
        field: &str,
        raw_i16: &str,
        value: &str,
        unit: &str,
        data_hex: &str,
        args: &str,
    ) -> Result<(), String> {
        let (frame_id, timestamp_us) = match info.meta {
            Some(meta) => (
                meta.frame_id.to_string(),
                if meta.timestamp_valid() {
                    meta.timestamp_us.to_string()
                } else {
                    String::new()
                },
            ),
            None => (String::new(), String::new()),
        };
        let dt_us = info.dt_us.map(|v| v.to_string()).unwrap_or_default();
        let fifo_len = fifo_len.map(|v| v.to_string()).unwrap_or_default();
        let sample_index = sample_index.map(|v| v.to_string()).unwrap_or_default();
        let cells = [
            info.seq.to_string(),
            report_kind_label(kind),
            frame_id,
            timestamp_us,
            dt_us,
            fifo_len,
            data_len.to_string(),
            sample_index,
            field.to_string(),
            raw_i16.to_string(),
            value.to_string(),
            unit.to_string(),
            data_hex.to_string(),
            args.to_string(),
        ];
        writeln!(
            self.file,
            "{}",
            cells
                .iter()
                .map(|cell| csv_escape(cell))
                .collect::<Vec<_>>()
                .join(",")
        )
        .map_err(|e| format!("failed to write CSV row: {e}"))
    }
}

struct BinSave {
    file: std::fs::File,
}

impl BinSave {
    fn create(path: &Path) -> Result<Self, String> {
        let mut file = std::fs::File::create(path)
            .map_err(|e| format!("failed to create capture {}: {e}", path.display()))?;
        file.write_all(CAPTURE_MAGIC)
            .map_err(|e| format!("failed to write capture header {}: {e}", path.display()))?;
        println!("Saving binary report capture to {}", path.display());
        Ok(Self { file })
    }

    fn write_report(
        &mut self,
        info: &ReportObserveInfo,
        kind: u32,
        args: &[rseq::trace::ReportArg],
    ) -> Result<(), String> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&kind.to_le_bytes());
        match info.meta {
            Some(meta) => {
                payload.push(1);
                payload.push(meta.flags);
                payload.extend_from_slice(&meta.frame_id.to_le_bytes());
                payload.extend_from_slice(&meta.timestamp_us.to_le_bytes());
            }
            None => {
                payload.push(0);
                payload.push(0);
                payload.extend_from_slice(&0u32.to_le_bytes());
                payload.extend_from_slice(&0u64.to_le_bytes());
            }
        }
        payload.push(args.len() as u8);
        for arg in args {
            match arg {
                rseq::trace::ReportArg::U32(value) => {
                    payload.push(rseq_link::wire::REPORT_ARG_U32);
                    payload.extend_from_slice(&value.to_le_bytes());
                }
                rseq::trace::ReportArg::Bytes(bytes) => {
                    payload.push(rseq_link::wire::REPORT_ARG_BYTES);
                    payload.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
                    payload.extend_from_slice(bytes);
                }
            }
        }
        let len = payload.len() as u32;
        self.file
            .write_all(&len.to_le_bytes())
            .and_then(|_| self.file.write_all(&payload))
            .map_err(|e| format!("failed to write capture record: {e}"))
    }
}

fn run_replay(
    path: &Path,
    report_decoders: ReportDecoderRegistry,
    save: &mut SaveSink,
    stats_every: u64,
) {
    let reports = read_capture(path).unwrap_or_else(|err| {
        eprintln!("{err}");
        std::process::exit(1);
    });
    println!(
        "Replaying {} report(s) from {}",
        reports.len(),
        path.display()
    );
    let mut state = ReportObserveState::default();
    for (meta, kind, args) in reports {
        let op = rseq::trace::BusOp::Report { meta, kind, args };
        print_observed_report(&op, &mut state, &report_decoders, save, stats_every);
    }
    state.maybe_print_summary(1);
}

fn read_capture(
    path: &Path,
) -> Result<
    Vec<(
        Option<rseq::trace::ReportMeta>,
        u32,
        Vec<rseq::trace::ReportArg>,
    )>,
    String,
> {
    let mut file = std::fs::File::open(path)
        .map_err(|e| format!("failed to open capture {}: {e}", path.display()))?;
    let mut magic = vec![0u8; CAPTURE_MAGIC.len()];
    file.read_exact(&mut magic)
        .map_err(|e| format!("failed to read capture header {}: {e}", path.display()))?;
    if magic != CAPTURE_MAGIC {
        return Err(format!("{} is not an rseq report capture", path.display()));
    }

    let mut reports = Vec::new();
    loop {
        let mut len_bytes = [0u8; 4];
        match file.read_exact(&mut len_bytes) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(format!("failed to read capture length: {e}")),
        }
        let len = u32::from_le_bytes(len_bytes) as usize;
        let mut payload = vec![0u8; len];
        file.read_exact(&mut payload)
            .map_err(|e| format!("failed to read capture payload: {e}"))?;
        reports.push(decode_capture_record(&payload)?);
    }
    Ok(reports)
}

fn decode_capture_record(
    payload: &[u8],
) -> Result<
    (
        Option<rseq::trace::ReportMeta>,
        u32,
        Vec<rseq::trace::ReportArg>,
    ),
    String,
> {
    let mut pos = 0usize;
    let kind = take_u32(payload, &mut pos)?;
    let meta_present = take_u8(payload, &mut pos)? != 0;
    let flags = take_u8(payload, &mut pos)?;
    let frame_id = take_u32(payload, &mut pos)?;
    let timestamp_us = take_u64(payload, &mut pos)?;
    let meta = meta_present.then_some(rseq::trace::ReportMeta {
        flags,
        frame_id,
        timestamp_us,
    });
    let argc = take_u8(payload, &mut pos)? as usize;
    let mut args = Vec::with_capacity(argc);
    for _ in 0..argc {
        match take_u8(payload, &mut pos)? {
            rseq_link::wire::REPORT_ARG_U32 => {
                args.push(rseq::trace::ReportArg::U32(take_u32(payload, &mut pos)?));
            }
            rseq_link::wire::REPORT_ARG_BYTES => {
                let len = take_u32(payload, &mut pos)? as usize;
                let bytes = take_bytes(payload, &mut pos, len)?.to_vec();
                args.push(rseq::trace::ReportArg::Bytes(bytes));
            }
            tag => return Err(format!("invalid capture arg tag 0x{tag:02x}")),
        }
    }
    Ok((meta, kind, args))
}

fn take_u8(payload: &[u8], pos: &mut usize) -> Result<u8, String> {
    let bytes = take_bytes(payload, pos, 1)?;
    Ok(bytes[0])
}

fn take_u32(payload: &[u8], pos: &mut usize) -> Result<u32, String> {
    let bytes = take_bytes(payload, pos, 4)?;
    Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
}

fn take_u64(payload: &[u8], pos: &mut usize) -> Result<u64, String> {
    let bytes = take_bytes(payload, pos, 8)?;
    Ok(u64::from_le_bytes(bytes.try_into().unwrap()))
}

fn take_bytes<'a>(payload: &'a [u8], pos: &mut usize, len: usize) -> Result<&'a [u8], String> {
    let end = pos
        .checked_add(len)
        .ok_or_else(|| "capture record length overflow".to_string())?;
    if end > payload.len() {
        return Err("truncated capture record".to_string());
    }
    let bytes = &payload[*pos..end];
    *pos = end;
    Ok(bytes)
}

fn first_report_u32(args: &[rseq::trace::ReportArg]) -> Option<u32> {
    args.iter().find_map(|arg| match arg {
        rseq::trace::ReportArg::U32(v) => Some(*v),
        _ => None,
    })
}

fn first_report_bytes(args: &[rseq::trace::ReportArg]) -> Option<&[u8]> {
    args.iter().find_map(|arg| match arg {
        rseq::trace::ReportArg::Bytes(bytes) => Some(bytes.as_slice()),
        _ => None,
    })
}

fn bytes_hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn i16_field_display(raw: i16, field: &str, decoder: &I16LeReportDecoder) -> (f64, &'static str) {
    if decoder.output == ReportOutputMode::PhysicalF32 {
        if decoder.gyro_fields.iter().any(|name| name == field) {
            return (gyro_raw_to_rad_s(raw, decoder.gyro_fs_dps), "rad/s");
        }
        if decoder.accel_fields.iter().any(|name| name == field) {
            return (accel_raw_to_m_s2(raw, decoder.accel_fs_g), "m/s^2");
        }
        if decoder.temp_field.as_deref() == Some(field) {
            return (
                temp_raw_to_c(raw, decoder.temp_lsb_per_c, decoder.temp_offset_c),
                "C",
            );
        }
    }
    (raw as f64, "count")
}

fn csv_escape(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
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
            rseq::trace::BusOp::BusSelect { kind, arg } => {
                if *arg == 0 {
                    println!("  Step {}: Select {} bus", step + 1, kind.as_str());
                } else {
                    println!(
                        "  Step {}: Select {} bus arg=0x{arg:x}",
                        step + 1,
                        kind.as_str()
                    );
                }
            }
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
            rseq::trace::BusOp::Report { meta, kind, args } => {
                let label = report_kind_label(*kind);
                let vals = format_report_args(args);
                println!(
                    "  Step {}: Report {label}{} args [{vals}]",
                    step + 1,
                    format_report_meta(*meta)
                );
            }
        }
    }
}

fn report_kind_label(kind: u32) -> String {
    rseq::report_kind_name(kind).map_or_else(|| format!("kind=0x{kind:x}"), |name| name.to_string())
}

fn format_report_args(args: &[rseq::trace::ReportArg]) -> String {
    args.iter()
        .map(|arg| match arg {
            rseq::trace::ReportArg::U32(v) => {
                format!("u32=0x{v:08x} ({})", *v as i32)
            }
            rseq::trace::ReportArg::Bytes(bytes) => {
                let preview = bytes
                    .iter()
                    .take(16)
                    .map(|b| format!("{b:02x}"))
                    .collect::<Vec<_>>()
                    .join(" ");
                if bytes.len() > 16 {
                    format!("bytes[{}]=[{preview} ...]", bytes.len())
                } else {
                    format!("bytes[{}]=[{preview}]", bytes.len())
                }
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn format_report_meta(meta: Option<rseq::trace::ReportMeta>) -> String {
    let Some(meta) = meta else {
        return String::new();
    };
    let mut out = String::new();
    let _ = write!(out, " frame_id={}", meta.frame_id);
    if meta.timestamp_valid() {
        let _ = write!(out, " ts_us={}", meta.timestamp_us);
    }
    out
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

fn watch_ignores_control_options(cli: &Cli) -> bool {
    cli.decompile
        || cli.execute
        || !cli.fire.is_empty()
        || cli.hex.is_some()
        || cli.output.is_some()
}

fn load_watch_report_decoders(cli: &Cli) -> ReportDecoderRegistry {
    let sources = load_watch_sources(cli).unwrap_or_else(|err| {
        eprintln!("{err}");
        std::process::exit(1);
    });
    if sources.is_empty() {
        return ReportDecoderRegistry::default();
    }

    let parsed_sources = parse_sources_for_report_metadata(sources).unwrap_or_else(|err| {
        eprintln!("{err}");
        std::process::exit(1);
    });

    report_decoders_from_sources(&parsed_sources).unwrap_or_else(|err| {
        eprintln!("{err}");
        std::process::exit(1);
    })
}

fn load_watch_sources(cli: &Cli) -> Result<Vec<(String, String, Option<PathBuf>)>, String> {
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
            let base = path.parent().map(Path::to_path_buf);
            sources.push((path.display().to_string(), content, base));
        }

        return Ok(sources);
    }

    if !cli.run.is_empty() {
        return Err("--run requires --manifest".to_string());
    }

    let mut sources = Vec::new();
    for file in &cli.file {
        let content = std::fs::read_to_string(file)
            .map_err(|e| format!("Failed to read rseq file {file}: {e}"))?;
        let base = Path::new(file).parent().map(Path::to_path_buf);
        sources.push((file.clone(), content, base));
    }
    Ok(sources)
}

fn parse_sources_for_report_metadata(
    sources: Vec<(String, String, Option<PathBuf>)>,
) -> Result<Vec<ParsedSource>, String> {
    let mut parsed_sources = Vec::with_capacity(sources.len());
    for (name, source, base_dir) in sources {
        let program = parse_detailed(&source).map_err(|errors| {
            let error = errors
                .into_iter()
                .next()
                .expect("parse_detailed returned at least one diagnostic");
            format!(
                "could not parse report metadata in {name}: {} at bytes {}..{}",
                error.message, error.span.start, error.span.end
            )
        })?;
        parsed_sources.push(ParsedSource {
            name,
            source,
            base_dir,
            program,
        });
    }
    Ok(parsed_sources)
}

fn report_decoders_from_sources(sources: &[ParsedSource]) -> Result<ReportDecoderRegistry, String> {
    let mut registry = ReportDecoderRegistry::default();
    for source in sources {
        collect_report_decoders(&source.program.stmts, &mut registry)
            .map_err(|err| format!("{}: {err}", source.name))?;
    }
    Ok(registry)
}

fn collect_report_decoders(
    stmts: &[rseq::Stmt],
    registry: &mut ReportDecoderRegistry,
) -> Result<(), String> {
    for stmt in stmts {
        match stmt {
            rseq::Stmt::Bus { .. } | rseq::Stmt::BusProbe { .. } => {}
            rseq::Stmt::ReportFormat {
                kind,
                decoder,
                options,
            } => {
                let kind = report_kind_from_value(kind)?;
                let decoder = build_report_decoder(decoder, options)?;
                registry.insert(kind, decoder);
            }
            rseq::Stmt::Irq { arms, .. } => {
                for arm in arms {
                    collect_report_decoders(&arm.body, registry)?;
                }
            }
            rseq::Stmt::Repeat { body, .. } => collect_report_decoders(body, registry)?,
            rseq::Stmt::If { then, else_, .. } => {
                collect_report_decoders(then, registry)?;
                collect_report_decoders(else_, registry)?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn report_kind_from_value(kind: &rseq::Value) -> Result<u32, String> {
    match kind {
        rseq::Value::Number(n) => Ok(*n),
        rseq::Value::Ident(name) => {
            rseq::report_kind_value(name).ok_or_else(|| format!("unknown report kind '{name}'"))
        }
        _ => Err("report_format! kind must be a number or built-in report kind name".to_string()),
    }
}

fn report_option_number(
    decoder: &str,
    option: &str,
    value: &rseq::ReportOptionValue,
) -> Result<f64, String> {
    match value {
        rseq::ReportOptionValue::Number(value) => Ok(*value as f64),
        _ => Err(format!("{decoder} option '{option}' must be a number")),
    }
}

fn report_option_ident_array(
    decoder: &str,
    option: &str,
    value: &rseq::ReportOptionValue,
) -> Result<Vec<String>, String> {
    match value {
        rseq::ReportOptionValue::IdentArray(values) => Ok(values.clone()),
        _ => Err(format!(
            "{decoder} option '{option}' must be an identifier array"
        )),
    }
}

fn report_option_ident(
    decoder: &str,
    option: &str,
    value: &rseq::ReportOptionValue,
) -> Result<String, String> {
    match value {
        rseq::ReportOptionValue::Ident(value) => Ok(value.clone()),
        _ => Err(format!("{decoder} option '{option}' must be an identifier")),
    }
}

fn report_output_mode(
    decoder: &str,
    option: &str,
    value: &rseq::ReportOptionValue,
) -> Result<ReportOutputMode, String> {
    let value = report_option_ident(decoder, option, value)?;
    match value.as_str() {
        "physical_f32" => Ok(ReportOutputMode::PhysicalF32),
        "raw_i16" => Ok(ReportOutputMode::RawI16),
        _ => Err(format!(
            "{decoder} option '{option}' must be physical_f32 or raw_i16, got '{value}'"
        )),
    }
}

fn validated_report_decoder(decoder: ReportDecoder) -> Result<ReportDecoder, String> {
    decoder.validate()?;
    Ok(decoder)
}

fn make_i16_le_decoder(
    label: &str,
    fields: Vec<String>,
    gyro_fields: Vec<String>,
    accel_fields: Vec<String>,
    temp_field: Option<String>,
    accel_fs_g: f64,
    gyro_fs_dps: f64,
    temp_lsb_per_c: f64,
    temp_offset_c: f64,
    output: ReportOutputMode,
) -> Result<ReportDecoder, String> {
    validated_report_decoder(ReportDecoder::I16Le(I16LeReportDecoder {
        label: label.to_string(),
        fields,
        gyro_fields,
        accel_fields,
        temp_field,
        accel_fs_g,
        gyro_fs_dps,
        temp_lsb_per_c,
        temp_offset_c,
        output,
    }))
}

fn build_report_decoder(
    decoder: &str,
    options: &[(String, rseq::ReportOptionValue)],
) -> Result<ReportDecoder, String> {
    match decoder {
        "i16_le" => {
            let mut fields = None;
            let mut accel_fields = Vec::new();
            let mut gyro_fields = Vec::new();
            let mut temp_field = None;
            let mut accel_fs_g = DEFAULT_QMI8660_ACCEL_FULL_SCALE_G;
            let mut gyro_fs_dps = DEFAULT_QMI8660_GYRO_FULL_SCALE_DPS;
            let mut temp_lsb_per_c = DEFAULT_TEMP_LSB_PER_C;
            let mut temp_offset_c = DEFAULT_TEMP_OFFSET_C;
            let mut output = DEFAULT_REPORT_OUTPUT_MODE;
            for (name, value) in options {
                match name.as_str() {
                    "fields" => fields = Some(report_option_ident_array(decoder, name, value)?),
                    "accel_fields" => {
                        accel_fields = report_option_ident_array(decoder, name, value)?
                    }
                    "gyro_fields" => gyro_fields = report_option_ident_array(decoder, name, value)?,
                    "temp_field" => temp_field = Some(report_option_ident(decoder, name, value)?),
                    "accel_fs_g" => accel_fs_g = report_option_number(decoder, name, value)?,
                    "gyro_fs_dps" => gyro_fs_dps = report_option_number(decoder, name, value)?,
                    "temp_lsb_per_c" => {
                        temp_lsb_per_c = report_option_number(decoder, name, value)?
                    }
                    "temp_offset_c" => temp_offset_c = report_option_number(decoder, name, value)?,
                    "output" => output = report_output_mode(decoder, name, value)?,
                    _ => {
                        return Err(format!(
                            "unknown i16_le option '{name}', expected fields, accel_fields, gyro_fields, temp_field, accel_fs_g, gyro_fs_dps, temp_lsb_per_c, temp_offset_c, or output"
                        ));
                    }
                }
            }
            let fields =
                fields.ok_or_else(|| "i16_le report decoder requires fields: [...]".to_string())?;
            make_i16_le_decoder(
                "i16_le",
                fields,
                gyro_fields,
                accel_fields,
                temp_field,
                accel_fs_g,
                gyro_fs_dps,
                temp_lsb_per_c,
                temp_offset_c,
                output,
            )
        }
        "qmi8660_fifo6" => {
            let mut accel_fs_g = DEFAULT_QMI8660_ACCEL_FULL_SCALE_G;
            let mut gyro_fs_dps = DEFAULT_QMI8660_GYRO_FULL_SCALE_DPS;
            let mut output = DEFAULT_REPORT_OUTPUT_MODE;
            for (name, value) in options {
                match name.as_str() {
                    "accel_fs_g" => accel_fs_g = report_option_number(decoder, name, value)?,
                    "gyro_fs_dps" => gyro_fs_dps = report_option_number(decoder, name, value)?,
                    "output" => output = report_output_mode(decoder, name, value)?,
                    _ => {
                        return Err(format!(
                            "unknown qmi8660_fifo6 option '{name}', expected accel_fs_g, gyro_fs_dps, or output"
                        ));
                    }
                }
            }
            make_i16_le_decoder(
                "qmi8660_fifo6",
                ["gx", "gy", "gz", "ax", "ay", "az"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
                ["gx", "gy", "gz"].into_iter().map(str::to_string).collect(),
                ["ax", "ay", "az"].into_iter().map(str::to_string).collect(),
                None,
                accel_fs_g,
                gyro_fs_dps,
                DEFAULT_TEMP_LSB_PER_C,
                DEFAULT_TEMP_OFFSET_C,
                output,
            )
        }
        _ => Err(format!(
            "unknown report decoder '{decoder}', expected i16_le or qmi8660_fifo6"
        )),
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

fn format_value(value: &rseq::Value) -> String {
    match value {
        rseq::Value::Number(n) => format!("0x{n:x}"),
        rseq::Value::Ident(name) => name.clone(),
        rseq::Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(format_value)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        rseq::Value::FieldMap(entries) => format!(
            "{{{}}}",
            entries
                .iter()
                .map(|(name, value)| format!("{name}: 0x{value:x}"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
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

    fn test_decoder(
        fields: &[&str],
        gyro_fields: &[&str],
        accel_fields: &[&str],
    ) -> I16LeReportDecoder {
        test_decoder_with_output(
            fields,
            gyro_fields,
            accel_fields,
            DEFAULT_REPORT_OUTPUT_MODE,
        )
    }

    fn test_decoder_with_output(
        fields: &[&str],
        gyro_fields: &[&str],
        accel_fields: &[&str],
        output: ReportOutputMode,
    ) -> I16LeReportDecoder {
        I16LeReportDecoder {
            label: "i16_le".to_string(),
            fields: fields.iter().map(|field| (*field).to_string()).collect(),
            gyro_fields: gyro_fields
                .iter()
                .map(|field| (*field).to_string())
                .collect(),
            accel_fields: accel_fields
                .iter()
                .map(|field| (*field).to_string())
                .collect(),
            temp_field: None,
            accel_fs_g: DEFAULT_QMI8660_ACCEL_FULL_SCALE_G,
            gyro_fs_dps: DEFAULT_QMI8660_GYRO_FULL_SCALE_DPS,
            temp_lsb_per_c: DEFAULT_TEMP_LSB_PER_C,
            temp_offset_c: DEFAULT_TEMP_OFFSET_C,
            output,
        }
    }

    #[test]
    fn test_decode_i16_le_fifo_samples_physical_units() {
        let decoder = test_decoder(
            &["gx", "gy", "gz", "ax", "ay", "az"],
            &["gx", "gy", "gz"],
            &["ax", "ay", "az"],
        );
        let bytes = [
            0x01, 0x00, // gx = 1
            0xff, 0xff, // gy = -1
            0x34, 0x12, // gz = 0x1234
            0x00, 0x80, // ax = -32768
            0xff, 0x7f, // ay = 32767
            0x00, 0x00, // az = 0
        ];
        let decoded = decode_i16_le_fifo_samples(&bytes, &decoder);

        assert_eq!(decoded.trailing_bytes, 0);
        assert_eq!(
            decoded.samples,
            vec![I16LeFifoSample {
                values: vec![
                    I16LeFieldValue {
                        field_index: 0,
                        value: 1,
                    },
                    I16LeFieldValue {
                        field_index: 1,
                        value: -1,
                    },
                    I16LeFieldValue {
                        field_index: 2,
                        value: 0x1234,
                    },
                    I16LeFieldValue {
                        field_index: 3,
                        value: -32768,
                    },
                    I16LeFieldValue {
                        field_index: 4,
                        value: 32767,
                    },
                    I16LeFieldValue {
                        field_index: 5,
                        value: 0,
                    },
                ],
            }]
        );
        assert_eq!(
            format_i16_le_fifo_decode(10, &decoded, &decoder),
            "decoded(i16_le physical_f32 gyro_rad_s acc_m_s2): [10] gyro=(gx=0.002,gy=-0.002,gz=10.167) acc=(ax=-156.906,ay=156.902,az=0.000)"
        );
    }

    #[test]
    fn test_i16_le_decoder_raw_i16_output_formats_raw_counts() {
        let decoder = test_decoder_with_output(
            &["gx", "gy", "gz", "ax", "ay", "az"],
            &["gx", "gy", "gz"],
            &["ax", "ay", "az"],
            ReportOutputMode::RawI16,
        );
        let bytes = [
            0x01, 0x00, // gx = 1
            0xff, 0xff, // gy = -1
            0x34, 0x12, // gz = 0x1234
            0x00, 0x80, // ax = -32768
            0xff, 0x7f, // ay = 32767
            0x00, 0x00, // az = 0
        ];
        let decoded = decode_i16_le_fifo_samples(&bytes, &decoder);

        assert_eq!(
            format_i16_le_fifo_decode(0, &decoded, &decoder),
            "decoded(i16_le raw_i16): [0] raw=(gx=1,gy=-1,gz=4660,ax=-32768,ay=32767,az=0)"
        );
    }

    #[test]
    fn test_i16_le_decoder_uses_declared_field_order() {
        let decoder = test_decoder(
            &["ax", "ay", "az", "gx", "gy", "gz"],
            &["gx", "gy", "gz"],
            &["ax", "ay", "az"],
        );
        let bytes = [
            0x00, 0x08, // ax = 2048
            0x00, 0x00, // ay = 0
            0x00, 0x00, // az = 0
            0x01, 0x00, // gx = 1
            0x02, 0x00, // gy = 2
            0x03, 0x00, // gz = 3
        ];
        let decoded = decode_i16_le_fifo_samples(&bytes, &decoder);

        assert_eq!(
            format_i16_le_fifo_decode(0, &decoded, &decoder),
            "decoded(i16_le physical_f32 gyro_rad_s acc_m_s2): [0] gyro=(gx=0.002,gy=0.004,gz=0.007) acc=(ax=9.807,ay=0.000,az=0.000)"
        );
    }

    #[test]
    fn test_report_decoder_registry_comes_from_explicit_report_format_stmt() {
        let source = "report_format!(FIFO_RAW, i16_le, { fields: [gx, gy, gz, ax, ay, az], gyro_fields: [gx, gy, gz], accel_fields: [ax, ay, az], accel_fs_g: 16, gyro_fs_dps: 4096, output: physical_f32 });";
        let program = rseq::parse(source).unwrap();
        let parsed = ParsedSource {
            name: "test.rseq".to_string(),
            source: source.to_string(),
            base_dir: None,
            program,
        };

        let decoders = report_decoders_from_sources(&[parsed]).unwrap();

        assert_eq!(
            decoders.get(rseq::REPORT_KIND_FIFO_RAW),
            Some(&ReportDecoder::I16Le(test_decoder(
                &["gx", "gy", "gz", "ax", "ay", "az"],
                &["gx", "gy", "gz"],
                &["ax", "ay", "az"],
            )))
        );
    }

    #[test]
    fn test_legacy_qmi8660_report_decoder_still_maps_to_i16_le() {
        let source =
            "report_format!(FIFO_RAW, qmi8660_fifo6, { accel_fs_g: 16, gyro_fs_dps: 4096 });";
        let program = rseq::parse(source).unwrap();
        let parsed = ParsedSource {
            name: "test.rseq".to_string(),
            source: source.to_string(),
            base_dir: None,
            program,
        };

        let decoders = report_decoders_from_sources(&[parsed]).unwrap();

        assert_eq!(
            decoders.get(rseq::REPORT_KIND_FIFO_RAW),
            Some(&ReportDecoder::I16Le(I16LeReportDecoder {
                label: "qmi8660_fifo6".to_string(),
                fields: ["gx", "gy", "gz", "ax", "ay", "az"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
                gyro_fields: ["gx", "gy", "gz"].into_iter().map(str::to_string).collect(),
                accel_fields: ["ax", "ay", "az"].into_iter().map(str::to_string).collect(),
                temp_field: None,
                accel_fs_g: 16.0,
                gyro_fs_dps: 4096.0,
                temp_lsb_per_c: DEFAULT_TEMP_LSB_PER_C,
                temp_offset_c: DEFAULT_TEMP_OFFSET_C,
                output: DEFAULT_REPORT_OUTPUT_MODE,
            }))
        );
    }

    #[test]
    fn test_fifo_raw_health_reports_mismatch_and_partial_bytes() {
        let decoder = test_decoder(
            &["gx", "gy", "gz", "ax", "ay", "az"],
            &["gx", "gy", "gz"],
            &["ax", "ay", "az"],
        );
        let decoded = decode_i16_le_fifo_samples(&[0; 13], &decoder);

        assert_eq!(decoded.samples.len(), 1);
        assert_eq!(decoded.trailing_bytes, 1);
        assert_eq!(
            format_fifo_raw_health(Some(12), 13, &decoded),
            " len_mismatch=status:12,data:13 partial_bytes=1"
        );
    }

    fn temp_capture_path(ext: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "rseq-cli-test-{}-{unique}.{ext}",
            std::process::id()
        ))
    }

    #[test]
    fn test_binary_capture_round_trip() {
        let path = temp_capture_path("bin");
        let info = ReportObserveInfo {
            seq: 1,
            meta: Some(rseq::trace::ReportMeta {
                flags: rseq_link::wire::REPORT_FLAG_TIMESTAMP_VALID,
                frame_id: 7,
                timestamp_us: 1234,
            }),
            frame_gap: None,
            frame_reset: None,
            dt_us: None,
            timestamp_rewind: None,
        };
        let args = vec![
            rseq::trace::ReportArg::U32(2),
            rseq::trace::ReportArg::Bytes(vec![0xaa, 0xbb]),
        ];

        {
            let mut save = BinSave::create(&path).unwrap();
            save.write_report(&info, rseq::REPORT_KIND_FIFO_RAW, &args)
                .unwrap();
        }

        let reports = read_capture(&path).unwrap();
        std::fs::remove_file(&path).ok();

        assert_eq!(
            reports,
            vec![(
                info.meta,
                rseq::REPORT_KIND_FIFO_RAW,
                vec![
                    rseq::trace::ReportArg::U32(2),
                    rseq::trace::ReportArg::Bytes(vec![0xaa, 0xbb]),
                ],
            )]
        );
    }

    #[test]
    fn test_csv_capture_writes_long_field_rows() {
        let path = temp_capture_path("csv");
        let decoder = ReportDecoder::I16Le(test_decoder_with_output(
            &["gx", "ax"],
            &["gx"],
            &["ax"],
            ReportOutputMode::PhysicalF32,
        ));
        let info = ReportObserveInfo {
            seq: 3,
            meta: Some(rseq::trace::ReportMeta {
                flags: rseq_link::wire::REPORT_FLAG_TIMESTAMP_VALID,
                frame_id: 9,
                timestamp_us: 10_000,
            }),
            frame_gap: None,
            frame_reset: None,
            dt_us: Some(1000),
            timestamp_rewind: None,
        };
        let args = vec![
            rseq::trace::ReportArg::U32(4),
            rseq::trace::ReportArg::Bytes(vec![0x01, 0x00, 0x00, 0x08]),
        ];

        {
            let mut save = CsvSave::create(&path).unwrap();
            save.write_report(&info, rseq::REPORT_KIND_FIFO_RAW, &args, Some(&decoder))
                .unwrap();
        }

        let csv = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(&path).ok();

        assert!(csv.starts_with("seq,kind,frame_id,timestamp_us,dt_us"));
        assert!(csv.contains("FIFO_RAW"));
        assert!(csv.contains(",gx,1,"));
        assert!(csv.contains(",rad/s,"));
        assert!(csv.contains(",ax,2048,"));
        assert!(csv.contains(",m/s^2,"));
    }
}
