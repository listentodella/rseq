use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt::Write as _;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, Sender},
};
use std::thread;
use std::time::{Duration, Instant};

use rseq::trace::{BusOp, ReportArg, ReportMeta};
use rseq::{CompiledProgram, ProgramUnit};

pub const DEFAULT_ACCEL_FULL_SCALE_G: f64 = 16.0;
pub const DEFAULT_GYRO_FULL_SCALE_DPS: f64 = 4096.0;
pub const DEFAULT_TEMP_LSB_PER_C: f64 = 1.0;
pub const DEFAULT_TEMP_OFFSET_C: f64 = 0.0;
pub const STANDARD_GRAVITY_MPS2: f64 = 9.80665;
pub const I16_FULL_SCALE_COUNTS: f64 = 32768.0;
pub const DEFAULT_BAUD: u32 = 115_200;
pub const MAX_TEXT_LINES: usize = 512;
pub const CAPTURE_MAGIC: &[u8] = b"RSEQCAP1\n";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SerialPortInfo {
    pub port_name: String,
    pub label: String,
    pub detail: String,
}

#[derive(Debug, Clone, Default)]
pub struct HostMetadata {
    pub report_decoders: ReportDecoderRegistry,
    pub register_catalog: RegisterCatalog,
}

#[derive(Debug, Clone, Default)]
pub struct RegisterCatalog {
    dumpable_by_addr: BTreeMap<u32, bool>,
    registers: Vec<RegisterInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterInfo {
    pub page: String,
    pub name: String,
    pub addr: u32,
    pub access: String,
    pub width: u32,
    pub desc: String,
    pub no_dump: bool,
    pub no_dump_reason: String,
    pub fields: Vec<FieldInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldInfo {
    pub name: String,
    pub bit_hi: u8,
    pub bit_lo: u8,
    pub desc: String,
    pub event: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterReadTarget {
    pub addr: u32,
    pub len: u16,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterWriteTarget {
    pub addr: u32,
    pub width: Option<usize>,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccessKind {
    Read,
    Write,
}

impl RegisterCatalog {
    pub fn mark_register(&mut self, page: &str, reg: &rseq::Register) {
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

    pub fn mark_dumpability(&mut self, addr: u32, width: u32, dumpable: bool) {
        for offset in 0..width.max(1) {
            self.dumpable_by_addr.insert(addr + offset, dumpable);
        }
    }

    pub fn is_no_dump(&self, addr: u32) -> bool {
        self.dumpable_by_addr
            .get(&addr)
            .is_some_and(|dumpable| !dumpable)
    }

    pub fn max_addr(&self) -> Option<u32> {
        self.dumpable_by_addr
            .keys()
            .next_back()
            .copied()
            .into_iter()
            .chain(
                self.registers
                    .iter()
                    .map(|reg| reg.addr + reg.width.max(1) - 1),
            )
            .max()
    }

    pub fn registers(&self) -> &[RegisterInfo] {
        &self.registers
    }

    pub fn pages(&self) -> Vec<String> {
        let mut pages = Vec::new();
        for reg in &self.registers {
            if !pages.iter().any(|page| page == &reg.page) {
                pages.push(reg.page.clone());
            }
        }
        pages
    }

    pub fn registers_for_page(&self, page: &str) -> Vec<&RegisterInfo> {
        self.registers
            .iter()
            .filter(|reg| reg.page == page)
            .collect()
    }

    pub fn registers_for_addr(&self, addr: u32) -> Vec<&RegisterInfo> {
        self.registers
            .iter()
            .filter(|reg| {
                let width = reg.width.max(1);
                addr >= reg.addr && addr < reg.addr + width
            })
            .collect()
    }

    pub fn registers_for_page_addr(&self, page: &str, addr: u32) -> Vec<&RegisterInfo> {
        self.registers
            .iter()
            .filter(|reg| {
                let width = reg.width.max(1);
                reg.page == page && addr >= reg.addr && addr < reg.addr + width
            })
            .collect()
    }

    pub fn is_no_dump_for_page(&self, page: &str, addr: u32) -> bool {
        let regs = self.registers_for_page_addr(page, addr);
        if regs.is_empty() {
            return false;
        }
        regs.into_iter().any(|reg| reg.no_dump)
    }

    pub fn selected_read_target(&self, addr: u32) -> Result<RegisterReadTarget, String> {
        if self.is_no_dump(addr) {
            return Err(format!("0x{addr:02x} is marked no_dump; read skipped"));
        }

        let regs = self.registers_for_addr(addr);
        let exact = regs.iter().copied().find(|reg| reg.addr == addr);
        let covering = regs.iter().copied().next();
        if let Some(reg) = exact.or(covering) {
            return register_read_target_from_info(reg);
        }

        Ok(RegisterReadTarget {
            addr,
            len: 1,
            label: format!("0x{addr:02x}"),
        })
    }

    pub fn selected_read_target_for_page(
        &self,
        page: &str,
        addr: u32,
    ) -> Result<RegisterReadTarget, String> {
        if self.is_no_dump_for_page(page, addr) {
            return Err(format!(
                "{page}.0x{addr:02x} is marked no_dump; read skipped"
            ));
        }

        let regs = self.registers_for_page_addr(page, addr);
        let exact = regs.iter().copied().find(|reg| reg.addr == addr);
        let covering = regs.iter().copied().next();
        if let Some(reg) = exact.or(covering) {
            return register_read_target_from_info(reg);
        }

        Ok(RegisterReadTarget {
            addr,
            len: 1,
            label: format!("{page}.0x{addr:02x}"),
        })
    }

    pub fn selected_write_target(&self, addr: u32) -> Result<RegisterWriteTarget, String> {
        let regs = self.registers_for_addr(addr);
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

    pub fn selected_write_target_for_page(
        &self,
        page: &str,
        addr: u32,
    ) -> Result<RegisterWriteTarget, String> {
        let regs = self.registers_for_page_addr(page, addr);
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
            return Err(format!("{page}.0x{addr:02x} is read-only; write skipped"));
        }

        Ok(RegisterWriteTarget {
            addr,
            width: Some(1),
            label: format!("{page}.0x{addr:02x}"),
        })
    }

    pub fn decoded_fields(&self, addr: u32, bytes: &[u8]) -> Vec<DecodedField> {
        self.registers_for_addr(addr)
            .into_iter()
            .flat_map(|reg| {
                reg.fields.iter().map(move |field| DecodedField {
                    register: format!("{}.{}", reg.page, reg.name),
                    name: field.name.clone(),
                    bit_hi: field.bit_hi,
                    bit_lo: field.bit_lo,
                    value: extract_field(bytes, field.bit_hi, field.bit_lo),
                    desc: field.desc.clone(),
                    event: field.event.clone(),
                })
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedField {
    pub register: String,
    pub name: String,
    pub bit_hi: u8,
    pub bit_lo: u8,
    pub value: Option<u128>,
    pub desc: String,
    pub event: Option<String>,
}

fn extract_field(bytes: &[u8], bit_hi: u8, bit_lo: u8) -> Option<u128> {
    let need = bit_hi as usize / 8 + 1;
    if bytes.len() < need {
        return None;
    }
    let mut raw = 0u128;
    for (idx, byte) in bytes.iter().take(16).enumerate() {
        raw |= (*byte as u128) << (idx * 8);
    }
    let hi = bit_hi.max(bit_lo);
    let lo = bit_hi.min(bit_lo);
    let width = hi - lo + 1;
    let mask = if width >= 128 {
        u128::MAX
    } else {
        (1u128 << width) - 1
    };
    Some((raw >> lo) & mask)
}

fn register_read_target_from_info(reg: &RegisterInfo) -> Result<RegisterReadTarget, String> {
    if reg.no_dump {
        return Err(format!(
            "{}.{} is marked no_dump; read skipped",
            reg.page, reg.name
        ));
    }

    let len = u16::try_from(reg.width.max(1)).map_err(|_| {
        format!(
            "{}.{} width {} exceeds u16::MAX",
            reg.page, reg.name, reg.width
        )
    })?;
    if len as usize > rseq_link::wire::CONTROL_MAX_READ_LEN {
        return Err(format!(
            "{}.{} width {} exceeds control read limit {}",
            reg.page,
            reg.name,
            len,
            rseq_link::wire::CONTROL_MAX_READ_LEN
        ));
    }

    Ok(RegisterReadTarget {
        addr: reg.addr,
        len,
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

pub fn register_is_writable(access: &str) -> bool {
    access.chars().any(|ch| ch == 'w' || ch == 'W')
}

#[derive(Debug, Clone)]
pub struct RseqSource {
    pub name: String,
    pub source: String,
    pub base_dir: Option<PathBuf>,
}

impl RseqSource {
    pub fn new(
        name: impl Into<String>,
        source: impl Into<String>,
        base_dir: Option<PathBuf>,
    ) -> Self {
        Self {
            name: name.into(),
            source: source.into(),
            base_dir,
        }
    }
}

pub fn load_host_metadata(files: &[PathBuf], chips: &[PathBuf]) -> Result<HostMetadata, String> {
    let sources = files
        .iter()
        .map(|file| {
            let source = std::fs::read_to_string(file)
                .map_err(|err| format!("failed to read {}: {err}", file.display()))?;
            Ok(RseqSource::new(
                file.display().to_string(),
                source,
                file.parent().map(Path::to_path_buf),
            ))
        })
        .collect::<Result<Vec<_>, String>>()?;
    load_host_metadata_from_sources(&sources, chips)
}

pub fn load_host_metadata_from_sources(
    sources: &[RseqSource],
    chips: &[PathBuf],
) -> Result<HostMetadata, String> {
    let mut metadata = HostMetadata::default();
    for chip in chips {
        collect_register_catalog_from_chip(chip, None, &mut metadata.register_catalog)
            .map_err(|err| format!("{}: {err}", chip.display()))?;
    }

    for source in sources {
        let program = rseq::parse_detailed(&source.source).map_err(|errors| {
            let error = errors
                .into_iter()
                .next()
                .expect("parse_detailed returned at least one diagnostic");
            format!(
                "could not parse {}: {} at bytes {}..{}",
                source.name, error.message, error.span.start, error.span.end
            )
        })?;
        collect_report_decoders(&program.stmts, &mut metadata.report_decoders)
            .map_err(|err| format!("{}: {err}", source.name))?;
        collect_register_catalog_from_stmts(
            source.base_dir.as_deref(),
            &program.stmts,
            &mut metadata.register_catalog,
        )
        .map_err(|err| format!("{}: {err}", source.name))?;
    }
    Ok(metadata)
}

pub fn compile_rseq_files(files: &[PathBuf]) -> Result<CompiledProgram, String> {
    let sources = files
        .iter()
        .map(|file| {
            let source = std::fs::read_to_string(file)
                .map_err(|err| format!("failed to read {}: {err}", file.display()))?;
            Ok(RseqSource::new(
                file.display().to_string(),
                source,
                file.parent().map(Path::to_path_buf),
            ))
        })
        .collect::<Result<Vec<_>, String>>()?;
    compile_rseq_sources(&sources)
}

pub fn compile_rseq_sources(sources: &[RseqSource]) -> Result<CompiledProgram, String> {
    let mut parsed = Vec::with_capacity(sources.len());
    for source in sources {
        let mut program = rseq::parse_detailed(&source.source).map_err(|errors| {
            let error = errors
                .into_iter()
                .next()
                .expect("parse_detailed returned at least one diagnostic");
            format!(
                "could not parse {}: {} at bytes {}..{}",
                source.name, error.message, error.span.start, error.span.end
            )
        })?;
        resolve_program_chip_paths(source.base_dir.as_deref(), &mut program.stmts);
        parsed.push(ParsedStartupSource {
            name: source.name.clone(),
            base_dir: source.base_dir.clone(),
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

struct ParsedStartupSource {
    name: String,
    base_dir: Option<PathBuf>,
    program: rseq::Program,
}

fn resolve_program_chip_paths(base_dir: Option<&Path>, stmts: &mut [rseq::Stmt]) {
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
                    resolve_program_chip_paths(base_dir, &mut arm.body);
                }
            }
            rseq::Stmt::Repeat { body, .. } => resolve_program_chip_paths(base_dir, body),
            rseq::Stmt::If { then, else_, .. } => {
                resolve_program_chip_paths(base_dir, then);
                resolve_program_chip_paths(base_dir, else_);
            }
            _ => {}
        }
    }
}

fn collect_register_catalog_from_stmts(
    base_dir: Option<&Path>,
    stmts: &[rseq::Stmt],
    register_catalog: &mut RegisterCatalog,
) -> Result<(), String> {
    let mut chip_paths = Vec::new();
    collect_chip_paths(stmts, &mut chip_paths);

    for chip_path in chip_paths {
        collect_register_catalog_from_chip(Path::new(&chip_path), base_dir, register_catalog)?;
    }

    Ok(())
}

fn collect_register_catalog_from_chip(
    chip_path: &Path,
    base_dir: Option<&Path>,
    register_catalog: &mut RegisterCatalog,
) -> Result<(), String> {
    let resolved = resolve_host_chip_path(&chip_path.to_string_lossy(), base_dir);
    let registry = rseq::ChipRegistry::load(&resolved)
        .map_err(|err| format!("failed to load {}: {err}", resolved.display()))?;
    for chip in registry.chips() {
        for page in &chip.pages {
            for reg in &page.registers {
                register_catalog.mark_register(&page.name, reg);
            }
        }
    }
    Ok(())
}

pub fn resolve_host_chip_path(path: &str, base_dir: Option<&Path>) -> PathBuf {
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

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReportDecoderRegistry {
    by_kind: HashMap<u32, ReportDecoder>,
}

impl ReportDecoderRegistry {
    pub fn insert(&mut self, kind: u32, decoder: ReportDecoder) {
        self.by_kind.insert(kind, decoder);
    }

    pub fn get(&self, kind: u32) -> Option<&ReportDecoder> {
        self.by_kind.get(&kind)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&u32, &ReportDecoder)> {
        self.by_kind.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.by_kind.is_empty()
    }

    pub fn len(&self) -> usize {
        self.by_kind.len()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ReportDecoder {
    I16Le(I16LeReportDecoder),
}

#[derive(Debug, Clone, PartialEq)]
pub struct I16LeReportDecoder {
    pub label: String,
    pub fields: Vec<String>,
    pub accel_fields: Vec<String>,
    pub gyro_fields: Vec<String>,
    pub temp_field: Option<String>,
    pub accel_fs_g: f64,
    pub gyro_fs_dps: f64,
    pub temp_lsb_per_c: f64,
    pub temp_offset_c: f64,
    pub output: ReportOutputMode,
}

impl I16LeReportDecoder {
    pub fn sample_bytes(&self) -> usize {
        self.fields.len() * 2
    }

    pub fn validate(&self) -> Result<(), String> {
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
pub enum ReportOutputMode {
    PhysicalF32,
    RawI16,
}

impl ReportOutputMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PhysicalF32 => "physical_f32",
            Self::RawI16 => "raw_i16",
        }
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
            let mut accel_fs_g = DEFAULT_ACCEL_FULL_SCALE_G;
            let mut gyro_fs_dps = DEFAULT_GYRO_FULL_SCALE_DPS;
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
            let mut accel_fs_g = DEFAULT_ACCEL_FULL_SCALE_G;
            let mut gyro_fs_dps = DEFAULT_GYRO_FULL_SCALE_DPS;
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

pub fn make_i16_le_decoder(
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct I16LeFieldValue {
    pub field_index: usize,
    pub value: i16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct I16LeFifoSample {
    pub values: Vec<I16LeFieldValue>,
}

impl I16LeFifoSample {
    pub fn value_by_name(&self, decoder: &I16LeReportDecoder, name: &str) -> Option<i16> {
        self.values
            .iter()
            .find(|value| decoder.fields[value.field_index] == name)
            .map(|value| value.value)
    }

    pub fn to_motion(
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
        let temp_c = decoder
            .temp_field
            .as_deref()
            .and_then(|field| self.value_by_name(decoder, field))
            .map(|raw| temp_raw_to_c(raw, decoder.temp_lsb_per_c, decoder.temp_offset_c));
        Some(MotionSample {
            timestamp_us: meta.and_then(|meta| meta.timestamp_valid().then_some(meta.timestamp_us)),
            acc,
            gyro,
            temp_c,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct I16LeFifoDecode {
    pub samples: Vec<I16LeFifoSample>,
    pub trailing_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MotionSample {
    pub timestamp_us: Option<u64>,
    pub acc: [f64; 3],
    pub gyro: [f64; 3],
    pub temp_c: Option<f64>,
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

pub fn decode_i16_le_fifo_samples(data: &[u8], decoder: &I16LeReportDecoder) -> I16LeFifoDecode {
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

pub fn accel_raw_to_m_s2(raw: i16, full_scale_g: f64) -> f64 {
    raw as f64 * full_scale_g * STANDARD_GRAVITY_MPS2 / I16_FULL_SCALE_COUNTS
}

pub fn gyro_raw_to_rad_s(raw: i16, full_scale_dps: f64) -> f64 {
    raw as f64 * full_scale_dps / I16_FULL_SCALE_COUNTS * std::f64::consts::PI / 180.0
}

pub fn temp_raw_to_c(raw: i16, lsb_per_c: f64, offset_c: f64) -> f64 {
    raw as f64 / lsb_per_c + offset_c
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReportHealth {
    pub total_reports: u64,
    pub dropped_frames: u64,
    pub duplicate_frames: u64,
    pub out_of_order_frames: u64,
    pub last_frame_id: Option<u32>,
    pub last_timestamp_us: Option<u64>,
    pub last_dt_us: Option<i64>,
}

#[derive(Debug, Clone, Default)]
pub struct ReportHealthTracker {
    health: ReportHealth,
}

impl ReportHealthTracker {
    pub fn observe(&mut self, meta: Option<ReportMeta>) -> ReportHealth {
        self.health.total_reports += 1;
        if let Some(meta) = meta {
            if let Some(last) = self.health.last_frame_id {
                if meta.frame_id == last {
                    self.health.duplicate_frames += 1;
                } else if meta.frame_id > last {
                    self.health.dropped_frames += (meta.frame_id - last).saturating_sub(1) as u64;
                } else {
                    self.health.out_of_order_frames += 1;
                }
            }

            if meta.timestamp_valid() {
                self.health.last_dt_us = self
                    .health
                    .last_timestamp_us
                    .map(|last| meta.timestamp_us as i64 - last as i64);
                self.health.last_timestamp_us = Some(meta.timestamp_us);
            }
            self.health.last_frame_id = Some(meta.frame_id);
        }
        self.health
    }

    pub fn health(&self) -> ReportHealth {
        self.health
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReportSummary {
    pub meta: Option<ReportMeta>,
    pub kind: u32,
    pub args: Vec<ReportArg>,
    pub label: String,
    pub line: String,
    pub fifo_len: Option<u32>,
    pub data_len: Option<usize>,
    pub sample_count: usize,
    pub trailing_bytes: usize,
    pub health: ReportHealth,
}

#[derive(Debug, Clone)]
pub struct ReportProcessor {
    decoders: ReportDecoderRegistry,
    health: ReportHealthTracker,
}

impl ReportProcessor {
    pub fn new(decoders: ReportDecoderRegistry) -> Self {
        Self {
            decoders,
            health: ReportHealthTracker::default(),
        }
    }

    pub fn decoders(&self) -> &ReportDecoderRegistry {
        &self.decoders
    }

    pub fn health(&self) -> ReportHealth {
        self.health.health()
    }

    pub fn handle_bus_op(&mut self, op: BusOp) -> Vec<SessionEvent> {
        match op {
            BusOp::Read { addr, data } => vec![SessionEvent::Register {
                addr,
                access: AccessKind::Read,
                data,
            }],
            BusOp::Write { addr, data } => vec![SessionEvent::Register {
                addr,
                access: AccessKind::Write,
                data,
            }],
            BusOp::Delay { us } => vec![SessionEvent::Log(format!("delay {us}us"))],
            BusOp::Log { msg } => vec![SessionEvent::Log(msg)],
            BusOp::Irq { pin } => vec![SessionEvent::Log(format!("irq pin {pin}"))],
            BusOp::BusSelect { kind, arg } => {
                vec![SessionEvent::Log(format!(
                    "bus select {} arg=0x{arg:x}",
                    kind.as_str()
                ))]
            }
            BusOp::Report { meta, kind, args } => self.handle_report(meta, kind, &args),
        }
    }

    pub fn handle_report(
        &mut self,
        meta: Option<ReportMeta>,
        kind: u32,
        args: &[ReportArg],
    ) -> Vec<SessionEvent> {
        let health = self.health.observe(meta);
        let label = report_kind_label(kind);
        let mut line = format!("{label}{}", format_report_meta(meta));
        let mut events = Vec::new();
        let mut summary = ReportSummary {
            meta,
            kind,
            args: args.to_vec(),
            label,
            line: String::new(),
            fifo_len: None,
            data_len: None,
            sample_count: 0,
            trailing_bytes: 0,
            health,
        };

        if kind == rseq::REPORT_KIND_FIFO_RAW {
            let fifo_len = first_report_u32(args);
            let bytes = first_report_bytes(args);
            summary.fifo_len = fifo_len;
            summary.data_len = bytes.map(<[u8]>::len);
            match (bytes, self.decoders.get(kind)) {
                (Some(bytes), Some(ReportDecoder::I16Le(decoder))) => {
                    let decoded = decode_i16_le_fifo_samples(bytes, decoder);
                    summary.sample_count = decoded.samples.len();
                    summary.trailing_bytes = decoded.trailing_bytes;
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
                            events.push(SessionEvent::Sample(sample));
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

        summary.line = line;
        events.push(SessionEvent::Report(summary.clone()));
        events.push(SessionEvent::Health(health));
        events
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportCaptureRecord {
    pub meta: Option<ReportMeta>,
    pub kind: u32,
    pub args: Vec<ReportArg>,
}

pub struct BinaryReportCaptureWriter {
    file: std::fs::File,
}

impl BinaryReportCaptureWriter {
    pub fn create(path: &Path) -> Result<Self, String> {
        let mut file = std::fs::File::create(path)
            .map_err(|e| format!("failed to create capture {}: {e}", path.display()))?;
        file.write_all(CAPTURE_MAGIC)
            .map_err(|e| format!("failed to write capture header {}: {e}", path.display()))?;
        Ok(Self { file })
    }

    pub fn write_report(
        &mut self,
        meta: Option<ReportMeta>,
        kind: u32,
        args: &[ReportArg],
    ) -> Result<(), String> {
        self.write_record(&ReportCaptureRecord {
            meta,
            kind,
            args: args.to_vec(),
        })
    }

    pub fn write_record(&mut self, record: &ReportCaptureRecord) -> Result<(), String> {
        let payload = encode_capture_record(record)?;
        let len = payload.len() as u32;
        self.file
            .write_all(&len.to_le_bytes())
            .and_then(|_| self.file.write_all(&payload))
            .map_err(|e| format!("failed to write capture record: {e}"))
    }
}

pub fn write_report_capture(path: &Path, records: &[ReportCaptureRecord]) -> Result<(), String> {
    let mut writer = BinaryReportCaptureWriter::create(path)?;
    for record in records {
        writer.write_record(record)?;
    }
    Ok(())
}

pub fn read_report_capture(path: &Path) -> Result<Vec<ReportCaptureRecord>, String> {
    let mut file = std::fs::File::open(path)
        .map_err(|e| format!("failed to open capture {}: {e}", path.display()))?;
    let mut magic = vec![0u8; CAPTURE_MAGIC.len()];
    file.read_exact(&mut magic)
        .map_err(|e| format!("failed to read capture header {}: {e}", path.display()))?;
    if magic != CAPTURE_MAGIC {
        return Err(format!("{} is not an rseq report capture", path.display()));
    }

    let mut records = Vec::new();
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
        records.push(decode_capture_record(&payload)?);
    }
    Ok(records)
}

pub fn encode_capture_record(record: &ReportCaptureRecord) -> Result<Vec<u8>, String> {
    if record.args.len() > u8::MAX as usize {
        return Err(format!(
            "capture report has {} args, max is {}",
            record.args.len(),
            u8::MAX
        ));
    }

    let mut payload = Vec::new();
    payload.extend_from_slice(&record.kind.to_le_bytes());
    match record.meta {
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
    payload.push(record.args.len() as u8);
    for arg in &record.args {
        match arg {
            ReportArg::U32(value) => {
                payload.push(rseq_link::wire::REPORT_ARG_U32);
                payload.extend_from_slice(&value.to_le_bytes());
            }
            ReportArg::Bytes(bytes) => {
                let len = u32::try_from(bytes.len()).map_err(|_| {
                    format!("capture bytes arg is too large: {} byte(s)", bytes.len())
                })?;
                payload.push(rseq_link::wire::REPORT_ARG_BYTES);
                payload.extend_from_slice(&len.to_le_bytes());
                payload.extend_from_slice(bytes);
            }
        }
    }
    Ok(payload)
}

pub fn decode_capture_record(payload: &[u8]) -> Result<ReportCaptureRecord, String> {
    let mut pos = 0usize;
    let kind = take_u32(payload, &mut pos)?;
    let meta_present = take_u8(payload, &mut pos)? != 0;
    let flags = take_u8(payload, &mut pos)?;
    let frame_id = take_u32(payload, &mut pos)?;
    let timestamp_us = take_u64(payload, &mut pos)?;
    let meta = meta_present.then_some(ReportMeta {
        flags,
        frame_id,
        timestamp_us,
    });
    let argc = take_u8(payload, &mut pos)? as usize;
    let mut args = Vec::with_capacity(argc);
    for _ in 0..argc {
        match take_u8(payload, &mut pos)? {
            rseq_link::wire::REPORT_ARG_U32 => {
                args.push(ReportArg::U32(take_u32(payload, &mut pos)?));
            }
            rseq_link::wire::REPORT_ARG_BYTES => {
                let len = take_u32(payload, &mut pos)? as usize;
                let bytes = take_bytes(payload, &mut pos, len)?.to_vec();
                args.push(ReportArg::Bytes(bytes));
            }
            tag => return Err(format!("invalid capture arg tag 0x{tag:02x}")),
        }
    }
    Ok(ReportCaptureRecord { meta, kind, args })
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

pub fn report_kind_label(kind: u32) -> String {
    rseq::report_kind_name(kind).map_or_else(|| format!("kind=0x{kind:x}"), str::to_string)
}

pub fn format_report_meta(meta: Option<ReportMeta>) -> String {
    match meta {
        Some(meta) if meta.timestamp_valid() => {
            format!(" frame_id={} ts_us={}", meta.frame_id, meta.timestamp_us)
        }
        Some(meta) => format!(" frame_id={}", meta.frame_id),
        None => String::new(),
    }
}

pub fn format_report_args(args: &[ReportArg]) -> String {
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

pub fn first_report_u32(args: &[ReportArg]) -> Option<u32> {
    args.iter().find_map(|arg| match arg {
        ReportArg::U32(value) => Some(*value),
        ReportArg::Bytes(_) => None,
    })
}

pub fn first_report_bytes(args: &[ReportArg]) -> Option<&[u8]> {
    args.iter().find_map(|arg| match arg {
        ReportArg::Bytes(bytes) => Some(bytes.as_slice()),
        ReportArg::U32(_) => None,
    })
}

pub fn hex_bytes(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn hex_preview(bytes: &[u8], max: usize) -> String {
    let mut out = hex_bytes(&bytes[..bytes.len().min(max)]);
    if bytes.len() > max {
        let _ = write!(out, " ... +{}B", bytes.len() - max);
    }
    out
}

pub fn parse_register_write_bytes(input: &str) -> Result<Vec<u8>, String> {
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

#[derive(Debug, Clone)]
pub enum SessionCommand {
    Ping,
    StopReports,
    ResetMcu,
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
    Shutdown,
}

#[derive(Debug, Clone)]
pub enum SessionEvent {
    Connected {
        label: String,
    },
    Disconnected,
    Log(String),
    Error(String),
    ExecStatus(String),
    Register {
        addr: u32,
        access: AccessKind,
        data: Vec<u8>,
    },
    Sample(MotionSample),
    Report(ReportSummary),
    Health(ReportHealth),
}

#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub serial: Option<String>,
    pub baud: u32,
    pub watch: bool,
    pub demo: bool,
    pub startup_program: Option<CompiledProgram>,
    pub report_decoders: ReportDecoderRegistry,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            serial: None,
            baud: DEFAULT_BAUD,
            watch: false,
            demo: true,
            startup_program: None,
            report_decoders: ReportDecoderRegistry::default(),
        }
    }
}

pub struct SessionHandle {
    pub commands: Sender<SessionCommand>,
    pub events: Receiver<SessionEvent>,
    stop: Arc<AtomicBool>,
}

impl SessionHandle {
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.commands.send(SessionCommand::Shutdown);
    }
}

impl Drop for SessionHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn spawn_session(config: SessionConfig) -> SessionHandle {
    let (event_tx, event_rx) = mpsc::channel();
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));

    if config.demo || config.serial.is_none() {
        spawn_demo_session(event_tx, cmd_rx, stop.clone());
    } else {
        spawn_serial_session(config, event_tx, cmd_rx, stop.clone());
    }

    SessionHandle {
        commands: cmd_tx,
        events: event_rx,
        stop,
    }
}

#[cfg(feature = "serial")]
pub fn available_serial_ports() -> Vec<SerialPortInfo> {
    rseq_link::SerialTransport::available_ports()
        .into_iter()
        .map(|port| {
            let label = serial_port_label(&port);
            let detail = serial_port_detail(&port);
            SerialPortInfo {
                port_name: port.port_name,
                label,
                detail,
            }
        })
        .collect()
}

#[cfg(not(feature = "serial"))]
pub fn available_serial_ports() -> Vec<SerialPortInfo> {
    Vec::new()
}

#[cfg(feature = "serial")]
fn serial_port_label(port: &rseq_link::SerialPortInfo) -> String {
    let name = Path::new(&port.port_name)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(&port.port_name);
    match port.product.as_deref() {
        Some(product) if !product.is_empty() => format!("{name} - {product}"),
        _ => name.to_string(),
    }
}

#[cfg(feature = "serial")]
fn serial_port_detail(port: &rseq_link::SerialPortInfo) -> String {
    let mut parts = vec![port.port_type.clone()];
    if let (Some(vid), Some(pid)) = (port.vid, port.pid) {
        parts.push(format!("{vid:04x}:{pid:04x}"));
    }
    if let Some(manufacturer) = port
        .manufacturer
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        parts.push(manufacturer.to_string());
    }
    if let Some(serial) = port
        .serial_number
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        parts.push(format!("sn={serial}"));
    }
    parts.join(" ")
}

fn spawn_demo_session(
    tx: Sender<SessionEvent>,
    cmd_rx: Receiver<SessionCommand>,
    stop: Arc<AtomicBool>,
) {
    thread::spawn(move || {
        let _ = tx.send(SessionEvent::Connected {
            label: "demo".to_string(),
        });
        let mut regs = [0u8; 256];
        regs[0] = 0x06;
        let started = Instant::now();
        let mut index = 0u64;
        while !stop.load(Ordering::Relaxed) {
            while let Ok(cmd) = cmd_rx.try_recv() {
                match cmd {
                    SessionCommand::Ping => {
                        let _ = tx.send(SessionEvent::Log("demo ping ok".to_string()));
                    }
                    SessionCommand::StopReports => {
                        let _ = tx.send(SessionEvent::Log("demo stop reports ok".to_string()));
                    }
                    SessionCommand::ResetMcu => {
                        regs = [0u8; 256];
                        regs[0] = 0x06;
                        let _ = tx.send(SessionEvent::Log("demo reset ok".to_string()));
                    }
                    SessionCommand::ReadRegister { addr, len, label } => {
                        let data = (0..len as usize)
                            .map(|offset| regs[(addr as usize + offset) & 0xff])
                            .collect::<Vec<_>>();
                        let _ = tx.send(SessionEvent::Register {
                            addr,
                            access: AccessKind::Read,
                            data: data.clone(),
                        });
                        let _ = tx.send(SessionEvent::Log(format!(
                            "demo dump {label} @ 0x{addr:02x}: [{}]",
                            hex_bytes(&data)
                        )));
                    }
                    SessionCommand::WriteRegister { addr, data, label } => {
                        for (offset, byte) in data.iter().enumerate() {
                            regs[(addr as usize + offset) & 0xff] = *byte;
                        }
                        let _ = tx.send(SessionEvent::Register {
                            addr,
                            access: AccessKind::Write,
                            data: data.clone(),
                        });
                        let _ = tx.send(SessionEvent::Log(format!(
                            "demo write {label} @ 0x{addr:02x}: [{}]",
                            hex_bytes(&data)
                        )));
                    }
                    SessionCommand::Shutdown => {
                        stop.store(true, Ordering::Relaxed);
                    }
                }
            }

            let t = started.elapsed().as_secs_f64();
            let phase = t * std::f64::consts::TAU;
            let sample = MotionSample {
                timestamp_us: Some(started.elapsed().as_micros() as u64),
                acc: [
                    phase.sin() * 2.0,
                    (phase * 0.7).cos() * 1.5,
                    STANDARD_GRAVITY_MPS2 + (phase * 0.3).sin() * 0.2,
                ],
                gyro: [
                    (phase * 1.7).sin() * 0.5,
                    (phase * 1.3).cos() * 0.4,
                    (phase * 0.9).sin() * 0.3,
                ],
                temp_c: Some(28.0 + (phase * 0.18).sin() * 1.5),
            };
            let meta = ReportMeta {
                flags: rseq_link::REPORT_FLAG_TIMESTAMP_VALID,
                frame_id: index as u32,
                timestamp_us: sample.timestamp_us.unwrap_or_default(),
            };
            let health = ReportHealth {
                total_reports: index + 1,
                last_frame_id: Some(index as u32),
                last_timestamp_us: sample.timestamp_us,
                ..ReportHealth::default()
            };
            let summary = ReportSummary {
                meta: Some(meta),
                kind: rseq::REPORT_KIND_FIFO_RAW,
                args: vec![
                    ReportArg::U32(12),
                    ReportArg::Bytes(vec![0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x00, 0x08]),
                ],
                label: "FIFO_RAW".to_string(),
                line: format!(
                    "FIFO_RAW frame_id={} ts_us={} demo sample",
                    meta.frame_id, meta.timestamp_us
                ),
                fifo_len: Some(12),
                data_len: Some(12),
                sample_count: 1,
                trailing_bytes: 0,
                health,
            };
            let _ = tx.send(SessionEvent::Sample(sample));
            let _ = tx.send(SessionEvent::Report(summary));
            let _ = tx.send(SessionEvent::Health(health));
            index += 1;
            thread::sleep(Duration::from_millis(33));
        }
        let _ = tx.send(SessionEvent::Disconnected);
    });
}

#[cfg(feature = "serial")]
fn spawn_serial_session(
    config: SessionConfig,
    tx: Sender<SessionEvent>,
    cmd_rx: Receiver<SessionCommand>,
    stop: Arc<AtomicBool>,
) {
    thread::spawn(move || {
        let serial = config.serial.expect("serial checked by caller");
        let startup_program = if config.watch {
            None
        } else {
            config.startup_program
        };
        let mode = if startup_program.is_some() {
            "load+exec"
        } else {
            "watch"
        };
        let label = format!("{serial} @ {} ({mode})", config.baud);
        let _ = tx.send(SessionEvent::Log(format!("opening serial {label}")));
        let transport = if startup_program.is_some() {
            rseq_link::SerialTransport::open(&serial, config.baud)
        } else {
            rseq_link::SerialTransport::open_observing(&serial, config.baud)
        };
        let transport = match transport {
            Ok(transport) => transport,
            Err(err) => {
                let _ = tx.send(SessionEvent::Error(format!("open serial failed: {err}")));
                return;
            }
        };
        let mut host = rseq::link::HostLink::new(transport);
        let mut processor = ReportProcessor::new(config.report_decoders);

        if let Some(program) = startup_program {
            if !load_and_exec_serial_program(&mut host, &program, &mut processor, &tx) {
                return;
            }
        } else {
            let _ = tx.send(SessionEvent::Log(
                "watch mode: no LOAD/EXEC frames will be sent".to_string(),
            ));
        }

        let _ = tx.send(SessionEvent::Connected { label });
        while !stop.load(Ordering::Relaxed) {
            while let Ok(cmd) = cmd_rx.try_recv() {
                if matches!(cmd, SessionCommand::Shutdown) {
                    stop.store(true, Ordering::Relaxed);
                    break;
                }
                handle_source_command(cmd, &mut host, &mut processor, &tx);
            }

            match host.observe_next_trace(Duration::from_millis(20)) {
                Ok(Some(op)) => send_processed_events(processor.handle_bus_op(op), &tx),
                Ok(None) => {}
                Err(err) => {
                    let _ = tx.send(SessionEvent::Error(format!("observe failed: {err}")));
                    thread::sleep(Duration::from_millis(250));
                }
            }
        }
        let _ = tx.send(SessionEvent::Disconnected);
    });
}

#[cfg(not(feature = "serial"))]
fn spawn_serial_session(
    config: SessionConfig,
    tx: Sender<SessionEvent>,
    _cmd_rx: Receiver<SessionCommand>,
    _stop: Arc<AtomicBool>,
) {
    let serial = config.serial.unwrap_or_else(|| "<none>".to_string());
    let _ = tx.send(SessionEvent::Error(format!(
        "serial support is disabled for {serial}; rebuild with --features serial"
    )));
}

#[cfg(feature = "serial")]
fn load_and_exec_serial_program(
    host: &mut rseq::link::HostLink<rseq_link::SerialTransport>,
    program: &CompiledProgram,
    processor: &mut ReportProcessor,
    tx: &Sender<SessionEvent>,
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
        let _ = tx.send(SessionEvent::Log(format!(
            "compiled irq!({pin}) but this transport maps only int1; segment skipped"
        )));
    }

    let _ = tx.send(SessionEvent::Log(format!(
        "loading rseq main={} byte(s), irq_handlers={}",
        program.main.len(),
        program.irq_bytecodes.len()
    )));
    if let Err(err) = host.load_segments(&segments) {
        let _ = tx.send(SessionEvent::Error(format!("LOAD failed: {err}")));
        return false;
    }
    let _ = tx.send(SessionEvent::Log("LOAD ok".to_string()));

    match host.exec() {
        Ok(result) => {
            let _ = tx.send(SessionEvent::ExecStatus(format!("{:?}", result.status)));
            for op in result.traces {
                send_processed_events(processor.handle_bus_op(op), tx);
            }
            true
        }
        Err(err) => {
            let _ = tx.send(SessionEvent::Error(format!("EXEC failed: {err}")));
            false
        }
    }
}

#[cfg(feature = "serial")]
fn handle_source_command(
    cmd: SessionCommand,
    host: &mut rseq::link::HostLink<rseq_link::SerialTransport>,
    processor: &mut ReportProcessor,
    tx: &Sender<SessionEvent>,
) {
    match cmd {
        SessionCommand::Ping => match host.ping() {
            Ok(()) => {
                let _ = tx.send(SessionEvent::Log("ping ok".to_string()));
            }
            Err(err) => {
                let _ = tx.send(SessionEvent::Error(format!("ping failed: {err}")));
            }
        },
        SessionCommand::StopReports => match host.stop_reports() {
            Ok(()) => {
                let _ = tx.send(SessionEvent::Log("stop reports ok".to_string()));
            }
            Err(err) => {
                let _ = tx.send(SessionEvent::Error(format!("stop reports failed: {err}")));
            }
        },
        SessionCommand::ResetMcu => match host.reset() {
            Ok(()) => {
                let _ = tx.send(SessionEvent::Log("reset ok".to_string()));
            }
            Err(err) => {
                let _ = tx.send(SessionEvent::Error(format!("reset failed: {err}")));
            }
        },
        SessionCommand::ReadRegister { addr, len, label } => {
            let result = host.control_read_observing(addr, len, Duration::from_secs(2), |op| {
                send_processed_events(processor.handle_bus_op(op), tx)
            });
            match result {
                Ok(result) => {
                    let _ = tx.send(SessionEvent::Register {
                        addr: result.addr,
                        access: AccessKind::Read,
                        data: result.data.clone(),
                    });
                    let _ = tx.send(SessionEvent::Log(format!(
                        "dump {} @ 0x{:02x} len={}: [{}]",
                        label,
                        result.addr,
                        result.data.len(),
                        hex_bytes(&result.data)
                    )));
                }
                Err(err) => {
                    let _ = tx.send(SessionEvent::Error(format!(
                        "dump {label} @ 0x{addr:02x} len={len} failed: {err}"
                    )));
                }
            }
        }
        SessionCommand::WriteRegister { addr, data, label } => {
            let result = host.control_write_observing(addr, &data, Duration::from_secs(2), |op| {
                send_processed_events(processor.handle_bus_op(op), tx)
            });
            match result {
                Ok(result) => {
                    let _ = tx.send(SessionEvent::Register {
                        addr: result.addr,
                        access: AccessKind::Write,
                        data: data.clone(),
                    });
                    let _ = tx.send(SessionEvent::Log(format!(
                        "write {} @ 0x{:02x} len={}: [{}]",
                        label,
                        result.addr,
                        result.len,
                        hex_bytes(&data)
                    )));
                }
                Err(err) => {
                    let _ = tx.send(SessionEvent::Error(format!(
                        "write {label} @ 0x{addr:02x} data=[{}] failed: {err}",
                        hex_bytes(&data)
                    )));
                }
            }
        }
        SessionCommand::Shutdown => {}
    }
}

fn send_processed_events(events: Vec<SessionEvent>, tx: &Sender<SessionEvent>) {
    for event in events {
        let _ = tx.send(event);
    }
}

pub fn push_bounded<T>(queue: &mut VecDeque<T>, value: T, cap: usize) {
    if cap == 0 {
        return;
    }
    if queue.len() == cap {
        queue.pop_front();
    }
    queue.push_back(value);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64};

    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

    fn temp_dir() -> PathBuf {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("rseq-host-test-{id}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_fixture(dir: &Path) -> (PathBuf, PathBuf) {
        let chip = dir.join("qmi8660.yaml");
        let script = dir.join("qmi8660_fifo.rseq");
        fs::write(
            &chip,
            r#"
sensor: qmi8660
pages:
  UI:
    registers:
      - addr: 0x00
        name: WHOAMI
        access: RO
      - addr: 0x0b
        name: COMM_CTL
        access: RW
      - addr: 0x54
        name: FIFO_STATUSL
        access: RO
      - addr: 0x55
        name: FIFO_STATUSH
        access: RO
        fields:
          - name: fifo_hi
            bits: "3:0"
      - addr: 0x57
        name: FIFO_DATA
        access: RO
        no_dump: true
        no_dump_reason: read drains fifo
"#,
        )
        .unwrap();
        fs::write(
            &script,
            r#"
chip!("qmi8660.yaml");
report_format!(FIFO_RAW, i16_le, {
    fields: [gx, gy, gz, ax, ay, az],
    gyro_fields: [gx, gy, gz],
    accel_fields: [ax, ay, az],
    accel_fs_g: 16,
    gyro_fs_dps: 4096,
    output: physical_f32,
});
let whoami = read!(UI.WHOAMI, 1);
"#,
        )
        .unwrap();
        (chip, script)
    }

    fn test_decoder(output: ReportOutputMode) -> I16LeReportDecoder {
        match make_i16_le_decoder(
            "i16_le",
            ["gx", "gy", "gz", "ax", "ay", "az"]
                .into_iter()
                .map(str::to_string)
                .collect(),
            ["gx", "gy", "gz"].into_iter().map(str::to_string).collect(),
            ["ax", "ay", "az"].into_iter().map(str::to_string).collect(),
            None,
            16.0,
            4096.0,
            DEFAULT_TEMP_LSB_PER_C,
            DEFAULT_TEMP_OFFSET_C,
            output,
        )
        .unwrap()
        {
            ReportDecoder::I16Le(decoder) => decoder,
        }
    }

    #[test]
    fn loads_metadata_from_rseq_and_chip_yaml() {
        let dir = temp_dir();
        let (_chip, script) = write_fixture(&dir);
        let metadata = load_host_metadata(&[script], &[]).unwrap();

        assert!(metadata.register_catalog.is_no_dump(0x57));
        assert!(!metadata.register_catalog.is_no_dump(0x54));
        assert!(
            metadata
                .report_decoders
                .get(rseq::REPORT_KIND_FIFO_RAW)
                .is_some()
        );
    }

    #[test]
    fn in_memory_rseq_sources_collect_metadata_and_compile() {
        let dir = temp_dir();
        let (_chip, script) = write_fixture(&dir);
        let source = fs::read_to_string(&script).unwrap();
        let sources = vec![RseqSource::new("editor.rseq", source, Some(dir))];

        let metadata = load_host_metadata_from_sources(&sources, &[]).unwrap();
        assert!(metadata.register_catalog.is_no_dump(0x57));
        assert!(
            metadata
                .report_decoders
                .get(rseq::REPORT_KIND_FIFO_RAW)
                .is_some()
        );

        let program = compile_rseq_sources(&sources).unwrap();
        assert!(!program.main.is_empty());
    }

    #[test]
    fn selected_register_targets_use_yaml_access_and_width() {
        let dir = temp_dir();
        let (chip, _script) = write_fixture(&dir);
        let metadata = load_host_metadata(&[], &[chip]).unwrap();

        let read = metadata
            .register_catalog
            .selected_read_target(0x54)
            .unwrap();
        assert_eq!(read.addr, 0x54);
        assert_eq!(read.len, 1);
        assert!(
            metadata
                .register_catalog
                .selected_read_target(0x57)
                .is_err()
        );

        let write = metadata
            .register_catalog
            .selected_write_target(0x0b)
            .unwrap();
        assert_eq!(write.addr, 0x0b);
        assert_eq!(write.width, Some(1));
        assert!(
            metadata
                .register_catalog
                .selected_write_target(0x00)
                .is_err()
        );
    }

    #[test]
    fn binary_report_capture_round_trips_records() {
        let dir = temp_dir();
        let path = dir.join("capture.bin");
        let records = vec![ReportCaptureRecord {
            meta: Some(ReportMeta {
                flags: rseq_link::REPORT_FLAG_TIMESTAMP_VALID,
                frame_id: 42,
                timestamp_us: 123_456,
            }),
            kind: rseq::REPORT_KIND_FIFO_RAW,
            args: vec![
                ReportArg::U32(4),
                ReportArg::Bytes(vec![0x11, 0x22, 0x33, 0x44]),
            ],
        }];

        write_report_capture(&path, &records).unwrap();
        assert_eq!(read_report_capture(&path).unwrap(), records);
    }

    #[test]
    fn decodes_i16_le_fifo_physical_units() {
        let decoder = test_decoder(ReportOutputMode::PhysicalF32);
        let raw = [1i16, -1, 0x1234, 2048, 0, -2048]
            .into_iter()
            .flat_map(i16::to_le_bytes)
            .collect::<Vec<_>>();
        let decoded = decode_i16_le_fifo_samples(&raw, &decoder);
        assert_eq!(decoded.samples.len(), 1);
        assert_eq!(decoded.trailing_bytes, 0);
        let sample = decoded.samples[0].to_motion(&decoder, None).unwrap();
        assert!((sample.acc[0] - 9.80665).abs() < 0.001);
        assert!(sample.gyro[2] > 10.0);
        assert_eq!(sample.temp_c, None);
    }

    #[test]
    fn decodes_optional_temperature_field_to_celsius() {
        let decoder = match make_i16_le_decoder(
            "i16_le",
            ["gx", "gy", "gz", "ax", "ay", "az", "temp"]
                .into_iter()
                .map(str::to_string)
                .collect(),
            ["gx", "gy", "gz"].into_iter().map(str::to_string).collect(),
            ["ax", "ay", "az"].into_iter().map(str::to_string).collect(),
            Some("temp".to_string()),
            16.0,
            4096.0,
            256.0,
            0.0,
            ReportOutputMode::PhysicalF32,
        )
        .unwrap()
        {
            ReportDecoder::I16Le(decoder) => decoder,
        };
        let raw = [1i16, -1, 0x1234, 2048, 0, -2048, 6400]
            .into_iter()
            .flat_map(i16::to_le_bytes)
            .collect::<Vec<_>>();
        let decoded = decode_i16_le_fifo_samples(&raw, &decoder);
        let sample = decoded.samples[0].to_motion(&decoder, None).unwrap();
        assert_eq!(sample.temp_c, Some(25.0));
    }

    #[test]
    fn report_health_tracks_drops_and_out_of_order_frames() {
        let mut tracker = ReportHealthTracker::default();
        let flags = rseq_link::REPORT_FLAG_TIMESTAMP_VALID;
        tracker.observe(Some(ReportMeta {
            flags,
            frame_id: 10,
            timestamp_us: 100,
        }));
        let health = tracker.observe(Some(ReportMeta {
            flags,
            frame_id: 13,
            timestamp_us: 250,
        }));
        assert_eq!(health.dropped_frames, 2);
        assert_eq!(health.last_dt_us, Some(150));
        let health = tracker.observe(Some(ReportMeta {
            flags,
            frame_id: 12,
            timestamp_us: 300,
        }));
        assert_eq!(health.out_of_order_frames, 1);
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
    fn report_processor_converts_fifo_report_trace_to_events() {
        let mut registry = ReportDecoderRegistry::default();
        registry.insert(
            rseq::REPORT_KIND_FIFO_RAW,
            ReportDecoder::I16Le(test_decoder(ReportOutputMode::PhysicalF32)),
        );
        let mut processor = ReportProcessor::new(registry);
        let raw = [0i16, 0, 0, 0, 0, 2048]
            .into_iter()
            .flat_map(i16::to_le_bytes)
            .collect::<Vec<_>>();
        let meta = ReportMeta {
            flags: rseq_link::REPORT_FLAG_TIMESTAMP_VALID,
            frame_id: 77,
            timestamp_us: 123_456,
        };

        let events = processor.handle_bus_op(BusOp::Report {
            meta: Some(meta),
            kind: rseq::REPORT_KIND_FIFO_RAW,
            args: vec![ReportArg::U32(raw.len() as u32), ReportArg::Bytes(raw)],
        });

        assert!(events.iter().any(|event| matches!(
            event,
            SessionEvent::Sample(sample)
                if sample.timestamp_us == Some(123_456) && sample.acc[2] > 9.0
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            SessionEvent::Report(summary)
                if summary.sample_count == 1
                    && summary.fifo_len == Some(12)
                    && summary.line.contains("frame_id=77")
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            SessionEvent::Health(health)
                if health.total_reports == 1 && health.last_frame_id == Some(77)
        )));
    }

    #[test]
    fn mock_transport_load_exec_traces_flow_into_session_events() {
        use rseq::link::HostLink;
        use rseq_link::MockTransport;
        use rseq_link::wire::ExecStatus;
        use rseq_mcu_sim::{SimBus, mcu_loop};

        let source = "\
write!(0x20, [0xaa, 0x55], 10);
let n = 7;
report!(0x10, n, n + 1);
";
        let program = rseq::parse(source).unwrap();
        let bytecode = rseq::compile(&program).unwrap();
        let (host_t, mcu_t) = MockTransport::pair();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_mcu = stop.clone();
        let mcu = std::thread::spawn(move || {
            let _ = mcu_loop(mcu_t, SimBus::new(), stop_mcu);
        });

        let mut host = HostLink::new(host_t);
        host.ping().unwrap();
        host.reset().unwrap();
        host.stop_reports().unwrap();
        host.load(&bytecode).unwrap();
        let result = host.exec().unwrap();
        stop.store(true, Ordering::SeqCst);
        drop(host);
        let _ = mcu.join();

        assert_eq!(result.status, ExecStatus::Ok);
        let mut processor = ReportProcessor::new(ReportDecoderRegistry::default());
        let events = result
            .traces
            .into_iter()
            .flat_map(|op| processor.handle_bus_op(op))
            .collect::<Vec<_>>();

        assert!(events.iter().any(|event| matches!(
            event,
            SessionEvent::Register {
                addr: 0x20,
                access: AccessKind::Write,
                data
            } if data == &vec![0xaa, 0x55]
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            SessionEvent::Log(line) if line == "delay 10us"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            SessionEvent::Report(summary)
                if summary.kind == 0x10 && summary.line.contains("u32=0x00000007")
        )));
    }
}
