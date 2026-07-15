use std::collections::{BTreeMap, HashMap, VecDeque};
use std::error::Error;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, Sender},
};
use std::thread;
use std::time::{Duration, Instant};

use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Axis, Block, Borders, Cell, Chart, Clear, Dataset, GraphType, Paragraph, Row, Table, Tabs, Wrap,
};
use ratatui::{DefaultTerminal, Frame};
use rseq::trace::{BusOp, ReportArg, ReportMeta};
use rseq::{CompiledProgram, ProgramUnit};

const DEFAULT_QMI8660_ACCEL_FULL_SCALE_G: f64 = 16.0;
const DEFAULT_QMI8660_GYRO_FULL_SCALE_DPS: f64 = 4096.0;
const STANDARD_GRAVITY_MPS2: f64 = 9.80665;
const I16_FULL_SCALE_COUNTS: f64 = 32768.0;
const DEFAULT_TEMP_LSB_PER_C: f64 = 1.0;
const DEFAULT_TEMP_OFFSET_C: f64 = 0.0;
const MAX_TEXT_LINES: usize = 256;
const MAX_MOTION_EVENTS: usize = 1024;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Local .rseq file to load/execute on the MCU. In --watch mode it is used for host metadata only.
    #[arg(short, long)]
    file: Vec<PathBuf>,

    /// Chip YAML used for register-map metadata.
    #[arg(long)]
    chip: Vec<PathBuf>,

    /// Serial port used to load/execute an rseq file or watch an already running MCU.
    #[arg(long, conflicts_with = "tcp")]
    serial: Option<String>,

    /// TCP byte-stream endpoint forwarded from a remote CDC/UART, for example 10.2.8.42:5657.
    #[arg(long, conflicts_with = "serial")]
    tcp: Option<String>,

    /// Serial baud rate.
    #[arg(long, default_value_t = 230_400)]
    baud: u32,

    /// Force the synthetic data source. This is also the default when no endpoint is supplied.
    #[arg(long)]
    demo: bool,

    /// Only watch an already-running MCU; do not send LOAD/EXEC control frames.
    #[arg(long, alias = "observe-only", alias = "rx-only")]
    watch: bool,

    /// Apply runtime controls after connecting, for example accel_odr=200Hz.
    #[arg(long = "set-control", value_name = "NAME=VALUE")]
    set_control: Vec<String>,

    /// Number of IMU samples retained for the charts.
    #[arg(long, default_value_t = 512)]
    history: usize,
}

fn main() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();
    let result = run(cli);
    ratatui::restore();
    result
}

fn run(cli: Cli) -> Result<(), Box<dyn Error>> {
    let metadata = load_host_metadata(&cli.file, &cli.chip)?;
    let startup_controls =
        rseq_host::resolve_tuning_assignments(&metadata.tuning_catalog, &cli.set_control)?;
    let startup_program = serial_startup_program(&cli)?;
    let (tx, rx) = mpsc::channel();
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let source_label = start_source(
        &cli,
        metadata.report_decoders,
        startup_program,
        startup_controls,
        tx,
        cmd_rx,
        stop.clone(),
    );

    let mut terminal = ratatui::try_init()?;
    let mut app = App::new(
        source_label,
        cli.history.max(16),
        metadata.register_dump,
        metadata.tuning_catalog,
        Some(cmd_tx),
    );
    run_app(&mut terminal, &mut app, rx)?;
    stop.store(true, Ordering::Relaxed);
    Ok(())
}

fn start_source(
    cli: &Cli,
    report_decoders: ReportDecoderRegistry,
    startup_program: Option<CompiledProgram>,
    startup_controls: Vec<rseq_host::TuningAssignment>,
    tx: Sender<AppEvent>,
    cmd_rx: Receiver<SourceCommand>,
    stop: Arc<AtomicBool>,
) -> String {
    if cli.demo || (cli.serial.is_none() && cli.tcp.is_none()) {
        spawn_demo_source(tx, cmd_rx, stop, startup_controls);
        return "demo".to_string();
    }

    let mode = if startup_program.is_some() {
        "load+exec"
    } else {
        "watch"
    };
    if let Some(serial) = cli.serial.clone() {
        let label = format!("{serial} @ {} ({mode})", cli.baud);
        spawn_serial_source(
            serial,
            cli.baud,
            startup_program,
            startup_controls,
            report_decoders,
            tx,
            cmd_rx,
            stop,
        );
        return label;
    }

    let tcp = cli.tcp.clone().expect("--tcp checked above");
    let label = format!("{tcp} ({mode})");
    spawn_tcp_source(
        tcp,
        startup_program,
        startup_controls,
        report_decoders,
        tx,
        cmd_rx,
        stop,
    );
    label
}

fn run_app(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    rx: Receiver<AppEvent>,
) -> Result<(), Box<dyn Error>> {
    while app.running {
        for _ in 0..2048 {
            match rx.try_recv() {
                Ok(event) => app.apply(event),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    app.apply(AppEvent::Error("data source disconnected".to_string()));
                    break;
                }
            }
        }

        terminal.draw(|frame| render(frame, app))?;

        if event::poll(Duration::from_millis(33))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    app.handle_key(key.code);
                }
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Motion,
    Reports,
    Registers,
    Controls,
    Logs,
}

impl Tab {
    const ALL: [Self; 5] = [
        Self::Motion,
        Self::Reports,
        Self::Registers,
        Self::Controls,
        Self::Logs,
    ];

    fn title(self) -> &'static str {
        match self {
            Self::Motion => "Motion",
            Self::Reports => "Reports",
            Self::Registers => "Registers",
            Self::Controls => "Controls",
            Self::Logs => "Logs",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ImuSample {
    index: u64,
    timestamp_us: Option<u64>,
    acc: [f64; 3],
    gyro: [f64; 3],
}

#[derive(Debug, Clone)]
struct TuiMotionEvent {
    kind: rseq_host::SpecialEventKind,
    meta: Option<ReportMeta>,
    sample_index: u64,
    args_len: usize,
}

#[derive(Debug, Clone)]
struct RegisterValue {
    access: AccessKind,
    data: Vec<u8>,
}

#[derive(Debug, Clone, Default)]
struct HostMetadata {
    report_decoders: ReportDecoderRegistry,
    register_dump: RegisterDumpMap,
    tuning_catalog: rseq_host::TuningControlCatalog,
}

#[derive(Debug, Clone, Default)]
struct RegisterDumpMap {
    dumpable_by_addr: BTreeMap<u32, bool>,
    registers: Vec<RegisterInfo>,
}

#[derive(Debug, Clone)]
struct RegisterInfo {
    page: String,
    name: String,
    addr: u32,
    access: String,
    width: u32,
    desc: String,
    no_dump: bool,
    no_dump_reason: String,
    fields: Vec<FieldInfo>,
}

#[derive(Debug, Clone)]
struct FieldInfo {
    name: String,
    bit_hi: u8,
    bit_lo: u8,
    desc: String,
    event: Option<String>,
}

impl RegisterDumpMap {
    fn mark_register(&mut self, page: &str, reg: &rseq::Register) {
        self.mark_dumpability(reg.addr, reg.width, !reg.no_dump);
        if self.registers.iter().any(|existing| {
            existing.page == page && existing.name == reg.name && existing.addr == reg.addr
        }) {
            return;
        }

        self.registers.push(RegisterInfo {
            page: page.to_string(),
            name: reg.name.clone(),
            addr: reg.addr,
            access: reg.access.clone(),
            width: reg.width,
            desc: reg.desc.clone(),
            no_dump: reg.no_dump,
            no_dump_reason: reg.no_dump_reason.clone(),
            fields: reg
                .fields
                .iter()
                .map(|field| FieldInfo {
                    name: field.name.clone(),
                    bit_hi: field.bit_hi,
                    bit_lo: field.bit_lo,
                    desc: field.desc.clone(),
                    event: field.event.clone(),
                })
                .collect(),
        });
    }

    fn mark_dumpability(&mut self, addr: u32, width: u32, dumpable: bool) {
        for offset in 0..width.max(1) {
            let cell_addr = addr.saturating_add(offset);
            self.dumpable_by_addr
                .entry(cell_addr)
                .and_modify(|existing| *existing = *existing && dumpable)
                .or_insert(dumpable);
        }
    }

    fn is_no_dump(&self, addr: u32) -> bool {
        self.dumpable_by_addr.get(&addr).copied() == Some(false)
    }

    fn max_addr(&self) -> Option<u32> {
        self.dumpable_by_addr.keys().next_back().copied()
    }

    fn registers_for_addr(&self, addr: u32) -> Vec<&RegisterInfo> {
        self.registers
            .iter()
            .filter(|reg| {
                let end = reg.addr.saturating_add(reg.width.max(1));
                addr >= reg.addr && addr < end
            })
            .collect()
    }
}

#[derive(Debug, Clone, Copy)]
enum AccessKind {
    Read,
    Write,
}

#[derive(Debug)]
enum AppEvent {
    Sample {
        timestamp_us: Option<u64>,
        acc: [f64; 3],
        gyro: [f64; 3],
    },
    Register {
        addr: u32,
        access: AccessKind,
        data: Vec<u8>,
    },
    Report(String),
    SpecialEvent(rseq_host::SpecialEvent),
    ControlApplied {
        name: String,
        label: String,
        report_scale: Option<rseq_host::ReportScaleUpdate>,
    },
    ControlFinished {
        name: String,
    },
    Log(String),
    Error(String),
}

#[derive(Debug, Clone)]
enum SourceCommand {
    ReadRegister {
        addr: u32,
        len: u16,
        label: String,
    },
    WriteRegister {
        addr: u32,
        data: Vec<u8>,
        label: String,
    },
    SetControl(rseq_host::TuningAssignment),
}

#[derive(Debug, Clone)]
struct RegisterReadTarget {
    addr: u32,
    len: u16,
    label: String,
}

#[derive(Debug, Clone)]
struct RegisterWriteTarget {
    addr: u32,
    width: Option<usize>,
    label: String,
}

#[derive(Debug, Clone)]
struct RegisterWriteDialog {
    target: RegisterWriteTarget,
    input: String,
    error: Option<String>,
}

#[derive(Debug, Clone)]
struct TuningDialog {
    control: rseq_host::TuningControl,
    input: String,
    error: Option<String>,
}

struct App {
    running: bool,
    tab: usize,
    source_label: String,
    source_commands: Option<Sender<SourceCommand>>,
    history: usize,
    samples: VecDeque<ImuSample>,
    register_dump: RegisterDumpMap,
    registers: BTreeMap<u32, RegisterValue>,
    selected_register_addr: u32,
    register_detail_open: bool,
    register_write_dialog: Option<RegisterWriteDialog>,
    tuning_catalog: rseq_host::TuningControlCatalog,
    selected_control_index: usize,
    tuning_dialog: Option<TuningDialog>,
    control_busy: Option<String>,
    reports: VecDeque<String>,
    motion_events: VecDeque<TuiMotionEvent>,
    motion_event_counts: [u64; 3],
    logs: VecDeque<String>,
    sample_counter: u64,
    report_counter: u64,
    error_counter: u64,
    started_at: Instant,
}

impl App {
    fn new(
        source_label: String,
        history: usize,
        register_dump: RegisterDumpMap,
        tuning_catalog: rseq_host::TuningControlCatalog,
        source_commands: Option<Sender<SourceCommand>>,
    ) -> Self {
        Self {
            running: true,
            tab: 0,
            source_label,
            source_commands,
            history,
            samples: VecDeque::with_capacity(history),
            register_dump,
            registers: BTreeMap::new(),
            selected_register_addr: 0,
            register_detail_open: false,
            register_write_dialog: None,
            tuning_catalog,
            selected_control_index: 0,
            tuning_dialog: None,
            control_busy: None,
            reports: VecDeque::with_capacity(MAX_TEXT_LINES),
            motion_events: VecDeque::with_capacity(MAX_MOTION_EVENTS),
            motion_event_counts: [0; 3],
            logs: VecDeque::with_capacity(MAX_TEXT_LINES),
            sample_counter: 0,
            report_counter: 0,
            error_counter: 0,
            started_at: Instant::now(),
        }
    }

    fn selected_tab(&self) -> Tab {
        Tab::ALL[self.tab]
    }

    fn handle_key(&mut self, code: KeyCode) {
        if self.handle_tuning_dialog_key(code) {
            return;
        }

        if self.handle_register_write_dialog_key(code) {
            return;
        }

        if self.selected_tab() == Tab::Registers && self.handle_register_key(code) {
            return;
        }
        if self.selected_tab() == Tab::Controls && self.handle_control_key(code) {
            return;
        }

        match code {
            KeyCode::Char('q') | KeyCode::Esc => self.running = false,
            KeyCode::Tab | KeyCode::Right => self.tab = (self.tab + 1) % Tab::ALL.len(),
            KeyCode::BackTab | KeyCode::Left => {
                self.tab = (self.tab + Tab::ALL.len() - 1) % Tab::ALL.len();
            }
            KeyCode::Char('1') => self.tab = 0,
            KeyCode::Char('2') => self.tab = 1,
            KeyCode::Char('3') => self.tab = 2,
            KeyCode::Char('4') => self.tab = 3,
            KeyCode::Char('5') => self.tab = 4,
            _ => {}
        }
    }

    fn handle_tuning_dialog_key(&mut self, code: KeyCode) -> bool {
        if self.tuning_dialog.is_none() {
            return false;
        }

        match code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.tuning_dialog = None;
                true
            }
            KeyCode::Enter => {
                self.submit_tuning_dialog();
                true
            }
            KeyCode::Backspace => {
                if let Some(dialog) = &mut self.tuning_dialog {
                    dialog.input.pop();
                    dialog.error = None;
                }
                true
            }
            KeyCode::Delete => {
                if let Some(dialog) = &mut self.tuning_dialog {
                    dialog.input.clear();
                    dialog.error = None;
                }
                true
            }
            KeyCode::Char(ch) if is_tuning_input_char(ch) => {
                if let Some(dialog) = &mut self.tuning_dialog {
                    dialog.input.push(ch);
                    dialog.error = None;
                }
                true
            }
            _ => true,
        }
    }

    fn handle_register_write_dialog_key(&mut self, code: KeyCode) -> bool {
        if self.register_write_dialog.is_none() {
            return false;
        }

        match code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.register_write_dialog = None;
                true
            }
            KeyCode::Enter => {
                self.submit_register_write_dialog();
                true
            }
            KeyCode::Backspace => {
                if let Some(dialog) = &mut self.register_write_dialog {
                    dialog.input.pop();
                    dialog.error = None;
                }
                true
            }
            KeyCode::Delete => {
                if let Some(dialog) = &mut self.register_write_dialog {
                    dialog.input.clear();
                    dialog.error = None;
                }
                true
            }
            KeyCode::Char(ch) if is_register_write_input_char(ch) => {
                if let Some(dialog) = &mut self.register_write_dialog {
                    dialog.input.push(ch);
                    dialog.error = None;
                }
                true
            }
            _ => true,
        }
    }

    fn handle_register_key(&mut self, code: KeyCode) -> bool {
        match code {
            KeyCode::Esc | KeyCode::Char('q') if self.register_detail_open => {
                self.register_detail_open = false;
                true
            }
            KeyCode::Enter | KeyCode::Char('i') => {
                self.register_detail_open = !self.register_detail_open;
                true
            }
            KeyCode::Char('r') => {
                self.request_selected_register_dump();
                true
            }
            KeyCode::Char('w') => {
                self.open_selected_register_write_dialog();
                true
            }
            KeyCode::Left => {
                self.move_register_selection(-1);
                true
            }
            KeyCode::Right => {
                self.move_register_selection(1);
                true
            }
            KeyCode::Up => {
                self.move_register_selection(-16);
                true
            }
            KeyCode::Down => {
                self.move_register_selection(16);
                true
            }
            KeyCode::PageUp => {
                self.move_register_selection(-0x100);
                true
            }
            KeyCode::PageDown => {
                self.move_register_selection(0x100);
                true
            }
            KeyCode::Home => {
                self.selected_register_addr = 0;
                true
            }
            KeyCode::End => {
                self.selected_register_addr = register_grid_max_addr(self).max(0x0f);
                true
            }
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Char('1'..='4') | KeyCode::Char('q') => {
                false
            }
            _ => false,
        }
    }

    fn handle_control_key(&mut self, code: KeyCode) -> bool {
        match code {
            KeyCode::Up => {
                self.move_control_selection(-1);
                true
            }
            KeyCode::Down => {
                self.move_control_selection(1);
                true
            }
            KeyCode::Home => {
                self.selected_control_index = 0;
                true
            }
            KeyCode::End => {
                self.selected_control_index =
                    self.tuning_catalog.controls().len().saturating_sub(1);
                true
            }
            KeyCode::Char('r') => {
                self.request_selected_control_read();
                true
            }
            KeyCode::Char('[') => {
                self.cycle_selected_control(-1);
                true
            }
            KeyCode::Char(']') => {
                self.cycle_selected_control(1);
                true
            }
            KeyCode::Enter | KeyCode::Char('e') | KeyCode::Char('w') => {
                self.open_tuning_dialog();
                true
            }
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Char('1'..='5') | KeyCode::Char('q') => {
                false
            }
            _ => false,
        }
    }

    fn move_register_selection(&mut self, delta: i32) {
        let max_addr = register_grid_max_addr(self).max(0x0f);
        let next = if delta.is_negative() {
            self.selected_register_addr
                .saturating_sub(delta.unsigned_abs())
        } else {
            self.selected_register_addr.saturating_add(delta as u32)
        };
        self.selected_register_addr = next.min(max_addr);
    }

    fn move_control_selection(&mut self, delta: i32) {
        let len = self.tuning_catalog.controls().len();
        if len == 0 {
            self.selected_control_index = 0;
            return;
        }
        let current = self.selected_control_index.min(len - 1);
        self.selected_control_index = if delta.is_negative() {
            current.saturating_sub(delta.unsigned_abs() as usize)
        } else {
            current.saturating_add(delta as usize).min(len - 1)
        };
    }

    fn selected_control(&self) -> Option<&rseq_host::TuningControl> {
        self.tuning_catalog
            .controls()
            .get(self.selected_control_index)
    }

    fn selected_control_bytes(&self) -> Option<Vec<u8>> {
        let control = self.selected_control()?;
        let width = control.width.max(1) as usize;
        let mut bytes = Vec::with_capacity(width);
        for offset in 0..width {
            bytes.push(
                *self
                    .registers
                    .get(&(control.addr + offset as u32))?
                    .data
                    .first()?,
            );
        }
        Some(bytes)
    }

    fn selected_control_value(&self) -> Option<u32> {
        let control = self.selected_control()?;
        let bytes = self.selected_control_bytes()?;
        rseq_host::tuning_control_value_from_bytes(control, &bytes)
    }

    fn request_selected_control_read(&mut self) {
        let Some(control) = self.selected_control().cloned() else {
            push_bounded(
                &mut self.logs,
                "no tuning controls loaded".to_string(),
                MAX_TEXT_LINES,
            );
            return;
        };
        let Some(tx) = &self.source_commands else {
            push_bounded(
                &mut self.logs,
                "control read unavailable: no active source command channel".to_string(),
                MAX_TEXT_LINES,
            );
            return;
        };
        let len = match u16::try_from(control.width.max(1)) {
            Ok(len) => len,
            Err(_) => {
                push_bounded(
                    &mut self.logs,
                    format!("{} width {} exceeds u16::MAX", control.name, control.width),
                    MAX_TEXT_LINES,
                );
                return;
            }
        };
        match tx.send(SourceCommand::ReadRegister {
            addr: control.addr,
            len,
            label: control.register_name.clone(),
        }) {
            Ok(()) => push_bounded(
                &mut self.logs,
                format!(
                    "control read {} @ 0x{:02x} len={}",
                    control.name, control.addr, len
                ),
                MAX_TEXT_LINES,
            ),
            Err(_) => push_bounded(
                &mut self.logs,
                "control read failed: source thread is not running".to_string(),
                MAX_TEXT_LINES,
            ),
        }
    }

    fn open_tuning_dialog(&mut self) {
        let Some(control) = self.selected_control().cloned() else {
            push_bounded(
                &mut self.logs,
                "no tuning controls loaded".to_string(),
                MAX_TEXT_LINES,
            );
            return;
        };
        let input = self
            .selected_control_value()
            .map(|value| rseq_host::tuning_control_value_label(&control, value))
            .unwrap_or_default();
        self.tuning_dialog = Some(TuningDialog {
            control,
            input,
            error: None,
        });
    }

    fn submit_tuning_dialog(&mut self) {
        let Some(dialog) = &mut self.tuning_dialog else {
            return;
        };
        let value = match rseq_host::parse_tuning_control_value(&dialog.control, &dialog.input) {
            Ok(value) => value,
            Err(err) => {
                dialog.error = Some(err);
                return;
            }
        };
        let assignment = rseq_host::TuningAssignment {
            control: dialog.control.clone(),
            value,
        };
        if self.send_control_assignment(assignment) {
            self.tuning_dialog = None;
        }
    }

    fn cycle_selected_control(&mut self, delta: i32) {
        let Some(control) = self.selected_control().cloned() else {
            push_bounded(
                &mut self.logs,
                "no tuning controls loaded".to_string(),
                MAX_TEXT_LINES,
            );
            return;
        };
        if control.options.is_empty() {
            self.open_tuning_dialog();
            return;
        }
        let current_value = self.selected_control_value();
        let current_index = current_value.and_then(|value| {
            control
                .options
                .iter()
                .position(|option| option.value == value)
        });
        let len = control.options.len();
        let next_index = match (current_index, delta.is_negative()) {
            (Some(index), true) => (index + len - 1) % len,
            (Some(index), false) => (index + 1) % len,
            (None, true) => len - 1,
            (None, false) => 0,
        };
        let value = control.options[next_index].value;
        self.send_control_assignment(rseq_host::TuningAssignment { control, value });
    }

    fn send_control_assignment(&mut self, assignment: rseq_host::TuningAssignment) -> bool {
        if let Some(name) = &self.control_busy {
            push_bounded(
                &mut self.logs,
                format!("control transaction for {name} is still running"),
                MAX_TEXT_LINES,
            );
            return false;
        }
        let Some(tx) = &self.source_commands else {
            push_bounded(
                &mut self.logs,
                "control write unavailable: no active source command channel".to_string(),
                MAX_TEXT_LINES,
            );
            return false;
        };
        let label = rseq_host::tuning_control_value_label(&assignment.control, assignment.value);
        match tx.send(SourceCommand::SetControl(assignment.clone())) {
            Ok(()) => {
                self.control_busy = Some(assignment.control.name.clone());
                push_bounded(
                    &mut self.logs,
                    format!("set request {}={label}", assignment.control.name),
                    MAX_TEXT_LINES,
                );
                true
            }
            Err(_) => {
                push_bounded(
                    &mut self.logs,
                    "control write failed: source thread is not running".to_string(),
                    MAX_TEXT_LINES,
                );
                false
            }
        }
    }

    fn request_selected_register_dump(&mut self) {
        let target = match self.selected_register_read_target() {
            Ok(target) => target,
            Err(reason) => {
                push_bounded(&mut self.logs, reason, MAX_TEXT_LINES);
                return;
            }
        };

        let Some(tx) = &self.source_commands else {
            push_bounded(
                &mut self.logs,
                "register dump unavailable: no active source command channel".to_string(),
                MAX_TEXT_LINES,
            );
            return;
        };

        match tx.send(SourceCommand::ReadRegister {
            addr: target.addr,
            len: target.len,
            label: target.label.clone(),
        }) {
            Ok(()) => push_bounded(
                &mut self.logs,
                format!(
                    "dump request {} @ 0x{:02x} len={}",
                    target.label, target.addr, target.len
                ),
                MAX_TEXT_LINES,
            ),
            Err(_) => push_bounded(
                &mut self.logs,
                "register dump failed: source thread is not running".to_string(),
                MAX_TEXT_LINES,
            ),
        }
    }

    fn open_selected_register_write_dialog(&mut self) {
        let target = match self.selected_register_write_target() {
            Ok(target) => target,
            Err(reason) => {
                push_bounded(&mut self.logs, reason, MAX_TEXT_LINES);
                return;
            }
        };

        let input = self
            .registers
            .get(&target.addr)
            .map(|value| hex_bytes(&value.data))
            .unwrap_or_default();
        self.register_write_dialog = Some(RegisterWriteDialog {
            target,
            input,
            error: None,
        });
    }

    fn submit_register_write_dialog(&mut self) {
        let Some(dialog) = &mut self.register_write_dialog else {
            return;
        };

        let data = match parse_register_write_bytes(&dialog.input) {
            Ok(data) => data,
            Err(err) => {
                dialog.error = Some(err);
                return;
            }
        };

        if let Some(width) = dialog.target.width {
            if data.len() != width {
                dialog.error = Some(format!("expected {width} byte(s), got {}", data.len()));
                return;
            }
        }

        if data.len() > rseq_link::wire::CONTROL_MAX_WRITE_LEN {
            dialog.error = Some(format!(
                "write length {} exceeds limit {}",
                data.len(),
                rseq_link::wire::CONTROL_MAX_WRITE_LEN
            ));
            return;
        }

        let target = dialog.target.clone();
        let Some(tx) = &self.source_commands else {
            dialog.error = Some("write unavailable: no active source command channel".to_string());
            return;
        };

        match tx.send(SourceCommand::WriteRegister {
            addr: target.addr,
            data: data.clone(),
            label: target.label.clone(),
        }) {
            Ok(()) => {
                push_bounded(
                    &mut self.logs,
                    format!(
                        "write request {} @ 0x{:02x}: [{}]",
                        target.label,
                        target.addr,
                        hex_bytes(&data)
                    ),
                    MAX_TEXT_LINES,
                );
                self.register_write_dialog = None;
            }
            Err(_) => {
                dialog.error = Some("write failed: source thread is not running".to_string());
            }
        }
    }

    fn selected_register_read_target(&self) -> Result<RegisterReadTarget, String> {
        let addr = self.selected_register_addr;
        if self.register_dump.is_no_dump(addr) {
            return Err(format!(
                "0x{addr:02x} is marked no_dump; direct read skipped"
            ));
        }

        let regs = self.register_dump.registers_for_addr(addr);
        let exact = regs
            .iter()
            .copied()
            .find(|reg| !reg.no_dump && reg.addr == addr);
        let covering = regs.iter().copied().find(|reg| !reg.no_dump);
        if let Some(reg) = exact.or(covering) {
            return register_read_target_from_info(reg);
        }

        Ok(RegisterReadTarget {
            addr,
            len: 1,
            label: format!("0x{addr:02x}"),
        })
    }

    fn selected_register_write_target(&self) -> Result<RegisterWriteTarget, String> {
        let addr = self.selected_register_addr;
        let regs = self.register_dump.registers_for_addr(addr);
        let exact = regs
            .iter()
            .copied()
            .find(|reg| register_is_writable(&reg.access) && reg.addr == addr);
        let covering = regs
            .iter()
            .copied()
            .find(|reg| register_is_writable(&reg.access));
        if let Some(reg) = exact.or(covering) {
            return register_write_target_from_info(reg);
        }

        if regs.iter().any(|reg| !register_is_writable(&reg.access)) {
            return Err(format!("0x{addr:02x} is read-only; write skipped"));
        }

        Ok(RegisterWriteTarget {
            addr,
            width: Some(1),
            label: format!("0x{addr:02x}"),
        })
    }

    fn apply(&mut self, event: AppEvent) {
        match event {
            AppEvent::Sample {
                timestamp_us,
                acc,
                gyro,
            } => {
                self.sample_counter += 1;
                push_bounded(
                    &mut self.samples,
                    ImuSample {
                        index: self.sample_counter,
                        timestamp_us,
                        acc,
                        gyro,
                    },
                    self.history,
                );
            }
            AppEvent::Register { addr, access, data } => self.apply_register(addr, access, data),
            AppEvent::Report(line) => {
                self.report_counter += 1;
                push_bounded(&mut self.reports, line, MAX_TEXT_LINES);
            }
            AppEvent::SpecialEvent(event) => {
                if self.motion_events.len() == MAX_MOTION_EVENTS {
                    self.motion_events.pop_front();
                }
                self.motion_event_counts[event.kind.index()] =
                    self.motion_event_counts[event.kind.index()].saturating_add(1);
                self.motion_events.push_back(TuiMotionEvent {
                    kind: event.kind,
                    meta: event.meta,
                    sample_index: self.sample_counter,
                    args_len: event.args.len(),
                });
            }
            AppEvent::ControlApplied {
                name,
                label,
                report_scale,
            } => {
                self.samples.clear();
                self.motion_events.clear();
                self.motion_event_counts = [0; 3];
                let scale = report_scale
                    .map(|update| format!(", {}={}", update.kind.as_str(), update.value))
                    .unwrap_or_default();
                push_bounded(
                    &mut self.logs,
                    format!("applied {name}={label}{scale}; motion history cleared"),
                    MAX_TEXT_LINES,
                );
            }
            AppEvent::ControlFinished { name } => {
                if self.control_busy.as_deref() == Some(name.as_str()) {
                    self.control_busy = None;
                }
            }
            AppEvent::Log(line) => {
                push_bounded(&mut self.logs, line, MAX_TEXT_LINES);
            }
            AppEvent::Error(line) => {
                self.error_counter += 1;
                push_bounded(&mut self.logs, format!("error: {line}"), MAX_TEXT_LINES);
            }
        }
    }

    fn apply_register(&mut self, addr: u32, access: AccessKind, data: Vec<u8>) {
        if data.is_empty() || self.register_dump.is_no_dump(addr) {
            self.update_register_cell(addr, access, data);
            return;
        }

        for (offset, byte) in data.into_iter().enumerate() {
            let Some(cell_addr) = addr.checked_add(offset as u32) else {
                break;
            };
            if self.register_dump.is_no_dump(cell_addr) {
                self.update_register_cell(cell_addr, access, Vec::new());
            } else {
                self.update_register_cell(cell_addr, access, vec![byte]);
            }
        }
    }

    fn update_register_cell(&mut self, addr: u32, access: AccessKind, data: Vec<u8>) {
        self.registers
            .entry(addr)
            .and_modify(|value| {
                value.access = access;
                value.data = data.clone();
            })
            .or_insert(RegisterValue { access, data });
    }
}

fn register_read_target_from_info(reg: &RegisterInfo) -> Result<RegisterReadTarget, String> {
    let width = reg.width.max(1);
    if width as usize > rseq_link::wire::CONTROL_MAX_READ_LEN {
        return Err(format!(
            "{}.{} width {} exceeds control read limit {}",
            reg.page,
            reg.name,
            width,
            rseq_link::wire::CONTROL_MAX_READ_LEN
        ));
    }
    Ok(RegisterReadTarget {
        addr: reg.addr,
        len: width as u16,
        label: format!("{}.{}", reg.page, reg.name),
    })
}

fn register_write_target_from_info(reg: &RegisterInfo) -> Result<RegisterWriteTarget, String> {
    if !register_is_writable(&reg.access) {
        return Err(format!(
            "{}.{} is read-only; write skipped",
            reg.page, reg.name
        ));
    }

    let width = reg.width.max(1);
    if width as usize > rseq_link::wire::CONTROL_MAX_WRITE_LEN {
        return Err(format!(
            "{}.{} width {} exceeds control write limit {}",
            reg.page,
            reg.name,
            width,
            rseq_link::wire::CONTROL_MAX_WRITE_LEN
        ));
    }

    Ok(RegisterWriteTarget {
        addr: reg.addr,
        width: Some(width as usize),
        label: format!("{}.{}", reg.page, reg.name),
    })
}

fn register_is_writable(access: &str) -> bool {
    access.chars().any(|ch| ch == 'w' || ch == 'W')
}

fn push_bounded<T>(queue: &mut VecDeque<T>, value: T, cap: usize) {
    if cap == 0 {
        return;
    }
    while queue.len() >= cap {
        queue.pop_front();
    }
    queue.push_back(value);
}

fn render(frame: &mut Frame<'_>, app: &App) {
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(frame.area());

    render_tabs(frame, root[0], app);
    match app.selected_tab() {
        Tab::Motion => render_motion(frame, root[1], app),
        Tab::Reports => render_reports(frame, root[1], app),
        Tab::Registers => render_registers(frame, root[1], app),
        Tab::Controls => render_controls(frame, root[1], app),
        Tab::Logs => render_logs(frame, root[1], app),
    }
    render_help(frame, root[2], app);
    render_status(frame, root[3], app);
}

fn render_tabs(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let titles = Tab::ALL.iter().map(|tab| tab.title());
    let tabs = Tabs::new(titles)
        .select(app.tab)
        .block(Block::default().borders(Borders::ALL).title("rseq-tui"))
        .style(Style::default().fg(Color::Gray))
        .highlight_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_widget(tabs, area);
}

fn render_motion(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Percentage(50),
            Constraint::Percentage(50),
        ])
        .split(area);
    render_motion_events(frame, root[0], app);
    render_motion_chart(
        frame,
        root[1],
        app,
        MotionKind::Accel,
        "acc m/s^2",
        ["ax", "ay", "az"],
        [Color::Green, Color::Yellow, Color::Cyan],
    );
    render_motion_chart(
        frame,
        root[2],
        app,
        MotionKind::Gyro,
        "gyro rad/s",
        ["gx", "gy", "gz"],
        [Color::Magenta, Color::LightBlue, Color::LightRed],
    );
}

fn render_motion_events(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let mut spans = vec![Span::styled(
        " events ",
        Style::default()
            .fg(Color::Gray)
            .add_modifier(Modifier::BOLD),
    )];
    for kind in rseq_host::SpecialEventKind::ALL {
        spans.push(Span::styled(
            format!(
                " {}={} ",
                kind.as_str(),
                app.motion_event_counts[kind.index()]
            ),
            Style::default().fg(special_event_color(kind)),
        ));
    }
    spans.push(Span::raw(" "));
    spans.push(Span::styled(
        latest_motion_event_label(app),
        Style::default().fg(Color::DarkGray),
    ));
    let paragraph = Paragraph::new(Line::from(spans))
        .block(Block::default().borders(Borders::ALL).title("events"));
    frame.render_widget(paragraph, area);
}

#[derive(Clone, Copy)]
enum MotionKind {
    Accel,
    Gyro,
}

fn render_motion_chart(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    kind: MotionKind,
    title: &'static str,
    names: [&'static str; 3],
    colors: [Color; 3],
) {
    let (series, x_bounds, y_bounds) = chart_series(app, kind);
    let mut datasets = vec![
        dataset(names[0], colors[0], &series[0]),
        dataset(names[1], colors[1], &series[1]),
        dataset(names[2], colors[2], &series[2]),
    ];
    let marker_series = motion_event_marker_series(app, x_bounds, y_bounds);
    for event_kind in rseq_host::SpecialEventKind::ALL {
        let data = &marker_series[event_kind.index()];
        if data.is_empty() {
            continue;
        }
        datasets.push(
            Dataset::default()
                .name(event_kind.as_str())
                .marker(symbols::Marker::Dot)
                .graph_type(GraphType::Scatter)
                .style(Style::default().fg(special_event_color(event_kind)))
                .data(data),
        );
    }
    let chart = Chart::new(datasets)
        .block(Block::default().borders(Borders::ALL).title(title))
        .x_axis(
            Axis::default()
                .bounds(x_bounds)
                .labels([format!("{:.0}", x_bounds[0]), format!("{:.0}", x_bounds[1])]),
        )
        .y_axis(Axis::default().bounds(y_bounds).labels([
            format!("{:.2}", y_bounds[0]),
            "0".to_string(),
            format!("{:.2}", y_bounds[1]),
        ]));
    frame.render_widget(chart, area);
}

fn dataset<'a>(name: &'static str, color: Color, data: &'a [(f64, f64)]) -> Dataset<'a> {
    Dataset::default()
        .name(name)
        .marker(symbols::Marker::Braille)
        .graph_type(GraphType::Line)
        .style(Style::default().fg(color))
        .data(data)
}

fn chart_series(app: &App, kind: MotionKind) -> ([Vec<(f64, f64)>; 3], [f64; 2], [f64; 2]) {
    let mut series: [Vec<(f64, f64)>; 3] = std::array::from_fn(|_| Vec::new());
    let mut y_min = f64::INFINITY;
    let mut y_max = f64::NEG_INFINITY;

    for sample in &app.samples {
        let values = match kind {
            MotionKind::Accel => sample.acc,
            MotionKind::Gyro => sample.gyro,
        };
        let x = sample.index as f64;
        for axis in 0..3 {
            let y = values[axis];
            y_min = y_min.min(y);
            y_max = y_max.max(y);
            series[axis].push((x, y));
        }
    }

    let x_bounds = match (app.samples.front(), app.samples.back()) {
        (Some(first), Some(last)) if first.index < last.index => {
            [first.index as f64, last.index as f64]
        }
        (Some(sample), _) => [sample.index as f64, sample.index as f64 + 1.0],
        _ => [0.0, 1.0],
    };

    let y_bounds = if y_min.is_finite() && y_max.is_finite() {
        if (y_max - y_min).abs() < f64::EPSILON {
            [y_min - 1.0, y_max + 1.0]
        } else {
            let pad = (y_max - y_min).abs() * 0.12;
            [y_min - pad, y_max + pad]
        }
    } else {
        match kind {
            MotionKind::Accel => [-12.0, 12.0],
            MotionKind::Gyro => [-2.0, 2.0],
        }
    };

    (series, x_bounds, y_bounds)
}

fn motion_event_marker_series(
    app: &App,
    x_bounds: [f64; 2],
    y_bounds: [f64; 2],
) -> [Vec<(f64, f64)>; 3] {
    let mut markers: [Vec<(f64, f64)>; 3] = std::array::from_fn(|_| Vec::new());
    let span = (y_bounds[1] - y_bounds[0]).abs().max(1.0);
    for event in &app.motion_events {
        let x = if let Some(meta) = event.meta.filter(|meta| meta.timestamp_valid()) {
            let Some(index) =
                nearest_imu_sample_index_by_timestamp(&app.samples, meta.timestamp_us)
            else {
                continue;
            };
            index
        } else {
            event.sample_index
        } as f64;
        if x < x_bounds[0] || x > x_bounds[1] {
            continue;
        }
        let lane = event.kind.index() as f64;
        let y = y_bounds[1] - span * (0.10 + lane * 0.07);
        markers[event.kind.index()].push((x, y));
    }
    markers
}

fn nearest_imu_sample_index_by_timestamp(
    samples: &VecDeque<ImuSample>,
    timestamp_us: u64,
) -> Option<u64> {
    let mut first_ts = None;
    let mut last_ts = None;
    let mut nearest = None;

    for sample in samples {
        let Some(sample_ts) = sample.timestamp_us else {
            continue;
        };
        first_ts.get_or_insert(sample_ts);
        last_ts = Some(sample_ts);
        let delta = sample_ts.abs_diff(timestamp_us);
        if nearest.is_none_or(|(_, best_delta)| delta < best_delta) {
            nearest = Some((sample.index, delta));
        }
    }

    let (Some(first_ts), Some(last_ts)) = (first_ts, last_ts) else {
        return None;
    };
    let min_ts = first_ts.min(last_ts);
    let max_ts = first_ts.max(last_ts);
    if timestamp_us < min_ts || timestamp_us > max_ts {
        return None;
    }

    nearest.map(|(index, _)| index)
}

fn latest_motion_event_label(app: &App) -> String {
    match app.motion_events.back() {
        Some(event) => format!(
            "latest {} {} args={}",
            event.kind.as_str(),
            format_motion_event_meta(event.meta),
            event.args_len
        ),
        None => "no special events".to_string(),
    }
}

fn format_motion_event_meta(meta: Option<ReportMeta>) -> String {
    match meta {
        Some(meta) if meta.timestamp_valid() => {
            format!("frame={} ts_us={}", meta.frame_id, meta.timestamp_us)
        }
        Some(meta) => format!("frame={}", meta.frame_id),
        None => "no-meta".to_string(),
    }
}

fn special_event_color(kind: rseq_host::SpecialEventKind) -> Color {
    match kind {
        rseq_host::SpecialEventKind::Amd => Color::Yellow,
        rseq_host::SpecialEventKind::Smd => Color::Red,
        rseq_host::SpecialEventKind::Drdy => Color::Cyan,
    }
}

fn render_reports(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let lines = latest_lines(&app.reports, area.height.saturating_sub(2) as usize);
    let paragraph = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title("reports"))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn render_registers(frame: &mut Frame<'_>, area: Rect, app: &App) {
    frame.render_widget(Clear, area);

    let max_addr = register_grid_max_addr(app).max(0x0f);
    let max_base = max_addr & !0x0f;
    let row_count = (max_base / 16 + 1) as usize;
    let rows = (0..row_count).map(|row| {
        let base = (row as u32) * 16;
        let mut cells = Vec::with_capacity(17);
        cells.push(Cell::from(format!("0x{base:02x}")).style(Style::default().fg(Color::Yellow)));
        for offset in 0..16 {
            cells.push(register_grid_cell(app, base + offset));
        }
        Row::new(cells)
    });

    let mut header_cells = Vec::with_capacity(17);
    header_cells.push(Cell::from("base"));
    for offset in 0..16 {
        header_cells.push(Cell::from(format!("{offset:02x}")));
    }

    let mut widths = Vec::with_capacity(17);
    widths.push(Constraint::Length(6));
    widths.extend((0..16).map(|_| Constraint::Length(2)));

    let table = Table::new(rows, widths)
        .column_spacing(1)
        .header(
            Row::new(header_cells).style(
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
        )
        .block(Block::default().borders(Borders::ALL).title("register map"));
    frame.render_widget(table, area);

    if app.register_detail_open {
        render_register_detail(frame, centered_rect(area, 74, 70), app);
    }
    if let Some(dialog) = &app.register_write_dialog {
        render_register_write_dialog(frame, centered_rect(area, 62, 28), dialog);
    }
}

fn register_grid_max_addr(app: &App) -> u32 {
    app.registers
        .keys()
        .next_back()
        .copied()
        .into_iter()
        .chain(app.register_dump.max_addr())
        .max()
        .unwrap_or(0x0f)
}

fn register_grid_cell(app: &App, addr: u32) -> Cell<'static> {
    let selected = app.selected_register_addr == addr;
    if app.register_dump.is_no_dump(addr) {
        return Cell::from("??").style(register_cell_style(
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
            selected,
        ));
    }

    let Some(reg) = app.registers.get(&addr) else {
        return Cell::from("--").style(register_cell_style(
            Style::default().fg(Color::DarkGray),
            selected,
        ));
    };

    let value = reg
        .data
        .first()
        .map(|byte| format!("{byte:02x}"))
        .unwrap_or_else(|| "--".to_string());
    let color = match reg.access {
        AccessKind::Read => Color::Green,
        AccessKind::Write => Color::Cyan,
    };
    Cell::from(value).style(register_cell_style(Style::default().fg(color), selected))
}

fn register_cell_style(style: Style, selected: bool) -> Style {
    if selected {
        style.add_modifier(Modifier::REVERSED | Modifier::BOLD)
    } else {
        style
    }
}

fn render_register_detail(frame: &mut Frame<'_>, area: Rect, app: &App) {
    frame.render_widget(Clear, area);
    let mut lines = Vec::new();
    let addr = app.selected_register_addr;
    let value = if app.register_dump.is_no_dump(addr) {
        "??".to_string()
    } else {
        app.registers
            .get(&addr)
            .and_then(|reg| reg.data.first())
            .map(|byte| format!("0x{byte:02x}"))
            .unwrap_or_else(|| "--".to_string())
    };

    lines.push(Line::from(vec![
        Span::styled("addr ", Style::default().fg(Color::DarkGray)),
        Span::raw(format!("0x{addr:02x}")),
        Span::styled(" value ", Style::default().fg(Color::DarkGray)),
        Span::raw(value),
    ]));

    let regs = app.register_dump.registers_for_addr(addr);
    if regs.is_empty() {
        lines.push(Line::from("no register metadata"));
    } else {
        for (idx, reg) in regs.iter().enumerate() {
            if idx != 0 {
                lines.push(Line::from(""));
            }
            lines.push(Line::from(vec![
                Span::styled("reg ", Style::default().fg(Color::DarkGray)),
                Span::raw(format!("{}.{}", reg.page, reg.name)),
                Span::styled(" access ", Style::default().fg(Color::DarkGray)),
                Span::raw(reg.access.clone()),
                Span::styled(" width ", Style::default().fg(Color::DarkGray)),
                Span::raw(reg.width.to_string()),
            ]));
            if addr != reg.addr {
                lines.push(Line::from(format!("byte offset +{}", addr - reg.addr)));
            }
            if reg.no_dump {
                let reason = if reg.no_dump_reason.is_empty() {
                    "no_dump".to_string()
                } else {
                    format!("no_dump: {}", reg.no_dump_reason)
                };
                lines.push(Line::from(reason));
            }
            if !reg.desc.is_empty() {
                lines.push(Line::from(reg.desc.clone()));
            }
            for field in &reg.fields {
                let bits = if field.bit_hi == field.bit_lo {
                    field.bit_lo.to_string()
                } else {
                    format!("{}:{}", field.bit_hi, field.bit_lo)
                };
                let mut line = format!("[{bits}] {}", field.name);
                if let Some(event) = &field.event {
                    let _ = write!(line, " event={event}");
                }
                if !field.desc.is_empty() {
                    let _ = write!(line, " - {}", field.desc);
                }
                lines.push(Line::from(line));
            }
        }
    }

    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("register detail"),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn render_register_write_dialog(frame: &mut Frame<'_>, area: Rect, dialog: &RegisterWriteDialog) {
    frame.render_widget(Clear, area);
    let mut lines = Vec::new();
    let width = dialog
        .target
        .width
        .map(|width| width.to_string())
        .unwrap_or_else(|| "any".to_string());
    lines.push(Line::from(vec![
        Span::styled("reg ", Style::default().fg(Color::DarkGray)),
        Span::raw(dialog.target.label.clone()),
        Span::styled(" addr ", Style::default().fg(Color::DarkGray)),
        Span::raw(format!("0x{:02x}", dialog.target.addr)),
        Span::styled(" bytes ", Style::default().fg(Color::DarkGray)),
        Span::raw(width),
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("> ", Style::default().fg(Color::Yellow)),
        Span::raw(dialog.input.clone()),
    ]));
    if let Some(error) = &dialog.error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            error.clone(),
            Style::default().fg(Color::Red),
        )));
    }

    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("write register: Enter send | Esc/q cancel"),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn render_controls(frame: &mut Frame<'_>, area: Rect, app: &App) {
    if app.tuning_catalog.is_empty() {
        let paragraph = Paragraph::new("no runtime controls loaded; pass --chip or chip!(...)")
            .block(Block::default().borders(Borders::ALL).title("controls"));
        frame.render_widget(paragraph, area);
        return;
    }

    let rows = app
        .tuning_catalog
        .controls()
        .iter()
        .enumerate()
        .map(|(idx, control)| {
            let selected = idx == app.selected_control_index;
            let value = control_value_text(app, control);
            let options = rseq_host::tuning_control_value_hint(control);
            Row::new([
                Cell::from(if control.group.is_empty() {
                    "General".to_string()
                } else {
                    control.group.clone()
                }),
                Cell::from(control.name.clone()),
                Cell::from(value),
                Cell::from(control.target.clone()),
                Cell::from(options),
            ])
            .style(if selected {
                Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
            } else {
                Style::default()
            })
        });

    let table = Table::new(
        rows,
        [
            Constraint::Length(12),
            Constraint::Length(18),
            Constraint::Length(14),
            Constraint::Length(24),
            Constraint::Min(24),
        ],
    )
    .column_spacing(1)
    .header(
        Row::new(["Group", "Name", "Value", "Target", "Options"]).style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
    )
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title("controls: r read | [/] cycle | Enter/e edit"),
    );
    frame.render_widget(table, area);

    if let Some(dialog) = &app.tuning_dialog {
        render_tuning_dialog(frame, centered_rect(area, 68, 30), dialog);
    }
}

fn control_value_text(app: &App, control: &rseq_host::TuningControl) -> String {
    let width = control.width.max(1) as usize;
    let mut bytes = Vec::with_capacity(width);
    for offset in 0..width {
        let Some(byte) = app
            .registers
            .get(&(control.addr + offset as u32))
            .and_then(|value| value.data.first())
        else {
            return "--".to_string();
        };
        bytes.push(*byte);
    }
    rseq_host::tuning_control_value_from_bytes(control, &bytes)
        .map(|value| {
            format!(
                "{} ({value})",
                rseq_host::tuning_control_value_label(control, value)
            )
        })
        .unwrap_or_else(|| "--".to_string())
}

fn render_tuning_dialog(frame: &mut Frame<'_>, area: Rect, dialog: &TuningDialog) {
    frame.render_widget(Clear, area);
    let mut lines = vec![
        Line::from(vec![
            Span::styled("control ", Style::default().fg(Color::DarkGray)),
            Span::raw(dialog.control.name.clone()),
            Span::styled(" target ", Style::default().fg(Color::DarkGray)),
            Span::raw(dialog.control.target.clone()),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("> ", Style::default().fg(Color::Yellow)),
            Span::raw(dialog.input.clone()),
        ]),
        Line::from(""),
        Line::from(format!(
            "values: {}",
            rseq_host::tuning_control_value_hint(&dialog.control)
        )),
    ];
    if let Some(error) = &dialog.error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            error.clone(),
            Style::default().fg(Color::Red),
        )));
    }
    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("set control: Enter send | Esc/q cancel"),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn centered_rect(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1]);
    horizontal[1]
}

fn render_logs(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let lines = latest_lines(&app.logs, area.height.saturating_sub(2) as usize);
    let paragraph = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title("logs"))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn render_help(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let hints = help_spans(app);
    frame.render_widget(Paragraph::new(Line::from(hints)), area);
}

fn help_spans(app: &App) -> Vec<Span<'static>> {
    if app.tuning_dialog.is_some() {
        return key_hint_spans(&[
            ("Enter", "set"),
            ("Esc/q", "cancel"),
            ("Backspace", "edit"),
            ("Del", "clear"),
        ]);
    }

    if app.register_write_dialog.is_some() {
        return key_hint_spans(&[
            ("Enter", "write"),
            ("Esc/q", "cancel"),
            ("Backspace", "edit"),
            ("Del", "clear"),
        ]);
    }

    if app.selected_tab() == Tab::Registers && app.register_detail_open {
        return key_hint_spans(&[
            ("q/Esc", "close detail"),
            ("r", "dump"),
            ("w", "write"),
            ("arrows", "select"),
            ("Tab", "switch"),
        ]);
    }

    if app.selected_tab() == Tab::Registers {
        return key_hint_spans(&[
            ("arrows", "select"),
            ("r", "dump"),
            ("w", "write"),
            ("Enter/i", "detail"),
            ("Tab", "switch"),
            ("q", "quit"),
        ]);
    }

    if app.selected_tab() == Tab::Controls {
        return key_hint_spans(&[
            ("up/down", "select"),
            ("r", "read"),
            ("[/]", "cycle"),
            ("Enter/e", "edit"),
            ("Tab", "switch"),
            ("q", "quit"),
        ]);
    }

    key_hint_spans(&[
        ("Tab/Shift+Tab", "switch"),
        ("1-5", "jump"),
        ("q/Esc", "quit"),
    ])
}

fn key_hint_spans(hints: &[(&'static str, &'static str)]) -> Vec<Span<'static>> {
    let mut spans = Vec::with_capacity(hints.len() * 4);
    spans.push(Span::styled("keys ", Style::default().fg(Color::DarkGray)));
    for (idx, (key, action)) in hints.iter().enumerate() {
        if idx != 0 {
            spans.push(Span::styled(" | ", Style::default().fg(Color::DarkGray)));
        }
        spans.push(Span::styled(
            *key,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(" ", Style::default().fg(Color::DarkGray)));
        spans.push(Span::raw(*action));
    }
    spans
}

fn render_status(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let latest_ts = app
        .samples
        .back()
        .and_then(|sample| sample.timestamp_us)
        .map(|ts| format!(" ts={ts}us"))
        .unwrap_or_default();
    let text = Line::from(vec![
        Span::styled("source ", Style::default().fg(Color::DarkGray)),
        Span::raw(&app.source_label),
        Span::styled(" samples ", Style::default().fg(Color::DarkGray)),
        Span::raw(app.sample_counter.to_string()),
        Span::styled(" reports ", Style::default().fg(Color::DarkGray)),
        Span::raw(app.report_counter.to_string()),
        Span::styled(" regs ", Style::default().fg(Color::DarkGray)),
        Span::raw(app.registers.len().to_string()),
        Span::styled(" controls ", Style::default().fg(Color::DarkGray)),
        Span::raw(app.tuning_catalog.len().to_string()),
        Span::styled(" errors ", Style::default().fg(Color::DarkGray)),
        Span::raw(app.error_counter.to_string()),
        Span::styled(" up ", Style::default().fg(Color::DarkGray)),
        Span::raw(format_age(app.started_at.elapsed())),
        Span::raw(latest_ts),
    ]);
    frame.render_widget(Paragraph::new(text), area);
}

fn latest_lines(lines: &VecDeque<String>, max: usize) -> Vec<Line<'static>> {
    lines
        .iter()
        .rev()
        .take(max)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|line| Line::from(line.clone()))
        .collect()
}

fn format_age(duration: Duration) -> String {
    if duration.as_secs() >= 60 {
        format!(
            "{}m{:02}s",
            duration.as_secs() / 60,
            duration.as_secs() % 60
        )
    } else if duration.as_secs() > 0 {
        format!("{}s", duration.as_secs())
    } else {
        format!("{}ms", duration.as_millis())
    }
}

fn spawn_demo_source(
    tx: Sender<AppEvent>,
    cmd_rx: Receiver<SourceCommand>,
    stop: Arc<AtomicBool>,
    startup_controls: Vec<rseq_host::TuningAssignment>,
) {
    thread::spawn(move || {
        let start = Instant::now();
        let mut frame = 0u64;
        let mut regs = [0u8; 256];
        regs[0] = 0x06;
        let _ = tx.send(AppEvent::Log("demo source started".to_string()));
        for assignment in startup_controls {
            apply_demo_control(assignment, &mut regs, &tx);
        }
        while !stop.load(Ordering::Relaxed) {
            while let Ok(cmd) = cmd_rx.try_recv() {
                match cmd {
                    SourceCommand::ReadRegister { addr, len, label } => {
                        let data = (0..len)
                            .map(|offset| regs[((addr + u32::from(offset)) & 0xff) as usize])
                            .collect::<Vec<_>>();
                        let _ = tx.send(AppEvent::Register {
                            addr,
                            access: AccessKind::Read,
                            data: data.clone(),
                        });
                        let _ = tx.send(AppEvent::Log(format!(
                            "demo dump {label} @ 0x{addr:02x} len={len}: [{}]",
                            hex_bytes(&data)
                        )));
                    }
                    SourceCommand::WriteRegister { addr, data, label } => {
                        for (offset, byte) in data.iter().enumerate() {
                            regs[(addr as usize + offset) & 0xff] = *byte;
                        }
                        let _ = tx.send(AppEvent::Register {
                            addr,
                            access: AccessKind::Write,
                            data: data.clone(),
                        });
                        let _ = tx.send(AppEvent::Log(format!(
                            "demo write {label} @ 0x{addr:02x}: [{}]",
                            hex_bytes(&data)
                        )));
                    }
                    SourceCommand::SetControl(assignment) => {
                        apply_demo_control(assignment, &mut regs, &tx);
                    }
                }
            }

            let t = start.elapsed().as_secs_f64();
            let acc = [
                (t * 1.7).sin() * 1.8,
                (t * 1.3).cos() * 1.2,
                9.80665 + (t * 0.9).sin() * 0.4,
            ];
            let gyro = [
                (t * 2.2).sin() * 1.4,
                (t * 1.1).cos() * 0.9,
                (t * 1.6).sin() * 0.6,
            ];
            let _ = tx.send(AppEvent::Sample {
                timestamp_us: Some(start.elapsed().as_micros() as u64),
                acc,
                gyro,
            });

            if frame % 8 == 0 {
                let fifo_len = 72 + ((frame / 8) % 4) as u32 * 12;
                let _ = tx.send(AppEvent::Report(format!(
                    "FIFO_RAW frame_id={frame} ts_us={} fifo_len={fifo_len} samples={}",
                    start.elapsed().as_micros(),
                    fifo_len / 12
                )));
            }
            if frame % 96 == 0 {
                let meta = ReportMeta {
                    flags: rseq_link::REPORT_FLAG_TIMESTAMP_VALID,
                    frame_id: frame as u32,
                    timestamp_us: start.elapsed().as_micros() as u64,
                };
                let _ = tx.send(AppEvent::SpecialEvent(rseq_host::SpecialEvent {
                    kind: rseq_host::SpecialEventKind::Amd,
                    meta: Some(meta),
                    args: Vec::new(),
                }));
                let _ = tx.send(AppEvent::Report(format!(
                    "AMD frame_id={} ts_us={}",
                    meta.frame_id, meta.timestamp_us
                )));
            }
            if frame % 16 == 0 {
                let fifo_low = ((72 + frame as u32) & 0xff) as u8;
                let fifo_high = (((72 + frame as u32) >> 8) & 0x0f) as u8;
                let _ = tx.send(AppEvent::Register {
                    addr: 0x00,
                    access: AccessKind::Read,
                    data: vec![0x06],
                });
                let _ = tx.send(AppEvent::Register {
                    addr: 0x2e,
                    access: AccessKind::Read,
                    data: vec![fifo_low],
                });
                let _ = tx.send(AppEvent::Register {
                    addr: 0x2f,
                    access: AccessKind::Read,
                    data: vec![fifo_high],
                });
                let _ = tx.send(AppEvent::Register {
                    addr: 0x30,
                    access: AccessKind::Read,
                    data: vec![0x40, 0x01, 0x3f, 0xff],
                });
            }

            frame = frame.wrapping_add(1);
            thread::sleep(Duration::from_millis(20));
        }
    });
}

fn apply_demo_control(
    assignment: rseq_host::TuningAssignment,
    regs: &mut [u8; 256],
    tx: &Sender<AppEvent>,
) {
    let name = assignment.control.name.clone();
    let label = rseq_host::tuning_control_value_label(&assignment.control, assignment.value);
    let report_scale = match rseq_host::tuning_assignment_report_scale(&assignment) {
        Ok(update) => update,
        Err(err) => {
            let _ = tx.send(AppEvent::Error(format!("demo set {name} failed: {err}")));
            let _ = tx.send(AppEvent::ControlFinished { name });
            return;
        }
    };
    let control = assignment.control;
    let width = control.width.max(1) as usize;
    let current = (0..width)
        .map(|offset| regs[(control.addr as usize + offset) & 0xff])
        .collect::<Vec<_>>();
    match rseq_host::apply_tuning_control_value(&control, &current, assignment.value) {
        Ok(data) => {
            for (offset, byte) in data.iter().enumerate() {
                regs[(control.addr as usize + offset) & 0xff] = *byte;
            }
            let _ = tx.send(AppEvent::Register {
                addr: control.addr,
                access: AccessKind::Write,
                data: data.clone(),
            });
            let _ = tx.send(AppEvent::Log(format!(
                "demo set {}={} @ 0x{:02x}: [{}]",
                control.name,
                rseq_host::tuning_control_value_label(&control, assignment.value),
                control.addr,
                hex_bytes(&data)
            )));
            let _ = tx.send(AppEvent::ControlApplied {
                name: control.name.clone(),
                label,
                report_scale,
            });
        }
        Err(err) => {
            let _ = tx.send(AppEvent::Error(format!(
                "demo set {} failed: {err}",
                control.name
            )));
        }
    }
    let _ = tx.send(AppEvent::ControlFinished { name });
}

#[cfg(feature = "serial")]
fn spawn_serial_source(
    serial: String,
    baud: u32,
    startup_program: Option<CompiledProgram>,
    startup_controls: Vec<rseq_host::TuningAssignment>,
    mut report_decoders: ReportDecoderRegistry,
    tx: Sender<AppEvent>,
    cmd_rx: Receiver<SourceCommand>,
    stop: Arc<AtomicBool>,
) {
    thread::spawn(move || {
        let mode = if startup_program.is_some() {
            "load+exec"
        } else {
            "watch"
        };
        let _ = tx.send(AppEvent::Log(format!(
            "opening serial {serial} @ {baud} ({mode})"
        )));
        let transport = if startup_program.is_some() {
            rseq_link::SerialTransport::open(&serial, baud)
        } else {
            rseq_link::SerialTransport::open_observing(&serial, baud)
        };
        let transport = match transport {
            Ok(transport) => transport,
            Err(err) => {
                let _ = tx.send(AppEvent::Error(format!("open serial failed: {err}")));
                return;
            }
        };
        let mut host = rseq::link::HostLink::new(transport);
        if let Some(program) = startup_program {
            if !load_and_exec_program(&mut host, &program, &mut report_decoders, &tx) {
                return;
            }
        } else {
            let _ = tx.send(AppEvent::Log(
                "watch mode: no LOAD/EXEC frames will be sent".to_string(),
            ));
        }
        handle_tui_control_transactions(&startup_controls, &mut host, &mut report_decoders, &tx);
        let _ = tx.send(AppEvent::Log("serial observe loop started".to_string()));
        while !stop.load(Ordering::Relaxed) {
            while let Ok(cmd) = cmd_rx.try_recv() {
                handle_source_command(cmd, &mut host, &mut report_decoders, &tx);
            }

            match host.observe_next_trace(Duration::from_millis(20)) {
                Ok(Some(op)) => handle_bus_op(op, &mut report_decoders, &tx),
                Ok(None) => {}
                Err(err) => {
                    let _ = tx.send(AppEvent::Error(format!("observe failed: {err}")));
                    thread::sleep(Duration::from_millis(250));
                }
            }
        }
    });
}

fn spawn_tcp_source(
    addr: String,
    startup_program: Option<CompiledProgram>,
    startup_controls: Vec<rseq_host::TuningAssignment>,
    mut report_decoders: ReportDecoderRegistry,
    tx: Sender<AppEvent>,
    cmd_rx: Receiver<SourceCommand>,
    stop: Arc<AtomicBool>,
) {
    thread::spawn(move || {
        let mode = if startup_program.is_some() {
            "load+exec"
        } else {
            "watch"
        };
        let _ = tx.send(AppEvent::Log(format!("opening tcp {addr} ({mode})")));
        let transport = if startup_program.is_some() {
            rseq_link::TcpTransport::connect(addr.as_str())
        } else {
            rseq_link::TcpTransport::connect_observing(addr.as_str())
        };
        let transport = match transport {
            Ok(transport) => transport,
            Err(err) => {
                let _ = tx.send(AppEvent::Error(format!("open tcp failed: {err}")));
                return;
            }
        };
        let mut host = rseq::link::HostLink::new(transport);
        if let Some(program) = startup_program {
            if !load_and_exec_program(&mut host, &program, &mut report_decoders, &tx) {
                return;
            }
        } else {
            let _ = tx.send(AppEvent::Log(
                "watch mode: no LOAD/EXEC frames will be sent".to_string(),
            ));
        }
        handle_tui_control_transactions(&startup_controls, &mut host, &mut report_decoders, &tx);
        let _ = tx.send(AppEvent::Log("tcp observe loop started".to_string()));
        while !stop.load(Ordering::Relaxed) {
            while let Ok(cmd) = cmd_rx.try_recv() {
                handle_source_command(cmd, &mut host, &mut report_decoders, &tx);
            }

            match host.observe_next_trace(Duration::from_millis(20)) {
                Ok(Some(op)) => handle_bus_op(op, &mut report_decoders, &tx),
                Ok(None) => {}
                Err(err) => {
                    let _ = tx.send(AppEvent::Error(format!("observe failed: {err}")));
                    thread::sleep(Duration::from_millis(250));
                }
            }
        }
    });
}

#[cfg(not(feature = "serial"))]
fn spawn_serial_source(
    serial: String,
    _baud: u32,
    _startup_program: Option<CompiledProgram>,
    _startup_controls: Vec<rseq_host::TuningAssignment>,
    _report_decoders: ReportDecoderRegistry,
    tx: Sender<AppEvent>,
    _cmd_rx: Receiver<SourceCommand>,
    _stop: Arc<AtomicBool>,
) {
    let _ = tx.send(AppEvent::Error(format!(
        "serial support is disabled for {serial}; rebuild rseq-tui with --features serial"
    )));
}

fn load_and_exec_program<T: rseq_link::Transport>(
    host: &mut rseq::link::HostLink<T>,
    program: &CompiledProgram,
    report_decoders: &mut ReportDecoderRegistry,
    tx: &Sender<AppEvent>,
) -> bool {
    use rseq_link::wire::{SEG_KIND_IRQ_INT1, SEG_KIND_MAIN};

    host.set_exec_timeout(Duration::from_secs(30));

    let mut segments: Vec<(u8, &[u8])> = vec![(SEG_KIND_MAIN, program.main.as_slice())];
    if let Some(int1_bc) = program.irq_bytecodes.get("int1") {
        segments.push((SEG_KIND_IRQ_INT1, int1_bc.as_slice()));
    }
    for pin in program
        .irq_bytecodes
        .keys()
        .filter(|pin| pin.as_str() != "int1")
    {
        let _ = tx.send(AppEvent::Log(format!(
            "compiled irq!({pin}) but this transport maps only int1; segment skipped"
        )));
    }

    let _ = tx.send(AppEvent::Log(format!(
        "loading rseq main={} byte(s), irq_handlers={}",
        program.main.len(),
        program.irq_bytecodes.len()
    )));
    if let Err(err) = host.load_segments(&segments) {
        let _ = tx.send(AppEvent::Error(format!("LOAD failed: {err}")));
        return false;
    }
    let _ = tx.send(AppEvent::Log("LOAD ok".to_string()));

    match host.exec() {
        Ok(result) => {
            let _ = tx.send(AppEvent::Log(format!("EXEC status: {:?}", result.status)));
            for op in result.traces {
                handle_bus_op(op, report_decoders, tx);
            }
            true
        }
        Err(err) => {
            let _ = tx.send(AppEvent::Error(format!("EXEC failed: {err}")));
            false
        }
    }
}

fn handle_source_command<T: rseq_link::Transport>(
    cmd: SourceCommand,
    host: &mut rseq::link::HostLink<T>,
    report_decoders: &mut ReportDecoderRegistry,
    tx: &Sender<AppEvent>,
) {
    match cmd {
        SourceCommand::ReadRegister { addr, len, label } => {
            let result = host.control_read_observing(addr, len, Duration::from_secs(2), |op| {
                handle_bus_op(op, report_decoders, tx)
            });
            match result {
                Ok(result) => {
                    let _ = tx.send(AppEvent::Register {
                        addr: result.addr,
                        access: AccessKind::Read,
                        data: result.data.clone(),
                    });
                    let _ = tx.send(AppEvent::Log(format!(
                        "dump {} @ 0x{:02x} len={}: [{}]",
                        label,
                        result.addr,
                        result.data.len(),
                        hex_bytes(&result.data)
                    )));
                }
                Err(err) => {
                    let _ = tx.send(AppEvent::Error(format!(
                        "dump {label} @ 0x{addr:02x} len={len} failed: {err}"
                    )));
                }
            }
        }
        SourceCommand::WriteRegister { addr, data, label } => {
            let result = host.control_write_observing(addr, &data, Duration::from_secs(2), |op| {
                handle_bus_op(op, report_decoders, tx)
            });
            match result {
                Ok(result) => {
                    let _ = tx.send(AppEvent::Register {
                        addr: result.addr,
                        access: AccessKind::Write,
                        data: data.clone(),
                    });
                    let _ = tx.send(AppEvent::Log(format!(
                        "write {} @ 0x{:02x} len={}: [{}]",
                        label,
                        result.addr,
                        result.len,
                        hex_bytes(&data)
                    )));
                }
                Err(err) => {
                    let _ = tx.send(AppEvent::Error(format!(
                        "write {label} @ 0x{addr:02x} data=[{}] failed: {err}",
                        hex_bytes(&data)
                    )));
                }
            }
        }
        SourceCommand::SetControl(assignment) => {
            handle_tui_control_transaction(assignment, host, report_decoders, tx);
        }
    }
}

fn handle_tui_control_transaction<T: rseq_link::Transport>(
    assignment: rseq_host::TuningAssignment,
    host: &mut rseq::link::HostLink<T>,
    report_decoders: &mut ReportDecoderRegistry,
    tx: &Sender<AppEvent>,
) {
    let control = &assignment.control;
    let name = control.name.clone();
    let label = rseq_host::tuning_control_value_label(control, assignment.value);
    let _ = tx.send(AppEvent::Log(format!(
        "pausing report stream to set {name}={label}"
    )));
    let reports_paused = tui_pause_reports_best_effort(host, tx);

    let result = apply_tui_paused_control(&assignment, host, report_decoders, tx);
    let resume_result = if reports_paused {
        Some(host.resume_reports())
    } else {
        let _ = host.resume_reports_timeout(Duration::from_millis(50));
        None
    };
    match result {
        Ok(report_scale) => {
            let _ = tx.send(AppEvent::ControlApplied {
                name: name.clone(),
                label: label.clone(),
                report_scale,
            });
            if matches!(resume_result, Some(Ok(()))) {
                let _ = tx.send(AppEvent::Log(format!(
                    "set {name}={label}; report stream resumed"
                )));
            } else if !reports_paused {
                let _ = tx.send(AppEvent::Log(format!(
                    "set {name}={label}; MCU pause unavailable, host stream resync requested"
                )));
            }
        }
        Err(err) => {
            let _ = tx.send(AppEvent::Error(format!("set {name} failed: {err}")));
        }
    }
    if let Some(Err(err)) = resume_result {
        let _ = tx.send(AppEvent::Error(format!(
            "resume reports after setting {name} failed: {err}"
        )));
    }
    let _ = tx.send(AppEvent::ControlFinished { name });
}

fn handle_tui_control_transactions<T: rseq_link::Transport>(
    assignments: &[rseq_host::TuningAssignment],
    host: &mut rseq::link::HostLink<T>,
    report_decoders: &mut ReportDecoderRegistry,
    tx: &Sender<AppEvent>,
) {
    if assignments.is_empty() {
        return;
    }
    if assignments.len() == 1 {
        handle_tui_control_transaction(assignments[0].clone(), host, report_decoders, tx);
        return;
    }

    let summary = tui_tuning_assignments_summary(assignments);
    let _ = tx.send(AppEvent::Log(format!(
        "pausing report stream to apply runtime controls: {summary}"
    )));
    let reports_paused = tui_pause_reports_best_effort(host, tx);

    let mut failed = false;
    for assignment in assignments {
        let control = &assignment.control;
        let name = control.name.clone();
        let label = rseq_host::tuning_control_value_label(control, assignment.value);
        match apply_tui_paused_control(assignment, host, report_decoders, tx) {
            Ok(report_scale) => {
                let _ = tx.send(AppEvent::ControlApplied {
                    name: name.clone(),
                    label: label.clone(),
                    report_scale,
                });
                let _ = tx.send(AppEvent::Log(format!("set {name}={label}")));
            }
            Err(err) => {
                let _ = tx.send(AppEvent::Error(format!("set {name} failed: {err}")));
                failed = true;
                break;
            }
        }
    }

    let resume_result = if reports_paused {
        Some(host.resume_reports())
    } else {
        let _ = host.resume_reports_timeout(Duration::from_millis(50));
        None
    };
    if let Some(Err(err)) = resume_result {
        let _ = tx.send(AppEvent::Error(format!(
            "resume reports after setting runtime controls failed: {err}"
        )));
    } else if !failed && reports_paused {
        let _ = tx.send(AppEvent::Log(
            "runtime controls applied; report stream resumed".to_string(),
        ));
    } else if !failed {
        let _ = tx.send(AppEvent::Log(
            "runtime controls applied; MCU pause unavailable, host stream resync requested"
                .to_string(),
        ));
    }
}

fn tui_pause_reports_best_effort<T: rseq_link::Transport>(
    host: &mut rseq::link::HostLink<T>,
    tx: &Sender<AppEvent>,
) -> bool {
    match host.pause_reports_timeout(Duration::from_millis(250)) {
        Ok(()) => true,
        Err(err) => {
            let _ = tx.send(AppEvent::Log(format!(
                "MCU report pause unavailable ({err}); applying control while suppressing reports on the host"
            )));
            false
        }
    }
}

fn tui_tuning_assignments_summary(assignments: &[rseq_host::TuningAssignment]) -> String {
    assignments
        .iter()
        .map(|assignment| {
            format!(
                "{}={}",
                assignment.control.name,
                rseq_host::tuning_control_value_label(&assignment.control, assignment.value)
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn apply_tui_paused_control<T: rseq_link::Transport>(
    assignment: &rseq_host::TuningAssignment,
    host: &mut rseq::link::HostLink<T>,
    report_decoders: &mut ReportDecoderRegistry,
    tx: &Sender<AppEvent>,
) -> Result<Option<rseq_host::ReportScaleUpdate>, String> {
    let control = &assignment.control;
    let len = u16::try_from(control.width.max(1))
        .ok()
        .filter(|len| *len > 0)
        .ok_or_else(|| format!("invalid register width {}", control.width))?;
    if len as usize > rseq_link::wire::CONTROL_MAX_READ_LEN {
        return Err(format!(
            "width {} exceeds control read limit {}",
            len,
            rseq_link::wire::CONTROL_MAX_READ_LEN
        ));
    }
    let read = host.control_read(control.addr, len).map_err(|err| {
        format!(
            "read {} @ 0x{:02x} failed: {err}",
            control.register_name, control.addr
        )
    })?;
    let _ = tx.send(AppEvent::Register {
        addr: read.addr,
        access: AccessKind::Read,
        data: read.data.clone(),
    });
    let data = rseq_host::apply_tuning_control_value(control, &read.data, assignment.value)?;
    if read.data.get(..data.len()) != Some(data.as_slice()) {
        let write = host.control_write(control.addr, &data).map_err(|err| {
            format!(
                "write {} @ 0x{:02x} data=[{}] failed: {err}",
                control.register_name,
                control.addr,
                hex_bytes(&data)
            )
        })?;
        let _ = tx.send(AppEvent::Register {
            addr: write.addr,
            access: AccessKind::Write,
            data,
        });
    }
    let report_scale = rseq_host::tuning_assignment_report_scale(assignment)?;
    if let Some(update) = report_scale {
        report_decoders.apply_scale_update(update);
    }
    report_decoders.mark_stream_reconfigured();
    Ok(report_scale)
}

fn handle_bus_op(op: BusOp, report_decoders: &mut ReportDecoderRegistry, tx: &Sender<AppEvent>) {
    match op {
        BusOp::Read { addr, data } => {
            let _ = tx.send(AppEvent::Register {
                addr,
                access: AccessKind::Read,
                data,
            });
        }
        BusOp::Write { addr, data } => {
            let _ = tx.send(AppEvent::Register {
                addr,
                access: AccessKind::Write,
                data,
            });
        }
        BusOp::Delay { us } => {
            let _ = tx.send(AppEvent::Log(format!("delay {us}us")));
        }
        BusOp::Log { msg } => {
            let _ = tx.send(AppEvent::Log(msg));
        }
        BusOp::Irq { pin } => {
            let _ = tx.send(AppEvent::Log(format!("irq pin {pin}")));
        }
        BusOp::BusSelect { kind, arg } => {
            let _ = tx.send(AppEvent::Log(format!(
                "bus select {} arg=0x{arg:x}",
                kind.as_str()
            )));
        }
        BusOp::Report { meta, kind, args } => {
            handle_report(meta, kind, &args, report_decoders, tx);
        }
    }
}

fn handle_report(
    meta: Option<ReportMeta>,
    kind: u32,
    args: &[ReportArg],
    report_decoders: &mut ReportDecoderRegistry,
    tx: &Sender<AppEvent>,
) {
    let label = report_kind_label(kind);
    let mut line = format!("{label}{}", format_report_meta(meta));
    if kind == rseq::REPORT_KIND_FIFO_RAW {
        let fifo_len = first_report_u32(args);
        let bytes = first_report_bytes(args);
        if report_decoders.take_fifo_discard() {
            let _ = write!(
                line,
                " discarded=reconfiguration-boundary fifo_len={} data_len={}",
                fifo_len
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "?".to_string()),
                bytes.map(<[u8]>::len).unwrap_or(0)
            );
            let _ = tx.send(AppEvent::Report(line));
            return;
        }
        match (bytes, report_decoders.get(kind)) {
            (Some(bytes), Some(ReportDecoder::I16Le(decoder))) => {
                let decoded = decode_i16_le_fifo_samples(bytes, decoder);
                let _ = write!(
                    line,
                    " decoder={} output={} fifo_len={} data_len={} samples={}",
                    decoder.label,
                    decoder.output.as_str(),
                    fifo_len
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "?".to_string()),
                    bytes.len(),
                    decoded.samples.len()
                );
                if decoded.trailing_bytes != 0 {
                    let _ = write!(line, " partial_bytes={}", decoded.trailing_bytes);
                }
                for sample in decoded.samples {
                    if let Some(sample) = sample.to_motion(decoder, meta) {
                        let _ = tx.send(AppEvent::Sample {
                            timestamp_us: sample.timestamp_us,
                            acc: sample.acc,
                            gyro: sample.gyro,
                        });
                    }
                }
            }
            (Some(bytes), None) => {
                let _ = write!(
                    line,
                    " data_len={} raw=[{}]",
                    bytes.len(),
                    hex_preview(bytes, 24)
                );
            }
            _ => {
                let _ = write!(line, " args=[{}]", format_report_args(args));
            }
        }
    } else {
        let args = format_report_args(args);
        if !args.is_empty() {
            let _ = write!(line, " args=[{args}]");
        }
    }
    if let Some(event_kind) = rseq_host::special_event_kind(kind) {
        let _ = tx.send(AppEvent::SpecialEvent(rseq_host::SpecialEvent {
            kind: event_kind,
            meta,
            args: args.to_vec(),
        }));
    }
    let _ = tx.send(AppEvent::Report(line));
}

fn report_kind_label(kind: u32) -> String {
    rseq::report_kind_name(kind).map_or_else(|| format!("kind=0x{kind:x}"), str::to_string)
}

fn format_report_meta(meta: Option<ReportMeta>) -> String {
    match meta {
        Some(meta) if meta.timestamp_valid() => {
            format!(" frame_id={} ts_us={}", meta.frame_id, meta.timestamp_us)
        }
        Some(meta) => format!(" frame_id={}", meta.frame_id),
        None => String::new(),
    }
}

fn format_report_args(args: &[ReportArg]) -> String {
    args.iter()
        .map(|arg| match arg {
            ReportArg::U32(value) => format!("u32=0x{value:08x}"),
            ReportArg::Bytes(bytes) => {
                format!("bytes[{}]=[{}]", bytes.len(), hex_preview(bytes, 16))
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn first_report_u32(args: &[ReportArg]) -> Option<u32> {
    args.iter().find_map(|arg| match arg {
        ReportArg::U32(value) => Some(*value),
        ReportArg::Bytes(_) => None,
    })
}

fn first_report_bytes(args: &[ReportArg]) -> Option<&[u8]> {
    args.iter().find_map(|arg| match arg {
        ReportArg::Bytes(bytes) => Some(bytes.as_slice()),
        ReportArg::U32(_) => None,
    })
}

fn hex_preview(bytes: &[u8], max: usize) -> String {
    let mut out = hex_bytes(&bytes[..bytes.len().min(max)]);
    if bytes.len() > max {
        out.push_str(" ...");
    }
    out
}

fn hex_bytes(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_register_write_input_char(ch: char) -> bool {
    ch.is_ascii_hexdigit()
        || matches!(
            ch,
            'x' | 'X' | ' ' | '\t' | ',' | ';' | '_' | '[' | ']' | '{' | '}'
        )
}

fn is_tuning_input_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-' | '+' | 'x' | 'X')
}

fn parse_register_write_bytes(input: &str) -> Result<Vec<u8>, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("enter at least one byte".to_string());
    }

    let normalized = trimmed
        .chars()
        .map(|ch| match ch {
            ',' | ';' | '[' | ']' | '{' | '}' => ' ',
            _ => ch,
        })
        .collect::<String>();

    let tokens = normalized.split_whitespace().collect::<Vec<_>>();
    if tokens.len() == 1 {
        return parse_register_write_token(tokens[0]);
    }

    let mut out = Vec::with_capacity(tokens.len());
    for token in tokens {
        let bytes = parse_register_write_token(token)?;
        if bytes.len() != 1 {
            return Err(format!("token '{token}' expands to more than one byte"));
        }
        out.extend(bytes);
    }
    Ok(out)
}

fn parse_register_write_token(token: &str) -> Result<Vec<u8>, String> {
    let mut hex = token.trim();
    if hex.is_empty() {
        return Ok(Vec::new());
    }
    if let Some(stripped) = hex.strip_prefix("0x").or_else(|| hex.strip_prefix("0X")) {
        hex = stripped;
    }
    let compact = hex.replace('_', "");
    if compact.is_empty() || !compact.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(format!("invalid hex byte '{token}'"));
    }

    if compact.len() <= 2 {
        let value =
            u8::from_str_radix(&compact, 16).map_err(|_| format!("invalid hex byte '{token}'"))?;
        return Ok(vec![value]);
    }

    let padded = if compact.len() % 2 == 0 {
        compact
    } else {
        format!("0{compact}")
    };
    let mut out = Vec::with_capacity(padded.len() / 2);
    for idx in (0..padded.len()).step_by(2) {
        let byte = u8::from_str_radix(&padded[idx..idx + 2], 16)
            .map_err(|_| format!("invalid hex byte '{token}'"))?;
        out.push(byte);
    }
    Ok(out)
}

#[derive(Debug, Clone, Default)]
struct ReportDecoderRegistry {
    by_kind: HashMap<u32, ReportDecoder>,
    discard_next_fifo_report: bool,
}

impl ReportDecoderRegistry {
    fn insert(&mut self, kind: u32, decoder: ReportDecoder) {
        self.by_kind.insert(kind, decoder);
    }

    fn get(&self, kind: u32) -> Option<&ReportDecoder> {
        self.by_kind.get(&kind)
    }

    fn apply_scale_update(&mut self, update: rseq_host::ReportScaleUpdate) -> usize {
        let mut updated = 0;
        for decoder in self.by_kind.values_mut() {
            match decoder {
                ReportDecoder::I16Le(decoder) => match update.kind {
                    rseq_host::ReportScaleKind::AccelFullScaleG
                        if !decoder.accel_fields.is_empty() =>
                    {
                        decoder.accel_fs_g = update.value;
                        updated += 1;
                    }
                    rseq_host::ReportScaleKind::GyroFullScaleDps
                        if !decoder.gyro_fields.is_empty() =>
                    {
                        decoder.gyro_fs_dps = update.value;
                        updated += 1;
                    }
                    _ => {}
                },
            }
        }
        updated
    }

    fn mark_stream_reconfigured(&mut self) {
        self.discard_next_fifo_report = true;
    }

    fn take_fifo_discard(&mut self) -> bool {
        std::mem::take(&mut self.discard_next_fifo_report)
    }
}

#[derive(Debug, Clone)]
enum ReportDecoder {
    I16Le(I16LeReportDecoder),
}

#[derive(Debug, Clone)]
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

#[derive(Debug, Clone, Copy)]
struct I16LeFieldValue {
    field_index: usize,
    value: i16,
}

#[derive(Debug, Clone)]
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

    fn to_motion(
        &self,
        decoder: &I16LeReportDecoder,
        meta: Option<ReportMeta>,
    ) -> Option<MotionSample> {
        let gyro = [
            scaled_field(self, decoder, decoder.gyro_fields.first()?, ScaleKind::Gyro)?,
            scaled_field(self, decoder, decoder.gyro_fields.get(1)?, ScaleKind::Gyro)?,
            scaled_field(self, decoder, decoder.gyro_fields.get(2)?, ScaleKind::Gyro)?,
        ];
        let acc = [
            scaled_field(
                self,
                decoder,
                decoder.accel_fields.first()?,
                ScaleKind::Accel,
            )?,
            scaled_field(
                self,
                decoder,
                decoder.accel_fields.get(1)?,
                ScaleKind::Accel,
            )?,
            scaled_field(
                self,
                decoder,
                decoder.accel_fields.get(2)?,
                ScaleKind::Accel,
            )?,
        ];
        Some(MotionSample {
            timestamp_us: meta.and_then(|meta| meta.timestamp_valid().then_some(meta.timestamp_us)),
            acc,
            gyro,
        })
    }
}

#[derive(Debug, Clone)]
struct I16LeFifoDecode {
    samples: Vec<I16LeFifoSample>,
    trailing_bytes: usize,
}

#[derive(Debug, Clone, Copy)]
struct MotionSample {
    timestamp_us: Option<u64>,
    acc: [f64; 3],
    gyro: [f64; 3],
}

#[derive(Debug, Clone, Copy)]
enum ScaleKind {
    Accel,
    Gyro,
}

fn scaled_field(
    sample: &I16LeFifoSample,
    decoder: &I16LeReportDecoder,
    field: &str,
    kind: ScaleKind,
) -> Option<f64> {
    let raw = sample.value_by_name(decoder, field)?;
    Some(match kind {
        ScaleKind::Accel => accel_raw_to_m_s2(raw, decoder.accel_fs_g),
        ScaleKind::Gyro => gyro_raw_to_rad_s(raw, decoder.gyro_fs_dps),
    })
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

fn accel_raw_to_m_s2(raw: i16, full_scale_g: f64) -> f64 {
    raw as f64 * full_scale_g * STANDARD_GRAVITY_MPS2 / I16_FULL_SCALE_COUNTS
}

fn gyro_raw_to_rad_s(raw: i16, full_scale_dps: f64) -> f64 {
    raw as f64 * full_scale_dps / I16_FULL_SCALE_COUNTS * std::f64::consts::PI / 180.0
}

fn serial_startup_program(cli: &Cli) -> Result<Option<CompiledProgram>, String> {
    if cli.demo || (cli.serial.is_none() && cli.tcp.is_none()) || cli.watch || cli.file.is_empty() {
        return Ok(None);
    }

    compile_rseq_files(&cli.file).map(Some)
}

struct ParsedStartupSource {
    name: String,
    base_dir: Option<PathBuf>,
    program: rseq::Program,
}

fn compile_rseq_files(files: &[PathBuf]) -> Result<CompiledProgram, String> {
    let mut parsed = Vec::with_capacity(files.len());
    for file in files {
        let source = std::fs::read_to_string(file)
            .map_err(|err| format!("failed to read {}: {err}", file.display()))?;
        let mut program = rseq::parse_detailed(&source).map_err(|errors| {
            let error = errors
                .into_iter()
                .next()
                .expect("parse_detailed returned at least one diagnostic");
            format!(
                "could not parse {}: {} at bytes {}..{}",
                file.display(),
                error.message,
                error.span.start,
                error.span.end
            )
        })?;
        resolve_program_chip_paths(file, &mut program.stmts);
        parsed.push(ParsedStartupSource {
            name: file.display().to_string(),
            base_dir: file.parent().map(Path::to_path_buf),
            program,
        });
    }

    let units = parsed
        .iter()
        .map(|source| ProgramUnit {
            program: &source.program,
            base_dir: source.base_dir.as_deref(),
        })
        .collect::<Vec<_>>();

    rseq::compile_program_units(&units).map_err(|diag| {
        let unit_name = parsed
            .get(diag.unit)
            .map(|source| source.name.as_str())
            .unwrap_or("<unknown source>");
        let help = diag
            .help
            .as_deref()
            .map(|help| format!(" ({help})"))
            .unwrap_or_default();
        format!(
            "could not compile {unit_name}: {} at bytes {}..{}{}",
            diag.message, diag.span.start, diag.span.end, help
        )
    })
}

fn resolve_program_chip_paths(file: &Path, stmts: &mut [rseq::Stmt]) {
    let base_dir = file.parent();
    for stmt in stmts {
        match stmt {
            rseq::Stmt::Chip { path } => {
                let resolved = resolve_host_chip_path(path, base_dir);
                if resolved.exists() {
                    *path = resolved.to_string_lossy().into_owned();
                }
            }
            rseq::Stmt::Irq { arms, .. } => {
                for arm in arms {
                    resolve_program_chip_paths(file, &mut arm.body);
                }
            }
            rseq::Stmt::Repeat { body, .. } => resolve_program_chip_paths(file, body),
            rseq::Stmt::If { then, else_, .. } => {
                resolve_program_chip_paths(file, then);
                resolve_program_chip_paths(file, else_);
            }
            _ => {}
        }
    }
}

fn load_host_metadata(files: &[PathBuf], chips: &[PathBuf]) -> Result<HostMetadata, String> {
    let mut metadata = HostMetadata::default();
    for chip in chips {
        collect_register_dump_map_from_chip(chip, None, &mut metadata.register_dump)
            .map_err(|err| format!("{}: {err}", chip.display()))?;
    }

    for file in files {
        let source = std::fs::read_to_string(file)
            .map_err(|err| format!("failed to read {}: {err}", file.display()))?;
        let program = rseq::parse_detailed(&source).map_err(|errors| {
            let error = errors
                .into_iter()
                .next()
                .expect("parse_detailed returned at least one diagnostic");
            format!(
                "could not parse {}: {} at bytes {}..{}",
                file.display(),
                error.message,
                error.span.start,
                error.span.end
            )
        })?;
        collect_report_decoders(&program.stmts, &mut metadata.report_decoders)
            .map_err(|err| format!("{}: {err}", file.display()))?;
        collect_register_dump_map_from_stmts(file, &program.stmts, &mut metadata.register_dump)
            .map_err(|err| format!("{}: {err}", file.display()))?;
    }
    metadata.tuning_catalog = rseq_host::load_host_metadata(files, chips)?.tuning_catalog;
    Ok(metadata)
}

fn collect_register_dump_map_from_stmts(
    file: &Path,
    stmts: &[rseq::Stmt],
    register_dump: &mut RegisterDumpMap,
) -> Result<(), String> {
    let mut chip_paths = Vec::new();
    collect_chip_paths(stmts, &mut chip_paths);
    let base_dir = file.parent();

    for chip_path in chip_paths {
        collect_register_dump_map_from_chip(Path::new(&chip_path), base_dir, register_dump)?;
    }

    Ok(())
}

fn collect_register_dump_map_from_chip(
    chip_path: &Path,
    base_dir: Option<&Path>,
    register_dump: &mut RegisterDumpMap,
) -> Result<(), String> {
    let resolved = resolve_host_chip_path(&chip_path.to_string_lossy(), base_dir);
    let registry = rseq::ChipRegistry::load(&resolved)
        .map_err(|err| format!("failed to load {}: {err}", resolved.display()))?;
    for chip in registry.chips() {
        for page in &chip.pages {
            for reg in &page.registers {
                register_dump.mark_register(&page.name, reg);
            }
        }
    }
    Ok(())
}

fn resolve_host_chip_path(path: &str, base_dir: Option<&Path>) -> PathBuf {
    let resolved = rseq::resolve_chip_path(path, base_dir);
    if resolved.exists() {
        return resolved;
    }

    let normalized = rseq::normalize_chip_path(path);
    let candidate = PathBuf::from(&normalized);
    if candidate.is_absolute() {
        return candidate;
    }

    if let Some(base_dir) = base_dir {
        for ancestor in base_dir.ancestors() {
            let from_ancestor = ancestor.join(&normalized);
            if from_ancestor.exists() {
                return from_ancestor;
            }
        }
    }

    resolved
}

fn collect_chip_paths(stmts: &[rseq::Stmt], paths: &mut Vec<String>) {
    for stmt in stmts {
        match stmt {
            rseq::Stmt::Chip { path } => paths.push(path.clone()),
            rseq::Stmt::Irq { arms, .. } => {
                for arm in arms {
                    collect_chip_paths(&arm.body, paths);
                }
            }
            rseq::Stmt::Repeat { body, .. } => collect_chip_paths(body, paths),
            rseq::Stmt::If { then, else_, .. } => {
                collect_chip_paths(then, paths);
                collect_chip_paths(else_, paths);
            }
            _ => {}
        }
    }
}

fn collect_report_decoders(
    stmts: &[rseq::Stmt],
    registry: &mut ReportDecoderRegistry,
) -> Result<(), String> {
    for stmt in stmts {
        match stmt {
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
        rseq::Value::Number(value) => Ok(*value),
        rseq::Value::Ident(name) => {
            rseq::report_kind_value(name).ok_or_else(|| format!("unknown report kind '{name}'"))
        }
        _ => Err("report_format! kind must be a number or built-in report kind name".to_string()),
    }
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
            let mut output = ReportOutputMode::PhysicalF32;
            for (name, value) in options {
                match name.as_str() {
                    "fields" => fields = Some(option_ident_array(decoder, name, value)?),
                    "accel_fields" => accel_fields = option_ident_array(decoder, name, value)?,
                    "gyro_fields" => gyro_fields = option_ident_array(decoder, name, value)?,
                    "temp_field" => temp_field = Some(option_ident(decoder, name, value)?),
                    "accel_fs_g" => accel_fs_g = option_number(decoder, name, value)?,
                    "gyro_fs_dps" => gyro_fs_dps = option_number(decoder, name, value)?,
                    "temp_lsb_per_c" => temp_lsb_per_c = option_number(decoder, name, value)?,
                    "temp_offset_c" => temp_offset_c = option_number(decoder, name, value)?,
                    "output" => output = option_output_mode(decoder, name, value)?,
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
            let mut output = ReportOutputMode::PhysicalF32;
            for (name, value) in options {
                match name.as_str() {
                    "accel_fs_g" => accel_fs_g = option_number(decoder, name, value)?,
                    "gyro_fs_dps" => gyro_fs_dps = option_number(decoder, name, value)?,
                    "output" => output = option_output_mode(decoder, name, value)?,
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
    let decoder = I16LeReportDecoder {
        label: label.to_string(),
        fields,
        accel_fields,
        gyro_fields,
        temp_field,
        accel_fs_g,
        gyro_fs_dps,
        temp_lsb_per_c,
        temp_offset_c,
        output,
    };
    decoder.validate()?;
    Ok(ReportDecoder::I16Le(decoder))
}

fn option_number(
    decoder: &str,
    option: &str,
    value: &rseq::ReportOptionValue,
) -> Result<f64, String> {
    match value {
        rseq::ReportOptionValue::Number(value) => Ok(*value as f64),
        _ => Err(format!("{decoder} option '{option}' must be a number")),
    }
}

fn option_ident_array(
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

fn option_ident(
    decoder: &str,
    option: &str,
    value: &rseq::ReportOptionValue,
) -> Result<String, String> {
    match value {
        rseq::ReportOptionValue::Ident(value) => Ok(value.clone()),
        _ => Err(format!("{decoder} option '{option}' must be an identifier")),
    }
}

fn option_output_mode(
    decoder: &str,
    option: &str,
    value: &rseq::ReportOptionValue,
) -> Result<ReportOutputMode, String> {
    let value = option_ident(decoder, option, value)?;
    match value.as_str() {
        "physical_f32" => Ok(ReportOutputMode::PhysicalF32),
        "raw_i16" => Ok(ReportOutputMode::RawI16),
        _ => Err(format!(
            "{decoder} option '{option}' must be physical_f32 or raw_i16, got '{value}'"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_no_dump_registers_from_chip_metadata() {
        let file =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/qmi8660_fifo.rseq");
        let metadata = load_host_metadata(&[file], &[]).expect("load qmi8660 metadata");

        assert!(metadata.register_dump.is_no_dump(0x57));
        assert!(!metadata.register_dump.is_no_dump(0x56));
        assert!(
            metadata
                .register_dump
                .max_addr()
                .is_some_and(|addr| addr >= 0x7d)
        );
    }

    #[test]
    fn loads_register_metadata_from_explicit_chip_option() {
        let chip = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../qmi8660.yaml");
        let metadata = load_host_metadata(&[], &[chip]).expect("load explicit chip metadata");
        let regs = metadata.register_dump.registers_for_addr(0x57);

        assert!(metadata.register_dump.is_no_dump(0x57));
        assert!(regs.iter().any(|reg| reg.name == "FIFO_DATA"));
    }

    #[test]
    fn duplicate_chip_metadata_is_merged() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let file = root.join("examples/qmi8660_fifo.rseq");
        let chip = root.join("qmi8660.yaml");
        let metadata = load_host_metadata(&[file], &[chip]).expect("load duplicate metadata");
        let fifo_regs = metadata
            .register_dump
            .registers_for_addr(0x57)
            .into_iter()
            .filter(|reg| reg.page == "UI" && reg.name == "FIFO_DATA")
            .count();

        assert_eq!(fifo_regs, 1);
    }

    #[test]
    fn compiles_startup_program_for_serial_load_exec() {
        let file =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/qmi8660_fifo.rseq");
        let compiled = compile_rseq_files(&[file]).expect("compile qmi8660 fifo startup program");

        assert!(!compiled.main.is_empty());
        assert!(compiled.irq_bytecodes.contains_key("int1"));
    }

    #[test]
    fn no_dump_reads_are_not_expanded_as_register_bytes() {
        let mut dump = RegisterDumpMap::default();
        dump.mark_dumpability(0x57, 1, false);
        let mut app = App::new(
            "test".to_string(),
            16,
            dump,
            rseq_host::TuningControlCatalog::default(),
            None,
        );

        app.apply(AppEvent::Register {
            addr: 0x57,
            access: AccessKind::Read,
            data: vec![0x11, 0x22, 0x33],
        });

        assert!(app.registers.contains_key(&0x57));
        assert!(!app.registers.contains_key(&0x58));
        assert!(!app.registers.contains_key(&0x59));
    }

    #[test]
    fn motion_event_marker_prefers_nearest_timestamped_sample() {
        let mut app = App::new(
            "test".to_string(),
            16,
            RegisterDumpMap::default(),
            rseq_host::TuningControlCatalog::default(),
            None,
        );
        app.samples.push_back(ImuSample {
            index: 10,
            timestamp_us: Some(100),
            acc: [0.0; 3],
            gyro: [0.0; 3],
        });
        app.samples.push_back(ImuSample {
            index: 11,
            timestamp_us: Some(220),
            acc: [0.0; 3],
            gyro: [0.0; 3],
        });
        app.motion_events.push_back(TuiMotionEvent {
            kind: rseq_host::SpecialEventKind::Amd,
            meta: Some(ReportMeta {
                flags: rseq_link::REPORT_FLAG_TIMESTAMP_VALID,
                frame_id: 1,
                timestamp_us: 200,
            }),
            sample_index: 99,
            args_len: 0,
        });

        let markers = motion_event_marker_series(&app, [10.0, 11.0], [-1.0, 1.0]);

        assert_eq!(markers[rseq_host::SpecialEventKind::Amd.index()].len(), 1);
        assert_eq!(markers[rseq_host::SpecialEventKind::Amd.index()][0].0, 11.0);
    }

    #[test]
    fn motion_event_marker_skips_timestamp_outside_sample_window() {
        let mut app = App::new(
            "test".to_string(),
            16,
            RegisterDumpMap::default(),
            rseq_host::TuningControlCatalog::default(),
            None,
        );
        app.samples.push_back(ImuSample {
            index: 20,
            timestamp_us: Some(500),
            acc: [0.0; 3],
            gyro: [0.0; 3],
        });
        app.samples.push_back(ImuSample {
            index: 21,
            timestamp_us: Some(600),
            acc: [0.0; 3],
            gyro: [0.0; 3],
        });
        app.motion_events.push_back(TuiMotionEvent {
            kind: rseq_host::SpecialEventKind::Amd,
            meta: Some(ReportMeta {
                flags: rseq_link::REPORT_FLAG_TIMESTAMP_VALID,
                frame_id: 1,
                timestamp_us: 200,
            }),
            sample_index: 20,
            args_len: 0,
        });

        let markers = motion_event_marker_series(&app, [20.0, 21.0], [-1.0, 1.0]);

        assert!(markers[rseq_host::SpecialEventKind::Amd.index()].is_empty());
    }

    #[test]
    fn selected_register_dump_uses_yaml_width() {
        let chip = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../qmi8660.yaml");
        let metadata = load_host_metadata(&[], &[chip]).expect("load explicit chip metadata");
        let mut app = App::new(
            "test".to_string(),
            16,
            metadata.register_dump,
            rseq_host::TuningControlCatalog::default(),
            None,
        );
        app.selected_register_addr = 0x54;

        let target = app
            .selected_register_read_target()
            .expect("FIFO_STATUSL is dumpable");
        assert_eq!(target.addr, 0x54);
        assert_eq!(target.len, 1);
        assert!(target.label.contains("FIFO_STATUSL"));
    }

    #[test]
    fn selected_register_dump_rejects_no_dump() {
        let chip = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../qmi8660.yaml");
        let metadata = load_host_metadata(&[], &[chip]).expect("load explicit chip metadata");
        let mut app = App::new(
            "test".to_string(),
            16,
            metadata.register_dump,
            rseq_host::TuningControlCatalog::default(),
            None,
        );
        app.selected_register_addr = 0x57;

        assert!(app.selected_register_read_target().is_err());
    }

    #[test]
    fn parses_register_write_hex_bytes() {
        assert_eq!(
            parse_register_write_bytes("12 0x34,56").unwrap(),
            vec![0x12, 0x34, 0x56]
        );
        assert_eq!(
            parse_register_write_bytes("0x1234").unwrap(),
            vec![0x12, 0x34]
        );
        assert_eq!(parse_register_write_bytes("abc").unwrap(), vec![0x0a, 0xbc]);
    }

    #[test]
    fn selected_register_write_rejects_read_only_yaml_register() {
        let chip = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../qmi8660.yaml");
        let metadata = load_host_metadata(&[], &[chip]).expect("load explicit chip metadata");
        let mut app = App::new(
            "test".to_string(),
            16,
            metadata.register_dump,
            rseq_host::TuningControlCatalog::default(),
            None,
        );
        app.selected_register_addr = 0x00;

        assert!(app.selected_register_write_target().is_err());
    }

    #[test]
    fn selected_register_write_uses_yaml_width_for_rw_register() {
        let chip = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../qmi8660.yaml");
        let metadata = load_host_metadata(&[], &[chip]).expect("load explicit chip metadata");
        let mut app = App::new(
            "test".to_string(),
            16,
            metadata.register_dump,
            rseq_host::TuningControlCatalog::default(),
            None,
        );
        app.selected_register_addr = 0x0b;

        let target = app
            .selected_register_write_target()
            .expect("COMM_CTL is writable");
        assert_eq!(target.addr, 0x0b);
        assert_eq!(target.width, Some(1));
    }

    #[test]
    fn q_closes_register_write_dialog_without_quitting() {
        let mut app = App::new(
            "test".to_string(),
            16,
            RegisterDumpMap::default(),
            rseq_host::TuningControlCatalog::default(),
            None,
        );
        app.register_write_dialog = Some(RegisterWriteDialog {
            target: RegisterWriteTarget {
                addr: 0x0b,
                width: Some(1),
                label: "UI.COMM_CTL".to_string(),
            },
            input: "00".to_string(),
            error: None,
        });

        app.handle_key(KeyCode::Char('q'));

        assert!(app.running);
        assert!(app.register_write_dialog.is_none());
    }

    #[test]
    fn q_closes_register_detail_without_quitting() {
        let mut app = App::new(
            "test".to_string(),
            16,
            RegisterDumpMap::default(),
            rseq_host::TuningControlCatalog::default(),
            None,
        );
        app.tab = 2;
        app.register_detail_open = true;

        app.handle_key(KeyCode::Char('q'));

        assert!(app.running);
        assert!(!app.register_detail_open);
    }

    #[test]
    fn q_quits_when_no_register_overlay_is_open() {
        let mut app = App::new(
            "test".to_string(),
            16,
            RegisterDumpMap::default(),
            rseq_host::TuningControlCatalog::default(),
            None,
        );
        app.tab = 2;

        app.handle_key(KeyCode::Char('q'));

        assert!(!app.running);
    }
}
