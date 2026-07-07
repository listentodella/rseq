mod plot;

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::Result as AnyhowResult;
use clap::Parser;
use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::{
    ActiveTheme, Disableable as _, ElementExt as _, Icon, IconName, Root, Selectable as _,
    Sizable as _, StyledExt as _, Theme, ThemeMode, TitleBar,
    button::{Button, ButtonVariants as _, DropdownButton},
    h_flex,
    input::{
        CompletionProvider, DocumentRangeSemanticTokensProvider, HoverProvider, Input, InputEvent,
        InputState,
    },
    scroll::ScrollableElement as _,
    tab::{Tab, TabBar},
    tooltip::Tooltip,
    v_flex,
};
use plot::{
    ScalarLineChart, TripleLineChart, TripleOhlc, TripleOhlcChart, Vec3, new_triple_ohlc,
    push_triple_ohlc,
};
use rseq_host::{
    AccessKind, FieldInfo, HostMetadata, MAX_TEXT_LINES, MotionSample, RegisterCatalog,
    RegisterInfo, ReportCaptureRecord, ReportDecoder, ReportDecoderRegistry, ReportHealth,
    ReportOutputMode, ReportProcessor, RseqSource, SessionCommand, SessionConfig, SessionEvent,
    SessionHandle, compile_rseq_files, compile_rseq_sources, hex_bytes, load_host_metadata,
    load_host_metadata_from_sources, make_i16_le_decoder, parse_register_write_bytes, push_bounded,
    read_report_capture, write_report_capture,
};
use serde::Deserialize;
use serde::Serialize;
use serde::de::DeserializeOwned;

const UI_TICK: Duration = Duration::from_millis(33);
const MAX_SAMPLES: usize = 600;
const HISTORY_BUCKET_SECS: u64 = 1;
const HISTORY_BUCKET_US: u64 = HISTORY_BUCKET_SECS * 1_000_000;
const MAX_HISTORY_BARS: usize = 120;
const MAX_HISTORY_BAR_SAMPLES: usize = 4096;
const REGISTER_DUMP_BATCH_MAX_LEN: usize = rseq_link::wire::CONTROL_MAX_READ_LEN;
const MAX_CAPTURE_RECORDS: usize = 200_000;
const CHART_ZOOM_MIN: f32 = 0.25;
const CHART_ZOOM_MAX: f32 = 8.0;
const CHART_WHEEL_ZOOM_STEP: f32 = 1.12;
const COMMON_SERIAL_BAUDS: [u32; 10] = [
    9_600, 19_200, 38_400, 57_600, 115_200, 230_400, 460_800, 921_600, 1_000_000, 2_000_000,
];
const DEFAULT_RSEQ_SOURCE: &str = r#"// New rseq script.
// Pick a chip YAML in the Sequences sidebar or add chip!("chip.yaml") here.

print!("rseq sequence start");
"#;

#[derive(Clone)]
struct RseqEditorLanguageProvider {
    root_dir: Option<PathBuf>,
    chips: Rc<RefCell<Vec<PathBuf>>>,
}

impl RseqEditorLanguageProvider {
    fn new(root_dir: Option<PathBuf>, chips: Rc<RefCell<Vec<PathBuf>>>) -> Self {
        Self { root_dir, chips }
    }

    fn analysis(&self, source: &str) -> rseq_lsp::DocumentAnalysis {
        let chips = self.chips.borrow().clone();
        rseq_lsp::analyze_document(
            source,
            self.root_dir.as_deref(),
            self.root_dir.as_deref(),
            &chips,
        )
    }

    fn completion_items(&self, source: &str, offset: usize) -> Vec<lsp_types::CompletionItem> {
        let analysis = self.analysis(source);
        rseq_lsp::completion_items_at_offset(source, offset, &analysis.facts)
            .into_iter()
            .filter_map(convert_rseq_lsp_type)
            .map(|item| rseq_gpui_completion_item(source, item))
            .collect()
    }

    fn hover(&self, source: &str, offset: usize) -> Option<lsp_types::Hover> {
        let analysis = self.analysis(source);
        let mut hover: lsp_types::Hover =
            rseq_lsp::hover_at_offset(source, offset, &analysis.facts)
                .and_then(convert_rseq_lsp_type)?;
        if let Some(range) = hover.range.as_mut() {
            *range = rseq_lsp_range_to_gpui_range(source, *range);
        }
        Some(hover)
    }
}

fn convert_rseq_lsp_type<T, U>(value: T) -> Option<U>
where
    T: Serialize,
    U: DeserializeOwned,
{
    serde_json::to_value(value)
        .ok()
        .and_then(|value| serde_json::from_value(value).ok())
}

impl CompletionProvider for RseqEditorLanguageProvider {
    fn completions(
        &self,
        text: &gpui_component::Rope,
        offset: usize,
        _trigger: lsp_types::CompletionContext,
        _window: &mut Window,
        _cx: &mut Context<InputState>,
    ) -> Task<AnyhowResult<lsp_types::CompletionResponse>> {
        let source = text.to_string();
        let items = if rseq_should_offer_completion(&source, offset) {
            self.completion_items(&source, offset)
        } else {
            Vec::new()
        };
        Task::ready(Ok(lsp_types::CompletionResponse::Array(items)))
    }

    fn is_completion_trigger(
        &self,
        _offset: usize,
        new_text: &str,
        _cx: &mut Context<InputState>,
    ) -> bool {
        new_text.is_empty()
            || new_text.chars().any(|ch| {
                ch.is_ascii_alphanumeric()
                    || matches!(
                        ch,
                        '_' | '.'
                            | '!'
                            | '('
                            | '{'
                            | ','
                            | ';'
                            | ')'
                            | ']'
                            | '}'
                            | '\n'
                            | '\r'
                            | ' '
                            | '\t'
                    )
            })
    }
}

fn rseq_gpui_completion_item(
    source: &str,
    mut item: lsp_types::CompletionItem,
) -> lsp_types::CompletionItem {
    let label = item.label.clone();
    if let Some(text_edit) = item.text_edit.as_mut() {
        match text_edit {
            lsp_types::CompletionTextEdit::Edit(edit) => {
                edit.range = rseq_lsp_range_to_gpui_range(source, edit.range);
                edit.new_text = label.clone();
            }
            lsp_types::CompletionTextEdit::InsertAndReplace(edit) => {
                edit.insert = rseq_lsp_range_to_gpui_range(source, edit.insert);
                edit.replace = rseq_lsp_range_to_gpui_range(source, edit.replace);
                edit.new_text = label.clone();
            }
        }
    } else {
        item.insert_text = Some(label.clone());
    }
    item.insert_text_format = Some(lsp_types::InsertTextFormat::PLAIN_TEXT);
    item
}

fn rseq_lsp_range_to_gpui_range(source: &str, range: lsp_types::Range) -> lsp_types::Range {
    lsp_types::Range::new(
        rseq_lsp_position_to_gpui_position(source, range.start),
        rseq_lsp_position_to_gpui_position(source, range.end),
    )
}

fn rseq_lsp_position_to_gpui_position(
    source: &str,
    position: lsp_types::Position,
) -> lsp_types::Position {
    let byte_offset = rseq_lsp_position_to_byte_offset(source, position);
    let (line, character) = rseq_byte_offset_to_gpui_position(source, byte_offset);
    lsp_types::Position::new(line, character)
}

fn rseq_lsp_position_to_byte_offset(source: &str, position: lsp_types::Position) -> usize {
    let mut current_line = 0u32;
    let mut line_start = 0usize;
    for (idx, ch) in source.char_indices() {
        if current_line == position.line {
            break;
        }
        if ch == '\n' {
            current_line += 1;
            line_start = idx + ch.len_utf8();
        }
    }

    if current_line < position.line {
        return source.len();
    }

    let line_end = source[line_start..]
        .find('\n')
        .map(|idx| line_start + idx)
        .unwrap_or(source.len());
    line_start + rseq_utf16_column_to_byte(&source[line_start..line_end], position.character)
}

fn rseq_utf16_column_to_byte(line: &str, target_col: u32) -> usize {
    let mut col = 0u32;
    for (idx, ch) in line.char_indices() {
        let width = ch.len_utf16() as u32;
        if col + width > target_col {
            return idx;
        }
        col += width;
    }
    line.len()
}

fn rseq_byte_offset_to_gpui_position(source: &str, offset: usize) -> (u32, u32) {
    let offset = offset.min(source.len());
    let mut line = 0u32;
    let mut character = 0u32;
    for (idx, ch) in source.char_indices() {
        if idx >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            character = 0;
        } else {
            character += 1;
        }
    }
    (line, character)
}

fn rseq_should_offer_completion(source: &str, offset: usize) -> bool {
    let offset = offset.min(source.len());
    let token = rseq_completion_prefix_before(source, offset);
    if !token.is_empty() {
        return true;
    }

    let Some(previous) = rseq_previous_non_whitespace_char(source, offset) else {
        return false;
    };
    matches!(previous, '(' | '{' | '[' | ',' | ':' | '.')
}

fn rseq_completion_prefix_before(source: &str, offset: usize) -> &str {
    let mut start = offset.min(source.len());
    while start > 0 {
        let Some((idx, ch)) = source[..start].char_indices().next_back() else {
            break;
        };
        if rseq_is_ident_continue(ch) {
            start = idx;
        } else {
            break;
        }
    }
    &source[start..offset.min(source.len())]
}

fn rseq_previous_non_whitespace_char(source: &str, offset: usize) -> Option<char> {
    source[..offset.min(source.len())]
        .chars()
        .rev()
        .find(|ch| !ch.is_whitespace())
}

impl HoverProvider for RseqEditorLanguageProvider {
    fn hover(
        &self,
        text: &gpui_component::Rope,
        offset: usize,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Task<AnyhowResult<Option<lsp_types::Hover>>> {
        let source = text.to_string();
        Task::ready(Ok(self.hover(&source, offset)))
    }
}

impl DocumentRangeSemanticTokensProvider for RseqEditorLanguageProvider {
    fn legend(&self) -> lsp_types::SemanticTokensLegend {
        lsp_types::SemanticTokensLegend {
            token_types: RSEQ_SEMANTIC_TOKEN_TYPES
                .iter()
                .map(|name| lsp_types::SemanticTokenType::from((*name).to_string()))
                .collect(),
            token_modifiers: vec![],
        }
    }

    fn semantic_tokens(
        &self,
        text: &gpui_component::Rope,
        range: Range<usize>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Task<AnyhowResult<lsp_types::SemanticTokens>> {
        let source = text.to_string();
        let analysis = self.analysis(&source);
        Task::ready(Ok(rseq_semantic_tokens(&source, range, &analysis.facts)))
    }
}

const RSEQ_SEMANTIC_TOKEN_TYPES: &[&str] = &[
    "comment",
    "string",
    "number",
    "keyword",
    "function",
    "type",
    "property",
    "constant",
    "variable",
    "operator",
    "punctuation",
    "label",
];

const RSEQ_FUNCTIONS: &[&str] = &[
    "chip!",
    "bus!",
    "bus_probe!",
    "read!",
    "write!",
    "update!",
    "irq!",
    "wait!",
    "repeat!",
    "print!",
    "report!",
    "report_format!",
];

const RSEQ_KEYWORDS: &[&str] = &["let", "if", "else", "on"];
const RSEQ_TYPES: &[&str] = &[
    "spi",
    "i2c",
    "i3c",
    "i16_le",
    "qmi8660_fifo6",
    "physical_f32",
    "raw_i16",
];
const RSEQ_PROPERTIES: &[&str] = &[
    "fields",
    "gyro_fields",
    "accel_fields",
    "temp_field",
    "accel_fs_g",
    "gyro_fs_dps",
    "temp_lsb_per_c",
    "temp_offset_c",
    "output",
];
const RSEQ_CONSTANTS: &[&str] = &["FIFO_RAW", "AMD", "SMD", "DRDY"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RseqSemanticKind {
    Comment,
    String,
    Number,
    Keyword,
    Function,
    Type,
    Property,
    Constant,
    Variable,
    Operator,
    Punctuation,
    Label,
}

impl RseqSemanticKind {
    fn token_type(self) -> u32 {
        match self {
            Self::Comment => 0,
            Self::String => 1,
            Self::Number => 2,
            Self::Keyword => 3,
            Self::Function => 4,
            Self::Type => 5,
            Self::Property => 6,
            Self::Constant => 7,
            Self::Variable => 8,
            Self::Operator => 9,
            Self::Punctuation => 10,
            Self::Label => 11,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RseqSemanticHit {
    range: Range<usize>,
    kind: RseqSemanticKind,
}

#[derive(Debug, Default)]
struct RseqSemanticSymbols {
    pages: BTreeSet<String>,
    registers: BTreeSet<String>,
    fields: BTreeSet<String>,
    events: BTreeSet<String>,
}

impl RseqSemanticSymbols {
    fn from_facts(facts: &rseq_lsp::LanguageFacts) -> Self {
        let mut symbols = Self::default();
        symbols.pages.extend(facts.pages.iter().cloned());
        for register in &facts.registers {
            symbols.registers.insert(register.name.clone());
            symbols
                .registers
                .insert(format!("{}.{}", register.page, register.name));
        }
        for field in &facts.fields {
            symbols.fields.insert(field.name.clone());
            symbols
                .fields
                .insert(format!("{}.{}.{}", field.page, field.register, field.name));
        }
        for event in &facts.events {
            symbols.events.insert(event.name.clone());
        }
        symbols
    }

    fn classify_ident(&self, token: &str) -> RseqSemanticKind {
        if RSEQ_FUNCTIONS.contains(&token) {
            RseqSemanticKind::Function
        } else if RSEQ_KEYWORDS.contains(&token) {
            RseqSemanticKind::Keyword
        } else if RSEQ_TYPES.contains(&token) || self.pages.contains(token) {
            RseqSemanticKind::Type
        } else if RSEQ_CONSTANTS.contains(&token) {
            RseqSemanticKind::Constant
        } else if RSEQ_PROPERTIES.contains(&token)
            || self.registers.contains(token)
            || self.fields.contains(token)
        {
            RseqSemanticKind::Property
        } else if self.events.contains(token) {
            RseqSemanticKind::Label
        } else {
            RseqSemanticKind::Variable
        }
    }
}

fn rseq_semantic_tokens(
    source: &str,
    range: Range<usize>,
    facts: &rseq_lsp::LanguageFacts,
) -> lsp_types::SemanticTokens {
    let hits = rseq_semantic_hits(source, range, facts);
    let mut data = Vec::with_capacity(hits.len());
    let mut previous_line = 0u32;
    let mut previous_character = 0u32;

    for hit in hits {
        let (line, character) = rseq_line_character(source, hit.range.start);
        let length = source[hit.range.clone()].chars().count() as u32;
        if length == 0 {
            continue;
        }
        let delta_line = line.saturating_sub(previous_line);
        let delta_start = if delta_line == 0 {
            character.saturating_sub(previous_character)
        } else {
            character
        };
        data.push(lsp_types::SemanticToken {
            delta_line,
            delta_start,
            length,
            token_type: hit.kind.token_type(),
            token_modifiers_bitset: 0,
        });
        previous_line = line;
        previous_character = character;
    }

    lsp_types::SemanticTokens {
        result_id: None,
        data,
    }
}

fn rseq_semantic_hits(
    source: &str,
    range: Range<usize>,
    facts: &rseq_lsp::LanguageFacts,
) -> Vec<RseqSemanticHit> {
    let symbols = RseqSemanticSymbols::from_facts(facts);
    let mut hits = Vec::new();
    let mut offset = 0usize;

    while offset < source.len() {
        let Some(ch) = source[offset..].chars().next() else {
            break;
        };

        if ch.is_whitespace() {
            offset += ch.len_utf8();
            continue;
        }

        if source[offset..].starts_with("//") {
            let end = source[offset..]
                .find('\n')
                .map(|line_end| offset + line_end)
                .unwrap_or(source.len());
            rseq_push_semantic_hit(&mut hits, offset..end, RseqSemanticKind::Comment, &range);
            offset = end;
            continue;
        }

        if ch == '"' {
            let end = rseq_scan_string(source, offset);
            rseq_push_semantic_hit(&mut hits, offset..end, RseqSemanticKind::String, &range);
            offset = end;
            continue;
        }

        if ch.is_ascii_digit() {
            let end = rseq_scan_number(source, offset);
            rseq_push_semantic_hit(&mut hits, offset..end, RseqSemanticKind::Number, &range);
            offset = end;
            continue;
        }

        if rseq_is_ident_start(ch) {
            let end = rseq_scan_ident(source, offset);
            let token = &source[offset..end];
            rseq_push_semantic_hit(
                &mut hits,
                offset..end,
                symbols.classify_ident(token),
                &range,
            );
            offset = end;
            continue;
        }

        if rseq_is_punctuation(ch) {
            let end = offset + ch.len_utf8();
            rseq_push_semantic_hit(
                &mut hits,
                offset..end,
                RseqSemanticKind::Punctuation,
                &range,
            );
            offset = end;
            continue;
        }

        if rseq_is_operator(ch) {
            let end = rseq_scan_operator(source, offset);
            rseq_push_semantic_hit(&mut hits, offset..end, RseqSemanticKind::Operator, &range);
            offset = end;
            continue;
        }

        offset += ch.len_utf8();
    }

    hits
}

fn rseq_push_semantic_hit(
    hits: &mut Vec<RseqSemanticHit>,
    token_range: Range<usize>,
    kind: RseqSemanticKind,
    requested_range: &Range<usize>,
) {
    if token_range.start >= token_range.end
        || token_range.end <= requested_range.start
        || token_range.start >= requested_range.end
    {
        return;
    }
    hits.push(RseqSemanticHit {
        range: token_range,
        kind,
    });
}

fn rseq_scan_string(source: &str, start: usize) -> usize {
    let mut escaped = false;
    let mut offset = start + 1;
    while offset < source.len() {
        let Some(ch) = source[offset..].chars().next() else {
            break;
        };
        offset += ch.len_utf8();
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '"' => break,
            '\n' => break,
            _ => {}
        }
    }
    offset
}

fn rseq_scan_number(source: &str, start: usize) -> usize {
    let mut offset = start;
    while offset < source.len() {
        let Some(ch) = source[offset..].chars().next() else {
            break;
        };
        if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.') {
            offset += ch.len_utf8();
        } else {
            break;
        }
    }
    offset
}

fn rseq_scan_ident(source: &str, start: usize) -> usize {
    let mut offset = start;
    while offset < source.len() {
        let Some(ch) = source[offset..].chars().next() else {
            break;
        };
        if rseq_is_ident_continue(ch) {
            offset += ch.len_utf8();
        } else {
            break;
        }
    }
    offset
}

fn rseq_scan_operator(source: &str, start: usize) -> usize {
    let mut offset = start;
    while offset < source.len() {
        let Some(ch) = source[offset..].chars().next() else {
            break;
        };
        if rseq_is_operator(ch) {
            offset += ch.len_utf8();
        } else {
            break;
        }
    }
    offset
}

fn rseq_is_ident_start(ch: char) -> bool {
    ch.is_ascii_alphabetic() || ch == '_'
}

fn rseq_is_ident_continue(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '!')
}

fn rseq_is_punctuation(ch: char) -> bool {
    matches!(ch, '{' | '}' | '(' | ')' | '[' | ']' | ',' | ':' | ';')
}

fn rseq_is_operator(ch: char) -> bool {
    matches!(
        ch,
        '=' | '|' | '&' | '<' | '>' | '+' | '-' | '*' | '/' | '%' | '^' | '!'
    )
}

fn rseq_line_character(source: &str, offset: usize) -> (u32, u32) {
    let offset = offset.min(source.len());
    let mut line = 0u32;
    let mut line_start = 0usize;
    for (idx, ch) in source.char_indices() {
        if idx >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            line_start = idx + ch.len_utf8();
        }
    }
    let character = source[line_start..offset].chars().count() as u32;
    (line, character)
}

#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = rseq_gpui, no_json)]
struct SelectLinkModeAction(usize);

#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = rseq_gpui, no_json)]
struct SelectSerialPortAction(String);

#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = rseq_gpui, no_json)]
struct SelectSerialBaudAction(u32);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CaptureSidecar {
    version: u32,
    format: String,
    rseq_files: Vec<String>,
    chip_files: Vec<String>,
    skip_samples: usize,
    report_decoders: Vec<CaptureDecoderMeta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CaptureDecoderMeta {
    kind: u32,
    kind_label: String,
    decoder: String,
    fields: Vec<String>,
    gyro_fields: Vec<String>,
    accel_fields: Vec<String>,
    temp_field: Option<String>,
    accel_fs_g: f64,
    gyro_fs_dps: f64,
    temp_lsb_per_c: f64,
    temp_offset_c: f64,
    output: String,
}

#[derive(Parser, Debug, Clone)]
#[command(author, version, about = "GPUI workstation for rseq MCU reports")]
struct Cli {
    #[arg(short, long)]
    file: Vec<PathBuf>,

    #[arg(long)]
    chip: Vec<PathBuf>,

    #[arg(long)]
    serial: Option<String>,

    #[arg(long, default_value_t = rseq_host::DEFAULT_BAUD)]
    baud: u32,

    #[arg(long)]
    demo: bool,

    #[arg(long, alias = "observe-only", alias = "rx-only")]
    watch: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinkMode {
    Demo,
    Serial,
    Tcp,
    Ble,
    WebSocket,
    Custom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MotionSeries {
    Acc,
    Gyro,
}

impl MotionSeries {
    fn label(self) -> &'static str {
        match self {
            Self::Acc => "Accelerometer",
            Self::Gyro => "Gyroscope",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ChartRange {
    y_min: f32,
    y_max: f32,
}

impl ChartRange {
    fn span(self) -> f32 {
        (self.y_max - self.y_min).max(f32::EPSILON)
    }

    fn value_at_y_fraction(self, y_fraction: f32) -> f32 {
        self.y_max - y_fraction.clamp(0.0, 1.0) * self.span()
    }
}

#[derive(Debug, Clone, Copy)]
struct ChartXRange {
    x_min: f32,
    x_max: f32,
}

impl ChartXRange {
    fn span(self) -> f32 {
        (self.x_max - self.x_min).max(f32::EPSILON)
    }

    fn value_at_x_fraction(self, x_fraction: f32) -> f32 {
        self.x_min + x_fraction.clamp(0.0, 1.0) * self.span()
    }
}

#[derive(Debug, Clone, Copy)]
struct ChartDragState {
    series: MotionSeries,
    start_position: Point<Pixels>,
    bounds: Bounds<Pixels>,
    y_range: ChartRange,
    x_range: ChartXRange,
    auto_x_range: ChartXRange,
}

#[derive(Debug, Clone)]
struct HistoryBar {
    acc: TripleOhlc,
    gyro: TripleOhlc,
    acc_samples: Vec<Vec3>,
    gyro_samples: Vec<Vec3>,
}

impl HistoryBar {
    fn samples_for(&self, series: MotionSeries) -> &[Vec3] {
        match series {
            MotionSeries::Acc => &self.acc_samples,
            MotionSeries::Gyro => &self.gyro_samples,
        }
    }
}

#[derive(Debug, Clone)]
struct HistoryIntradayView {
    series: MotionSeries,
    bar_index: usize,
    bar_count: usize,
    samples: Vec<Vec3>,
}

impl LinkMode {
    const ALL: [Self; 6] = [
        Self::Demo,
        Self::Serial,
        Self::Tcp,
        Self::Ble,
        Self::WebSocket,
        Self::Custom,
    ];

    fn id(self) -> usize {
        match self {
            Self::Demo => 0,
            Self::Serial => 1,
            Self::Tcp => 2,
            Self::Ble => 3,
            Self::WebSocket => 4,
            Self::Custom => 5,
        }
    }

    fn from_id(id: usize) -> Option<Self> {
        Self::ALL.iter().copied().find(|mode| mode.id() == id)
    }

    fn label(self) -> &'static str {
        match self {
            Self::Demo => "Demo",
            Self::Serial => "Serial",
            Self::Tcp => "TCP",
            Self::Ble => "BLE",
            Self::WebSocket => "WebSocket",
            Self::Custom => "Custom",
        }
    }

    fn can_connect(self) -> bool {
        matches!(self, Self::Demo | Self::Serial)
    }

    fn tooltip(self) -> &'static str {
        match self {
            Self::Demo => "Run the built-in simulated report stream.",
            Self::Serial => {
                "Use an MCU over USB CDC or USB-UART with the rseq-link frame protocol."
            }
            Self::Tcp => "Reserved for a future TCP byte-stream transport.",
            Self::Ble => "Reserved for a future BLE transport.",
            Self::WebSocket => "Reserved for a future WebSocket transport.",
            Self::Custom => "Reserved for project-specific transports.",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum PanelTab {
    #[default]
    Motion,
    Reports,
    Registers,
    Sequences,
    Logs,
}

impl PanelTab {
    const ALL: [Self; 5] = [
        Self::Motion,
        Self::Reports,
        Self::Registers,
        Self::Sequences,
        Self::Logs,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Motion => "Motion",
            Self::Reports => "Reports",
            Self::Registers => "Registers",
            Self::Sequences => "Sequences",
            Self::Logs => "Logs",
        }
    }

    fn from_index(index: usize) -> Self {
        Self::ALL.get(index).copied().unwrap_or_default()
    }

    fn index(self) -> usize {
        Self::ALL.iter().position(|tab| *tab == self).unwrap_or(0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum RegisterViewMode {
    #[default]
    Dump,
    Map,
}

impl RegisterViewMode {
    const ALL: [Self; 2] = [Self::Dump, Self::Map];

    fn label(self) -> &'static str {
        match self {
            Self::Dump => "Matrix",
            Self::Map => "Register Map",
        }
    }
}

#[derive(Debug, Clone)]
struct RegisterValue {
    access: AccessKind,
    data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RegisterDumpRange {
    start: u32,
    len: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum SequenceViewMode {
    #[default]
    Text,
    Blocks,
}

impl SequenceViewMode {
    const ALL: [Self; 2] = [Self::Text, Self::Blocks];

    fn label(self) -> &'static str {
        match self {
            Self::Text => "Text",
            Self::Blocks => "Blocks",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VisualStepKind {
    Read,
    Write,
}

impl VisualStepKind {
    fn label(self) -> &'static str {
        match self {
            Self::Read => "Read",
            Self::Write => "Write",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct VisualStepBlueprint {
    kind: VisualStepKind,
    address: String,
    read_len: String,
    data: String,
    delay_us: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct VisualSequenceBlueprint {
    name: String,
    steps: Vec<VisualStepBlueprint>,
}

struct VisualSequence {
    name_input: Entity<InputState>,
    steps: Vec<VisualStepEditor>,
}

struct VisualStepEditor {
    kind: VisualStepKind,
    address_input: Entity<InputState>,
    read_len_input: Entity<InputState>,
    data_input: Entity<InputState>,
    delay_us_input: Entity<InputState>,
}

#[derive(Clone)]
struct DragVisualSequence {
    entity_id: EntityId,
    sequence_index: usize,
    label: String,
}

impl Render for DragVisualSequence {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .id("drag-visual-sequence")
            .px_3()
            .py_1()
            .gap_2()
            .items_center()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().drag_border)
            .bg(cx.theme().popover)
            .shadow_sm()
            .opacity(0.92)
            .child(Icon::new(IconName::Menu).xsmall())
            .child(
                div()
                    .text_xs()
                    .font_semibold()
                    .text_color(cx.theme().foreground)
                    .child(self.label.clone()),
            )
    }
}

#[derive(Clone)]
struct DragVisualStep {
    entity_id: EntityId,
    sequence_index: usize,
    step_index: usize,
    label: String,
}

impl Render for DragVisualStep {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .id("drag-visual-step")
            .px_3()
            .py_1()
            .gap_2()
            .items_center()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().drag_border)
            .bg(cx.theme().popover)
            .shadow_sm()
            .opacity(0.92)
            .child(Icon::new(IconName::Menu).xsmall())
            .child(
                div()
                    .font_family("monospace")
                    .text_xs()
                    .text_color(cx.theme().foreground)
                    .child(self.label.clone()),
            )
    }
}

#[derive(Debug, Clone)]
struct HistoryBucket {
    acc: TripleOhlc,
    gyro: TripleOhlc,
    acc_samples: Vec<Vec3>,
    gyro_samples: Vec<Vec3>,
    samples: usize,
    start_timestamp_us: Option<u64>,
    started_at: Instant,
}

impl HistoryBucket {
    fn new(acc: Vec3, gyro: Vec3, timestamp_us: Option<u64>, now: Instant) -> Self {
        Self {
            acc: new_triple_ohlc(acc),
            gyro: new_triple_ohlc(gyro),
            acc_samples: vec![acc],
            gyro_samples: vec![gyro],
            samples: 1,
            start_timestamp_us: timestamp_us,
            started_at: now,
        }
    }

    fn push(&mut self, acc: Vec3, gyro: Vec3) {
        push_triple_ohlc(&mut self.acc, acc);
        push_triple_ohlc(&mut self.gyro, gyro);
        push_vec_capped(&mut self.acc_samples, acc, MAX_HISTORY_BAR_SAMPLES);
        push_vec_capped(&mut self.gyro_samples, gyro, MAX_HISTORY_BAR_SAMPLES);
        self.samples += 1;
    }

    fn should_finish_before(&self, timestamp_us: Option<u64>, now: Instant) -> bool {
        if let (Some(start), Some(timestamp)) = (self.start_timestamp_us, timestamp_us) {
            timestamp >= start && timestamp - start >= HISTORY_BUCKET_US
        } else {
            now.duration_since(self.started_at) >= Duration::from_secs(HISTORY_BUCKET_SECS)
        }
    }

    fn to_bar(&self) -> HistoryBar {
        HistoryBar {
            acc: self.acc,
            gyro: self.gyro,
            acc_samples: self.acc_samples.clone(),
            gyro_samples: self.gyro_samples.clone(),
        }
    }
}

pub struct RseqGpui {
    cli: Cli,
    metadata: HostMetadata,
    startup_program: Option<rseq::CompiledProgram>,
    compile_status: String,
    link_mode: LinkMode,
    session: Option<SessionHandle>,
    selected_tab: PanelTab,
    selected_register_addr: u32,
    active_register_page: Option<String>,
    register_view_mode: RegisterViewMode,
    inline_write_addr: Option<u32>,
    sequence_path: Option<PathBuf>,
    sequence_dirty: bool,
    sequence_status: String,
    sequence_view_mode: SequenceViewMode,
    visual_sequences: Vec<VisualSequence>,
    active_visual_sequence: usize,
    serial_port_input: Entity<InputState>,
    serial_baud_input: Entity<InputState>,
    serial_ports: Vec<rseq_host::SerialPortInfo>,
    skip_samples_input: Entity<InputState>,
    rseq_path_input: Entity<InputState>,
    chip_path_input: Entity<InputState>,
    sequence_editor: Entity<InputState>,
    sequence_lsp_chips: Rc<RefCell<Vec<PathBuf>>>,
    inline_write_input: Entity<InputState>,
    write_input: Entity<InputState>,
    samples: VecDeque<MotionSample>,
    sample_skip_count: usize,
    sample_skip_remaining: usize,
    history_bars: VecDeque<HistoryBar>,
    history_bucket: Option<HistoryBucket>,
    acc_chart_range: Option<ChartRange>,
    gyro_chart_range: Option<ChartRange>,
    acc_chart_x_range: Option<ChartXRange>,
    gyro_chart_x_range: Option<ChartXRange>,
    acc_chart_bounds: Option<Bounds<Pixels>>,
    gyro_chart_bounds: Option<Bounds<Pixels>>,
    chart_drag: Option<ChartDragState>,
    acc_history_bounds: Option<Bounds<Pixels>>,
    gyro_history_bounds: Option<Bounds<Pixels>>,
    history_intraday: Option<HistoryIntradayView>,
    show_temperature_panel: bool,
    registers: BTreeMap<u32, RegisterValue>,
    capture_records: Vec<ReportCaptureRecord>,
    reports: VecDeque<String>,
    logs: VecDeque<String>,
    health: ReportHealth,
    connection_label: String,
    session_mode: String,
    connected: bool,
    _tick: Task<()>,
}

impl RseqGpui {
    fn new(
        cli: Cli,
        metadata: HostMetadata,
        startup_program: Option<rseq::CompiledProgram>,
        compile_status: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let link_mode = if cli.demo {
            LinkMode::Demo
        } else {
            LinkMode::Serial
        };
        let serial_port_value = cli.serial.clone().unwrap_or_default();
        let serial_baud_value = cli.baud.to_string();
        let serial_ports = rseq_host::available_serial_ports();
        let auto_serial_port = (serial_port_value.is_empty() && serial_ports.len() == 1)
            .then(|| serial_ports[0].port_name.clone());
        let rseq_path_value = path_list_value(&cli.file);
        let chip_path_value = path_list_value(&cli.chip);
        let (sequence_path, sequence_text, sequence_status) = initial_sequence_source(&cli.file);
        let visual_sequences = visual_sequences_from_source(&sequence_text, window, cx);
        let serial_port_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("/dev/cu.usbmodem...")
                .default_value(auto_serial_port.clone().unwrap_or(serial_port_value))
        });
        let serial_baud_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("115200")
                .default_value(serial_baud_value)
        });
        let skip_samples_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("0")
                .default_value("0")
        });
        let rseq_path_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("examples/qmi8660_fifo.rseq")
                .default_value(rseq_path_value)
        });
        let chip_path_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("qmi8660.yaml or from chip!(...)")
                .default_value(chip_path_value)
        });
        let sequence_lsp_chips = Rc::new(RefCell::new(cli.chip.clone()));
        let sequence_lsp_provider = Rc::new(RseqEditorLanguageProvider::new(
            std::env::current_dir().ok(),
            sequence_lsp_chips.clone(),
        ));
        let sequence_editor = cx.new(|cx| {
            let mut input = InputState::new(window, cx)
                .multi_line(true)
                .rows(24)
                .soft_wrap(false)
                .scroll_beyond_last_line(Some(3))
                .cursor_surrounding_lines(Some(1))
                .placeholder("write rseq here")
                .default_value(sequence_text);
            input.lsp.completion_provider = Some(sequence_lsp_provider.clone());
            input.lsp.hover_provider = Some(sequence_lsp_provider.clone());
            input
        });
        let inline_write_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("hex")
                .default_value("")
        });
        let write_input = cx.new(|cx| InputState::new(window, cx).placeholder("hex bytes"));
        cx.subscribe_in(
            &inline_write_input,
            window,
            |this, _, event: &InputEvent, _window, cx| {
                match event {
                    InputEvent::PressEnter { .. } => this.commit_inline_register_write(cx),
                    InputEvent::Blur => this.inline_write_addr = None,
                    InputEvent::Change | InputEvent::Focus => {}
                }
                cx.notify();
            },
        )
        .detach();
        cx.subscribe_in(
            &sequence_editor,
            window,
            |this, _, event: &InputEvent, _window, cx| {
                if matches!(event, InputEvent::Change) {
                    this.sequence_dirty = true;
                    this.sequence_status = "modified".to_string();
                    this.refresh_sequence_editor_diagnostics(cx);
                    cx.notify();
                }
            },
        )
        .detach();
        let active_register_page = metadata.register_catalog.pages().into_iter().next();
        let _tick = cx.spawn(async move |this, cx| {
            loop {
                smol::Timer::after(UI_TICK).await;
                if this
                    .update(cx, |state, cx| {
                        state.drain_session_events();
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        });

        let mut app = Self {
            cli,
            metadata,
            startup_program,
            compile_status,
            link_mode,
            session: None,
            selected_tab: PanelTab::Motion,
            selected_register_addr: 0,
            active_register_page,
            register_view_mode: RegisterViewMode::Dump,
            inline_write_addr: None,
            sequence_path,
            sequence_dirty: false,
            sequence_status,
            sequence_view_mode: SequenceViewMode::Text,
            visual_sequences,
            active_visual_sequence: 0,
            serial_port_input,
            serial_baud_input,
            serial_ports,
            skip_samples_input,
            rseq_path_input,
            chip_path_input,
            sequence_editor,
            sequence_lsp_chips,
            inline_write_input,
            write_input,
            samples: VecDeque::with_capacity(MAX_SAMPLES),
            sample_skip_count: 0,
            sample_skip_remaining: 0,
            history_bars: VecDeque::with_capacity(MAX_HISTORY_BARS),
            history_bucket: None,
            acc_chart_range: None,
            gyro_chart_range: None,
            acc_chart_x_range: None,
            gyro_chart_x_range: None,
            acc_chart_bounds: None,
            gyro_chart_bounds: None,
            chart_drag: None,
            acc_history_bounds: None,
            gyro_history_bounds: None,
            history_intraday: None,
            show_temperature_panel: true,
            registers: BTreeMap::new(),
            capture_records: Vec::new(),
            reports: VecDeque::with_capacity(MAX_TEXT_LINES),
            logs: VecDeque::with_capacity(MAX_TEXT_LINES),
            health: ReportHealth::default(),
            connection_label: "disconnected".to_string(),
            session_mode: "idle".to_string(),
            connected: false,
            _tick,
        };

        if app.cli.demo || app.cli.serial.is_some() {
            app.start_session(app.default_watch_mode());
        }

        app
    }

    fn default_watch_mode(&self) -> bool {
        self.cli.watch || self.startup_program.is_none()
    }

    fn input_default_watch_mode(&self, cx: &Context<Self>) -> bool {
        self.cli.watch || self.rseq_files_from_input(cx).is_empty()
    }

    fn prepare_connection_config(&mut self, cx: &Context<Self>) -> bool {
        match self.link_mode {
            LinkMode::Demo => {
                self.cli.demo = true;
                self.cli.serial = None;
                true
            }
            LinkMode::Serial => match self.resolve_serial_config(cx) {
                Ok((port, baud)) => {
                    self.cli.demo = false;
                    self.cli.serial = Some(port);
                    self.cli.baud = baud;
                    true
                }
                Err(reason) => {
                    push_bounded(&mut self.logs, reason, MAX_TEXT_LINES);
                    false
                }
            },
            mode => {
                push_bounded(
                    &mut self.logs,
                    format!("{} transport is not implemented yet", mode.label()),
                    MAX_TEXT_LINES,
                );
                false
            }
        }
    }

    fn resolve_serial_config(&mut self, cx: &Context<Self>) -> Result<(String, u32), String> {
        let mut port = self.serial_port_input.read(cx).value().trim().to_string();
        if port.is_empty() {
            self.serial_ports = rseq_host::available_serial_ports();
            match self.serial_ports.len() {
                0 => return Err("no serial ports found".to_string()),
                1 => port = self.serial_ports[0].port_name.clone(),
                _ => {
                    let ports = self
                        .serial_ports
                        .iter()
                        .map(|port| port.port_name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    return Err(format!("select a serial port first: {ports}"));
                }
            }
        }

        let baud = parse_serial_baud(&self.serial_baud_input.read(cx).value())?;
        Ok((port, baud))
    }

    fn rseq_files_from_input(&self, cx: &Context<Self>) -> Vec<PathBuf> {
        parse_path_list(&self.rseq_path_input.read(cx).value())
    }

    fn chip_files_from_input(&self, cx: &Context<Self>) -> Vec<PathBuf> {
        let chips = parse_path_list(&self.chip_path_input.read(cx).value());
        *self.sequence_lsp_chips.borrow_mut() = chips.clone();
        chips
    }

    fn refresh_sequence_editor_diagnostics(&self, cx: &mut Context<Self>) {
        let source = self.sequence_source_from_editor(cx);
        let chips = self.chip_files_from_input(cx);
        let analysis = rseq_lsp::analyze_document(
            &source.source,
            source.base_dir.as_deref(),
            std::env::current_dir().ok().as_deref(),
            &chips,
        );
        self.sequence_editor.update(cx, |input, cx| {
            let text = input.text().clone();
            if let Some(diagnostics) = input.diagnostics_mut() {
                diagnostics.reset(&text);
                diagnostics.extend(analysis.diagnostics.into_iter().map(|diagnostic| {
                    let range = rseq_lsp_range_to_gpui_range(
                        &source.source,
                        lsp_types::Range::new(
                            lsp_types::Position::new(
                                diagnostic.range.start.line,
                                diagnostic.range.start.character,
                            ),
                            lsp_types::Position::new(
                                diagnostic.range.end.line,
                                diagnostic.range.end.character,
                            ),
                        ),
                    );
                    lsp_types::Diagnostic {
                        range,
                        severity: Some(lsp_types::DiagnosticSeverity::ERROR),
                        source: diagnostic.source,
                        message: diagnostic.message,
                        ..Default::default()
                    }
                }));
            }
            cx.notify();
        });
    }

    fn reload_workspace_from_inputs(&mut self, compile_program: bool, cx: &Context<Self>) -> bool {
        self.cli.file = self.rseq_files_from_input(cx);
        self.cli.chip = self.chip_files_from_input(cx);

        let loaded = load_workspace(&self.cli.file, &self.cli.chip, compile_program);
        self.apply_loaded_workspace(loaded, "workspace")
    }

    fn reload_workspace_from_sequence_editor(
        &mut self,
        compile_program: bool,
        cx: &Context<Self>,
    ) -> bool {
        self.cli.chip = self.chip_files_from_input(cx);
        if let Some(path) = self.sequence_path.clone() {
            self.cli.file = vec![path];
        }

        let source = match self.sequence_source_from_current_view(cx) {
            Ok(source) => source,
            Err(errors) => {
                self.sequence_status = errors.join("\n");
                self.compile_status = format!("visual sequence error: {}", self.sequence_status);
                push_bounded(
                    &mut self.logs,
                    format!("sequence build failed: {}", self.sequence_status),
                    MAX_TEXT_LINES,
                );
                return false;
            }
        };
        let loaded = load_workspace_from_sources(&[source], &self.cli.chip, compile_program);
        let ok = self.apply_loaded_workspace(loaded, "sequence");
        self.sequence_status = self.compile_status.clone();
        ok
    }

    fn apply_loaded_workspace(&mut self, loaded: LoadedWorkspace, label: &str) -> bool {
        let ok = loaded.ok;
        self.metadata = loaded.metadata;
        self.startup_program = loaded.startup_program;
        self.compile_status = loaded.status.clone();
        self.registers.clear();
        self.sync_active_register_page();

        push_bounded(
            &mut self.logs,
            if ok {
                format!("{label} loaded: {}", loaded.status)
            } else {
                format!("{label} load failed: {}", loaded.status)
            },
            MAX_TEXT_LINES,
        );

        if self.session.is_some() {
            push_bounded(
                &mut self.logs,
                "decoder/register metadata changes apply on the next Watch or Load & Run"
                    .to_string(),
                MAX_TEXT_LINES,
            );
        }

        ok
    }

    fn sync_active_register_page(&mut self) {
        let pages = self.metadata.register_catalog.pages();
        if pages.is_empty() {
            self.active_register_page = None;
            return;
        }

        if self
            .active_register_page
            .as_ref()
            .is_some_and(|active| pages.iter().any(|page| page == active))
        {
            return;
        }

        self.active_register_page = pages.into_iter().next();
    }

    fn set_active_register_page(&mut self, page: String, cx: &mut Context<Self>) {
        self.active_register_page = Some(page);
        self.inline_write_addr = None;
        cx.notify();
    }

    fn connect_from_inputs(&mut self, cx: &Context<Self>) {
        if !self.prepare_connection_config(cx) {
            return;
        }
        let watch = self.input_default_watch_mode(cx);
        if self.reload_workspace_from_inputs(!watch, cx) {
            self.start_session(watch);
        }
    }

    fn load_and_run_from_inputs(&mut self, cx: &Context<Self>) {
        if !self.prepare_connection_config(cx) {
            return;
        }
        if !self.reload_workspace_from_inputs(true, cx) {
            return;
        }
        if self.startup_program.is_none() {
            push_bounded(
                &mut self.logs,
                "no compiled rseq program; choose an .rseq file first".to_string(),
                MAX_TEXT_LINES,
            );
            return;
        }
        self.start_session(false);
    }

    fn watch_from_inputs(&mut self, cx: &Context<Self>) {
        if !self.prepare_connection_config(cx) {
            return;
        }
        if self.reload_workspace_from_inputs(false, cx) {
            self.start_session(true);
        }
    }

    fn connect_from_current_source(&mut self, cx: &Context<Self>) {
        if self.use_sequence_editor_source(cx) {
            if !self.prepare_connection_config(cx) {
                return;
            }
            let watch = self.cli.watch || !self.current_view_has_source(cx);
            if self.reload_workspace_from_sequence_editor(!watch, cx) {
                self.start_session(watch);
            }
        } else {
            self.connect_from_inputs(cx);
        }
    }

    fn load_and_run_from_current_source(&mut self, cx: &Context<Self>) {
        if self.use_sequence_editor_source(cx) {
            self.load_and_run_sequence_editor(cx);
        } else {
            self.load_and_run_from_inputs(cx);
        }
    }

    fn watch_from_current_source(&mut self, cx: &Context<Self>) {
        if self.use_sequence_editor_source(cx) {
            if !self.prepare_connection_config(cx) {
                return;
            }
            if self.reload_workspace_from_sequence_editor(false, cx) {
                self.start_session(true);
            }
        } else {
            self.watch_from_inputs(cx);
        }
    }

    fn load_and_run_sequence_editor(&mut self, cx: &Context<Self>) {
        if !self.prepare_connection_config(cx) {
            return;
        }
        if !self.current_view_has_source(cx) {
            push_bounded(
                &mut self.logs,
                "no rseq source in current sequence view".to_string(),
                MAX_TEXT_LINES,
            );
            self.sequence_status = "no rseq source".to_string();
            return;
        }
        if !self.reload_workspace_from_sequence_editor(true, cx) {
            return;
        }
        if self.startup_program.is_none() {
            push_bounded(
                &mut self.logs,
                "sequence editor did not produce startup bytecode".to_string(),
                MAX_TEXT_LINES,
            );
            return;
        }
        self.start_session(false);
    }

    fn use_sequence_editor_source(&self, cx: &Context<Self>) -> bool {
        self.selected_tab == PanelTab::Sequences
            || self.sequence_dirty
            || (self.rseq_files_from_input(cx).is_empty() && self.sequence_path.is_some())
    }

    fn has_startup_source(&self, cx: &Context<Self>) -> bool {
        !self.rseq_files_from_input(cx).is_empty()
            || self.sequence_path.is_some()
            || self.sequence_dirty
            || (self.selected_tab == PanelTab::Sequences && self.current_view_has_source(cx))
    }

    fn sequence_has_source(&self, cx: &Context<Self>) -> bool {
        !self.sequence_editor.read(cx).value().trim().is_empty()
    }

    fn current_view_has_source(&self, cx: &Context<Self>) -> bool {
        match self.sequence_view_mode {
            SequenceViewMode::Text => self.sequence_has_source(cx),
            SequenceViewMode::Blocks => self
                .visual_sequences
                .iter()
                .any(|sequence| !sequence.steps.is_empty()),
        }
    }

    fn sequence_source_from_editor(&self, cx: &Context<Self>) -> RseqSource {
        let source = self.sequence_editor.read(cx).value().to_string();
        let base_dir = self
            .sequence_path
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .or_else(|| std::env::current_dir().ok());
        let name = self
            .sequence_path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "untitled.rseq".to_string());
        RseqSource::new(name, source, base_dir)
    }

    fn sequence_source_from_current_view(
        &self,
        cx: &Context<Self>,
    ) -> Result<RseqSource, Vec<String>> {
        if self.sequence_view_mode == SequenceViewMode::Blocks {
            let source = self.visual_source_all(cx)?;
            let base_dir = self
                .sequence_path
                .as_deref()
                .and_then(Path::parent)
                .map(Path::to_path_buf)
                .or_else(|| std::env::current_dir().ok());
            return Ok(RseqSource::new("visual-sequences.rseq", source, base_dir));
        }

        Ok(self.sequence_source_from_editor(cx))
    }

    fn current_sequence_text(&self, cx: &Context<Self>) -> String {
        if self.sequence_view_mode == SequenceViewMode::Blocks {
            self.visual_source_all(cx).unwrap_or_else(|errors| {
                format!("// visual sequence error:\n// {}", errors.join("\n// "))
            })
        } else {
            self.sequence_editor.read(cx).value().to_string()
        }
    }

    fn new_sequence(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sequence_path = None;
        self.sequence_dirty = false;
        self.sequence_status = "new untitled sequence".to_string();
        self.cli.file.clear();
        self.visual_sequences = visual_sequences_from_source(DEFAULT_RSEQ_SOURCE, window, cx);
        self.active_visual_sequence = 0;
        self.rseq_path_input.update(cx, |input, cx| {
            input.set_value("", window, cx);
        });
        self.sequence_editor.update(cx, |input, cx| {
            input.set_value(DEFAULT_RSEQ_SOURCE, window, cx);
        });
        self.startup_program = None;
        cx.notify();
    }

    fn open_sequence_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Open rseq sequence".into()),
        });

        self.sequence_status = "opening rseq...".to_string();
        cx.notify();

        cx.spawn_in(window, async move |view, cx| {
            let selected = receiver
                .await
                .ok()
                .and_then(|result| result.ok())
                .flatten()
                .and_then(|paths| paths.into_iter().next());

            let Some(path) = selected else {
                _ = cx.update(|_, cx| {
                    _ = view.update(cx, |this, cx| {
                        this.sequence_status = "open cancelled".to_string();
                        cx.notify();
                    });
                });
                return;
            };

            let result = std::fs::read_to_string(&path)
                .map_err(|err| format!("failed to read {}: {err}", path.display()));

            _ = cx.update(|window, cx| {
                _ = view.update(cx, |this, cx| {
                    match result {
                        Ok(source) => {
                            let visual_source = source.clone();
                            this.sequence_path = Some(path.clone());
                            this.sequence_dirty = false;
                            this.sequence_status = format!("opened {}", display_path_name(&path));
                            this.cli.file = vec![path.clone()];
                            this.rseq_path_input.update(cx, |input, cx| {
                                input.set_value(path.display().to_string(), window, cx);
                            });
                            this.sequence_editor.update(cx, |input, cx| {
                                input.set_value(source, window, cx);
                            });
                            this.visual_sequences =
                                visual_sequences_from_source(&visual_source, window, cx);
                            this.active_visual_sequence = 0;
                            this.reload_workspace_from_sequence_editor(true, cx);
                        }
                        Err(message) => {
                            this.sequence_status = message;
                        }
                    }
                    cx.notify();
                });
            });
        })
        .detach();
    }

    fn save_sequence_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(path) = self.sequence_path.clone() {
            self.write_sequence_file(path, window, cx);
        } else {
            self.save_sequence_file_as(window, cx);
        }
    }

    fn save_sequence_file_as(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let source = self.current_sequence_text(cx);
        let directory = self.sequence_save_directory();
        let default_name = self.sequence_default_file_name();
        let receiver = cx.prompt_for_new_path(&directory, Some(default_name.as_str()));

        self.sequence_status = "choosing save path...".to_string();
        cx.notify();

        cx.spawn_in(window, async move |view, cx| {
            let selected = receiver.await.ok().and_then(|result| result.ok()).flatten();
            let Some(path) = selected else {
                _ = cx.update(|_, cx| {
                    _ = view.update(cx, |this, cx| {
                        this.sequence_status = "save cancelled".to_string();
                        cx.notify();
                    });
                });
                return;
            };

            Self::write_sequence_source(view, path, source, cx).await;
        })
        .detach();
    }

    fn write_sequence_file(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let source = self.current_sequence_text(cx);
        self.sequence_status = format!("saving {}...", display_path_name(&path));
        cx.notify();

        cx.spawn_in(window, async move |view, cx| {
            Self::write_sequence_source(view, path, source, cx).await;
        })
        .detach();
    }

    async fn write_sequence_source(
        view: WeakEntity<Self>,
        path: PathBuf,
        source: String,
        cx: &mut AsyncWindowContext,
    ) {
        let result = std::fs::write(&path, source)
            .map_err(|err| format!("failed to save {}: {err}", path.display()));

        _ = cx.update(|window, cx| {
            _ = view.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        this.sequence_path = Some(path.clone());
                        this.sequence_dirty = false;
                        this.sequence_status = format!("saved {}", display_path_name(&path));
                        this.cli.file = vec![path.clone()];
                        this.rseq_path_input.update(cx, |input, cx| {
                            input.set_value(path.display().to_string(), window, cx);
                        });
                        this.reload_workspace_from_sequence_editor(true, cx);
                    }
                    Err(message) => {
                        this.sequence_status = message;
                    }
                }
                cx.notify();
            });
        });
    }

    fn sequence_save_directory(&self) -> PathBuf {
        self.sequence_path
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."))
    }

    fn sequence_default_file_name(&self) -> String {
        self.sequence_path
            .as_deref()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .map(ToString::to_string)
            .unwrap_or_else(|| "sequence.rseq".to_string())
    }

    fn open_chip_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Open chip YAML".into()),
        });

        self.compile_status = "choosing chip metadata...".to_string();
        cx.notify();

        cx.spawn_in(window, async move |view, cx| {
            let selected = receiver
                .await
                .ok()
                .and_then(|result| result.ok())
                .flatten()
                .and_then(|paths| paths.into_iter().next());

            _ = cx.update(|window, cx| {
                _ = view.update(cx, |this, cx| {
                    if let Some(path) = selected {
                        let value = path.display().to_string();
                        this.chip_path_input.update(cx, |input, cx| {
                            input.set_value(value, window, cx);
                        });
                        if this.current_view_has_source(cx) {
                            this.reload_workspace_from_sequence_editor(true, cx);
                        } else {
                            let compile_program = !this.rseq_files_from_input(cx).is_empty();
                            this.reload_workspace_from_inputs(compile_program, cx);
                        }
                    } else {
                        this.compile_status = "chip open cancelled".to_string();
                    }
                    cx.notify();
                });
            });
        })
        .detach();
    }

    fn save_capture_file_as(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.capture_records.is_empty() {
            push_bounded(
                &mut self.logs,
                "no report capture records to save".to_string(),
                MAX_TEXT_LINES,
            );
            cx.notify();
            return;
        }

        let directory = self.capture_save_directory();
        let default_name = self.capture_default_file_name();
        let receiver = cx.prompt_for_new_path(&directory, Some(default_name.as_str()));
        let records = self.capture_records.clone();
        let sidecar = self.capture_sidecar(cx);

        push_bounded(
            &mut self.logs,
            format!("choosing capture save path for {} report(s)", records.len()),
            MAX_TEXT_LINES,
        );
        cx.notify();

        cx.spawn_in(window, async move |view, cx| {
            let selected = receiver.await.ok().and_then(|result| result.ok()).flatten();
            let Some(path) = selected else {
                _ = cx.update(|_, cx| {
                    _ = view.update(cx, |this, cx| {
                        push_bounded(
                            &mut this.logs,
                            "capture save cancelled".to_string(),
                            MAX_TEXT_LINES,
                        );
                        cx.notify();
                    });
                });
                return;
            };

            Self::write_capture_files(view, path, records, sidecar, cx).await;
        })
        .detach();
    }

    async fn write_capture_files(
        view: WeakEntity<Self>,
        path: PathBuf,
        records: Vec<ReportCaptureRecord>,
        sidecar: CaptureSidecar,
        cx: &mut AsyncWindowContext,
    ) {
        let result = if path.extension().and_then(|ext| ext.to_str()) != Some("bin") {
            Err(format!(
                "capture path {} must end with .bin",
                path.display()
            ))
        } else {
            write_report_capture(&path, &records).and_then(|_| {
                let sidecar_path = capture_sidecar_path(&path);
                let json = serde_json::to_string_pretty(&sidecar)
                    .map_err(|err| format!("failed to encode capture metadata: {err}"))?;
                std::fs::write(&sidecar_path, json).map_err(|err| {
                    format!(
                        "failed to write capture metadata {}: {err}",
                        sidecar_path.display()
                    )
                })
            })
        };

        _ = cx.update(|_, cx| {
            _ = view.update(cx, |this, cx| {
                match result {
                    Ok(()) => push_bounded(
                        &mut this.logs,
                        format!(
                            "saved {} report(s) to {} and {}",
                            records.len(),
                            path.display(),
                            capture_sidecar_path(&path).display()
                        ),
                        MAX_TEXT_LINES,
                    ),
                    Err(message) => push_bounded(&mut this.logs, message, MAX_TEXT_LINES),
                }
                cx.notify();
            });
        });
    }

    fn replay_capture_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Replay rseq capture".into()),
        });

        push_bounded(
            &mut self.logs,
            "choosing capture replay file...".to_string(),
            MAX_TEXT_LINES,
        );
        cx.notify();

        cx.spawn_in(window, async move |view, cx| {
            let selected = receiver
                .await
                .ok()
                .and_then(|result| result.ok())
                .flatten()
                .and_then(|paths| paths.into_iter().next());

            let Some(path) = selected else {
                _ = cx.update(|_, cx| {
                    _ = view.update(cx, |this, cx| {
                        push_bounded(
                            &mut this.logs,
                            "capture replay cancelled".to_string(),
                            MAX_TEXT_LINES,
                        );
                        cx.notify();
                    });
                });
                return;
            };

            let records = read_report_capture(&path);
            let sidecar = read_capture_sidecar(&path);

            _ = cx.update(|window, cx| {
                _ = view.update(cx, |this, cx| {
                    match records {
                        Ok(records) => {
                            this.stop_session();
                            if let Some(sidecar) = sidecar {
                                if let Err(message) =
                                    this.apply_capture_sidecar(sidecar, window, cx)
                                {
                                    push_bounded(&mut this.logs, message, MAX_TEXT_LINES);
                                }
                            }
                            this.reset_stream_state(true);
                            this.session_mode = "replay".to_string();
                            this.connection_label = format!("replay {}", display_path_name(&path));
                            let count = records.len();
                            let mut processor =
                                ReportProcessor::new(this.metadata.report_decoders.clone());
                            for record in records {
                                let events =
                                    processor.handle_report(record.meta, record.kind, &record.args);
                                for event in events {
                                    this.apply_event(event);
                                }
                            }
                            push_bounded(
                                &mut this.logs,
                                format!("replayed {count} report(s) from {}", path.display()),
                                MAX_TEXT_LINES,
                            );
                        }
                        Err(message) => push_bounded(&mut this.logs, message, MAX_TEXT_LINES),
                    }
                    cx.notify();
                });
            });
        })
        .detach();
    }

    fn capture_save_directory(&self) -> PathBuf {
        self.sequence_path
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."))
    }

    fn capture_default_file_name(&self) -> String {
        let stem = self
            .sequence_path
            .as_deref()
            .and_then(Path::file_stem)
            .and_then(|name| name.to_str())
            .unwrap_or("rseq");
        format!("{stem}-capture.bin")
    }

    fn capture_sidecar(&self, cx: &Context<Self>) -> CaptureSidecar {
        CaptureSidecar {
            version: 1,
            format: "rseq-report-capture-bin-v1".to_string(),
            rseq_files: self
                .rseq_files_from_input(cx)
                .iter()
                .map(|path| path.display().to_string())
                .collect(),
            chip_files: self
                .chip_files_from_input(cx)
                .iter()
                .map(|path| path.display().to_string())
                .collect(),
            skip_samples: self.sample_skip_count,
            report_decoders: capture_decoder_meta(&self.metadata.report_decoders),
        }
    }

    fn apply_capture_sidecar(
        &mut self,
        sidecar: CaptureSidecar,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let registry = report_decoder_registry_from_sidecar(&sidecar)?;
        if !registry.is_empty() {
            self.metadata.report_decoders = registry;
        }

        self.cli.file = sidecar.rseq_files.iter().map(PathBuf::from).collect();
        self.cli.chip = sidecar.chip_files.iter().map(PathBuf::from).collect();
        self.rseq_path_input.update(cx, |input, cx| {
            input.set_value(sidecar.rseq_files.join("; "), window, cx);
        });
        self.chip_path_input.update(cx, |input, cx| {
            input.set_value(sidecar.chip_files.join("; "), window, cx);
        });
        self.sample_skip_count = sidecar.skip_samples;
        self.sample_skip_remaining = sidecar.skip_samples;
        self.skip_samples_input.update(cx, |input, cx| {
            input.set_value(sidecar.skip_samples.to_string(), window, cx);
        });
        Ok(())
    }

    fn start_session(&mut self, watch: bool) {
        if let Some(session) = self.session.take() {
            session.stop();
        }

        self.reset_stream_state(true);
        let demo = self.link_mode == LinkMode::Demo || self.cli.demo;
        let serial = if demo { None } else { self.cli.serial.clone() };
        self.connected = false;
        self.session_mode = if watch {
            "watch".to_string()
        } else {
            "load/run".to_string()
        };
        self.connection_label = if demo {
            "demo".to_string()
        } else {
            format!(
                "{} @ {}",
                serial.as_deref().unwrap_or("<none>"),
                self.cli.baud
            )
        };
        push_bounded(
            &mut self.logs,
            if watch {
                "starting watch session".to_string()
            } else {
                "starting load+run session".to_string()
            },
            MAX_TEXT_LINES,
        );

        let config = SessionConfig {
            serial,
            baud: self.cli.baud,
            watch,
            demo,
            startup_program: (!watch).then(|| self.startup_program.clone()).flatten(),
            report_decoders: self.metadata.report_decoders.clone(),
        };
        self.session = Some(rseq_host::spawn_session(config));
    }

    fn stop_session(&mut self) {
        if let Some(session) = self.session.take() {
            session.stop();
        }
        self.connected = false;
        self.connection_label = "disconnected".to_string();
        self.session_mode = "idle".to_string();
    }

    fn send_command(&mut self, command: SessionCommand) {
        let Some(session) = &self.session else {
            push_bounded(
                &mut self.logs,
                "no active session; connect first".to_string(),
                MAX_TEXT_LINES,
            );
            return;
        };
        if session.commands.send(command).is_err() {
            push_bounded(
                &mut self.logs,
                "session command channel is closed".to_string(),
                MAX_TEXT_LINES,
            );
        }
    }

    fn drain_session_events(&mut self) {
        let mut events = Vec::new();
        if let Some(session) = &self.session {
            while let Ok(event) = session.events.try_recv() {
                events.push(event);
                if events.len() > 4096 {
                    break;
                }
            }
        }
        for event in events {
            self.apply_event(event);
        }
    }

    fn apply_event(&mut self, event: SessionEvent) {
        match event {
            SessionEvent::Connected { label } => {
                self.connected = true;
                self.connection_label = label;
            }
            SessionEvent::Disconnected => {
                self.connected = false;
                push_bounded(
                    &mut self.logs,
                    "session disconnected".to_string(),
                    MAX_TEXT_LINES,
                );
            }
            SessionEvent::Log(line) => push_bounded(&mut self.logs, line, MAX_TEXT_LINES),
            SessionEvent::Error(line) => {
                push_bounded(&mut self.logs, format!("error: {line}"), MAX_TEXT_LINES)
            }
            SessionEvent::ExecStatus(status) => push_bounded(
                &mut self.logs,
                format!("exec status: {status}"),
                MAX_TEXT_LINES,
            ),
            SessionEvent::Register { addr, access, data } => {
                for (offset, byte) in data.iter().copied().enumerate() {
                    let cell_addr = addr + offset as u32;
                    if offset == 0 || !self.register_is_no_dump(cell_addr) {
                        self.registers.insert(
                            cell_addr,
                            RegisterValue {
                                access: access.clone(),
                                data: vec![byte],
                            },
                        );
                    }
                }
                if self.register_is_no_dump(addr) {
                    self.registers.insert(addr, RegisterValue { access, data });
                }
            }
            SessionEvent::Sample(sample) => {
                self.ingest_motion_sample(sample);
            }
            SessionEvent::Report(summary) => {
                self.push_capture_record(ReportCaptureRecord {
                    meta: summary.meta,
                    kind: summary.kind,
                    args: summary.args.clone(),
                });
                push_bounded(&mut self.reports, summary.line, MAX_TEXT_LINES);
            }
            SessionEvent::Health(health) => self.health = health,
        }
    }

    fn request_selected_register_dump(&mut self) {
        match self.selected_register_read_target(self.selected_register_addr) {
            Ok(target) => self.send_command(SessionCommand::ReadRegister {
                addr: target.addr,
                len: target.len,
                label: target.label,
            }),
            Err(reason) => push_bounded(&mut self.logs, reason, MAX_TEXT_LINES),
        }
    }

    fn request_active_register_page_dump(&mut self) {
        let page = self
            .active_register_page
            .clone()
            .unwrap_or_else(|| "registers".to_string());
        let registers = self.active_register_page_registers();
        let (ranges, skipped) = register_dump_ranges(&registers, REGISTER_DUMP_BATCH_MAX_LEN);

        if ranges.is_empty() {
            push_bounded(
                &mut self.logs,
                "no readable registers in active page".to_string(),
                MAX_TEXT_LINES,
            );
            return;
        }

        let byte_count: usize = ranges.iter().map(|range| range.len as usize).sum();
        push_bounded(
            &mut self.logs,
            format!(
                "dumping active page {page}: {} range(s), {byte_count} byte(s), skipped {} register(s)",
                ranges.len(),
                skipped.len()
            ),
            MAX_TEXT_LINES,
        );
        for range in &ranges {
            push_bounded(
                &mut self.logs,
                format!(
                    "dump range {page}.0x{:02x}..0x{:02x} len={}",
                    range.start,
                    range.start + range.len as u32 - 1,
                    range.len
                ),
                MAX_TEXT_LINES,
            );
        }
        for item in skipped.iter().take(8) {
            push_bounded(&mut self.logs, item.clone(), MAX_TEXT_LINES);
        }
        if skipped.len() > 8 {
            push_bounded(
                &mut self.logs,
                format!("... {} more skipped registers omitted", skipped.len() - 8),
                MAX_TEXT_LINES,
            );
        }

        for range in ranges {
            self.send_command(SessionCommand::ReadRegister {
                addr: range.start,
                len: range.len,
                label: format!("{page}.0x{:02x}+{}", range.start, range.len),
            });
        }
    }

    fn write_selected_register(&mut self, cx: &Context<Self>) {
        let value = self.write_input.read(cx).value().to_string();
        self.write_register_value(self.selected_register_addr, &value);
    }

    fn write_register_value(&mut self, addr: u32, value: &str) {
        let data = match parse_register_write_bytes(value) {
            Ok(data) => data,
            Err(reason) => {
                push_bounded(&mut self.logs, reason, MAX_TEXT_LINES);
                return;
            }
        };
        let target = match self.selected_register_write_target(addr) {
            Ok(target) => target,
            Err(reason) => {
                push_bounded(&mut self.logs, reason, MAX_TEXT_LINES);
                return;
            }
        };
        if let Some(width) = target.width {
            if data.len() != width {
                push_bounded(
                    &mut self.logs,
                    format!("expected {width} byte(s), got {}", data.len()),
                    MAX_TEXT_LINES,
                );
                return;
            }
        }
        self.send_command(SessionCommand::WriteRegister {
            addr: target.addr,
            data,
            label: target.label,
        });
    }

    fn begin_inline_register_write(
        &mut self,
        addr: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selected_register_addr = addr;
        let target = match self.selected_register_write_target(addr) {
            Ok(target) => target,
            Err(reason) => {
                push_bounded(&mut self.logs, reason, MAX_TEXT_LINES);
                return;
            }
        };

        let width = target.width.unwrap_or(1).max(1);
        let mut initial = self.register_bytes(target.addr, width);
        if initial.is_empty() {
            initial.push(0);
        }
        self.inline_write_addr = Some(target.addr);
        self.inline_write_input.update(cx, |input, cx| {
            input.set_value(hex_bytes(&initial[..initial.len().min(width)]), window, cx);
        });
    }

    fn commit_inline_register_write(&mut self, cx: &Context<Self>) {
        let Some(addr) = self.inline_write_addr.take() else {
            return;
        };
        let value = self.inline_write_input.read(cx).value().to_string();
        self.write_register_value(addr, &value);
    }

    fn register_bytes(&self, addr: u32, width: usize) -> Vec<u8> {
        if let Some(value) = self.registers.get(&addr) {
            if value.data.len() >= width {
                return value.data.iter().copied().take(width).collect();
            }
        }

        (0..width)
            .filter_map(|offset| {
                self.registers
                    .get(&(addr + offset as u32))
                    .and_then(|value| value.data.first())
                    .copied()
            })
            .collect()
    }

    fn active_register_page(&self) -> Option<&str> {
        self.active_register_page.as_deref()
    }

    fn register_infos_for_addr(&self, addr: u32) -> Vec<RegisterInfo> {
        let regs = if let Some(page) = self.active_register_page() {
            self.metadata
                .register_catalog
                .registers_for_page_addr(page, addr)
        } else {
            self.metadata.register_catalog.registers_for_addr(addr)
        };
        regs.into_iter().cloned().collect()
    }

    fn active_register_page_registers(&self) -> Vec<RegisterInfo> {
        if let Some(page) = self.active_register_page() {
            self.metadata
                .register_catalog
                .registers_for_page(page)
                .into_iter()
                .cloned()
                .collect()
        } else {
            self.metadata.register_catalog.registers().to_vec()
        }
    }

    fn register_is_no_dump(&self, addr: u32) -> bool {
        if let Some(page) = self.active_register_page() {
            self.metadata
                .register_catalog
                .is_no_dump_for_page(page, addr)
        } else {
            self.metadata.register_catalog.is_no_dump(addr)
        }
    }

    fn selected_register_read_target(
        &self,
        addr: u32,
    ) -> Result<rseq_host::RegisterReadTarget, String> {
        if let Some(page) = self.active_register_page() {
            self.metadata
                .register_catalog
                .selected_read_target_for_page(page, addr)
        } else {
            self.metadata.register_catalog.selected_read_target(addr)
        }
    }

    fn selected_register_write_target(
        &self,
        addr: u32,
    ) -> Result<rseq_host::RegisterWriteTarget, String> {
        if let Some(page) = self.active_register_page() {
            self.metadata
                .register_catalog
                .selected_write_target_for_page(page, addr)
        } else {
            self.metadata.register_catalog.selected_write_target(addr)
        }
    }

    fn axis_colors(cx: &Context<Self>) -> [Hsla; 3] {
        [cx.theme().red, cx.theme().green, cx.theme().blue]
    }

    fn reset_stream_state(&mut self, clear_capture: bool) {
        self.samples.clear();
        self.history_bars.clear();
        self.history_bucket = None;
        self.history_intraday = None;
        self.chart_drag = None;
        self.health = ReportHealth::default();
        self.sample_skip_remaining = self.sample_skip_count;
        if clear_capture {
            self.capture_records.clear();
        }
    }

    fn push_capture_record(&mut self, record: ReportCaptureRecord) {
        if self.capture_records.len() >= MAX_CAPTURE_RECORDS {
            self.capture_records.remove(0);
        }
        self.capture_records.push(record);
    }

    fn apply_sample_skip_setting(&mut self, cx: &Context<Self>) {
        match parse_nonnegative_usize(&self.skip_samples_input.read(cx).value(), "skip samples") {
            Ok(count) => {
                self.sample_skip_count = count;
                self.sample_skip_remaining = count;
                push_bounded(
                    &mut self.logs,
                    format!("sample skip set to {count}; next {count} sample(s) will be hidden"),
                    MAX_TEXT_LINES,
                );
            }
            Err(reason) => push_bounded(&mut self.logs, reason, MAX_TEXT_LINES),
        }
    }

    fn clear_motion_samples(&mut self) {
        self.samples.clear();
        self.history_bars.clear();
        self.history_bucket = None;
        self.history_intraday = None;
        self.chart_drag = None;
        self.sample_skip_remaining = self.sample_skip_count;
        push_bounded(
            &mut self.logs,
            "motion chart cleared".to_string(),
            MAX_TEXT_LINES,
        );
    }

    fn ingest_motion_sample(&mut self, sample: MotionSample) {
        if self.sample_skip_remaining > 0 {
            self.sample_skip_remaining -= 1;
            return;
        }

        let acc = motion_acc_vec3(&sample);
        let gyro = motion_gyro_vec3(&sample);

        if self.samples.len() == MAX_SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
        self.push_history_sample(acc, gyro, sample.timestamp_us);
    }

    fn push_history_sample(&mut self, acc: Vec3, gyro: Vec3, timestamp_us: Option<u64>) {
        let now = Instant::now();
        if self
            .history_bucket
            .as_ref()
            .is_some_and(|bucket| bucket.should_finish_before(timestamp_us, now))
        {
            self.finish_history_bucket();
        }

        match self.history_bucket.as_mut() {
            Some(bucket) => bucket.push(acc, gyro),
            None => self.history_bucket = Some(HistoryBucket::new(acc, gyro, timestamp_us, now)),
        }
    }

    fn finish_history_bucket(&mut self) {
        let Some(bucket) = self.history_bucket.take() else {
            return;
        };
        push_history_capped(&mut self.history_bars, bucket.to_bar());
    }

    fn acc_data(&self) -> Vec<Vec3> {
        self.samples.iter().map(motion_acc_vec3).collect()
    }

    fn gyro_data(&self) -> Vec<Vec3> {
        self.samples.iter().map(motion_gyro_vec3).collect()
    }

    fn series_data(&self, series: MotionSeries) -> Vec<Vec3> {
        match series {
            MotionSeries::Acc => self.acc_data(),
            MotionSeries::Gyro => self.gyro_data(),
        }
    }

    fn chart_range(&self, series: MotionSeries) -> Option<ChartRange> {
        match series {
            MotionSeries::Acc => self.acc_chart_range,
            MotionSeries::Gyro => self.gyro_chart_range,
        }
    }

    fn set_chart_range(&mut self, series: MotionSeries, range: Option<ChartRange>) {
        match series {
            MotionSeries::Acc => self.acc_chart_range = range,
            MotionSeries::Gyro => self.gyro_chart_range = range,
        }
    }

    fn chart_x_range(&self, series: MotionSeries) -> Option<ChartXRange> {
        match series {
            MotionSeries::Acc => self.acc_chart_x_range,
            MotionSeries::Gyro => self.gyro_chart_x_range,
        }
    }

    fn set_chart_x_range(&mut self, series: MotionSeries, range: Option<ChartXRange>) {
        match series {
            MotionSeries::Acc => self.acc_chart_x_range = range,
            MotionSeries::Gyro => self.gyro_chart_x_range = range,
        }
    }

    fn chart_bounds(&self, series: MotionSeries) -> Option<Bounds<Pixels>> {
        match series {
            MotionSeries::Acc => self.acc_chart_bounds,
            MotionSeries::Gyro => self.gyro_chart_bounds,
        }
    }

    fn set_chart_bounds(&mut self, series: MotionSeries, bounds: Bounds<Pixels>) {
        match series {
            MotionSeries::Acc => self.acc_chart_bounds = Some(bounds),
            MotionSeries::Gyro => self.gyro_chart_bounds = Some(bounds),
        }
    }

    fn history_bounds(&self, series: MotionSeries) -> Option<Bounds<Pixels>> {
        match series {
            MotionSeries::Acc => self.acc_history_bounds,
            MotionSeries::Gyro => self.gyro_history_bounds,
        }
    }

    fn set_history_bounds(&mut self, series: MotionSeries, bounds: Bounds<Pixels>) {
        match series {
            MotionSeries::Acc => self.acc_history_bounds = Some(bounds),
            MotionSeries::Gyro => self.gyro_history_bounds = Some(bounds),
        }
    }

    fn chart_display_range(
        &self,
        series: MotionSeries,
        data: &[Vec3],
        min_span: f32,
    ) -> ChartRange {
        self.chart_range(series)
            .unwrap_or_else(|| auto_chart_range(data, min_span))
    }

    fn chart_display_x_range(&self, series: MotionSeries, data_len: usize) -> ChartXRange {
        self.chart_x_range(series)
            .unwrap_or_else(|| auto_chart_x_range(data_len))
    }

    fn chart_display_zoom(&self, series: MotionSeries, data: &[Vec3], min_span: f32) -> f32 {
        let auto = auto_chart_range(data, min_span);
        let range = self.chart_display_range(series, data, min_span);
        (auto.span() / range.span()).clamp(CHART_ZOOM_MIN, CHART_ZOOM_MAX)
    }

    fn zoom_chart_from_wheel(
        &mut self,
        series: MotionSeries,
        min_span: f32,
        event: &ScrollWheelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let delta = event.delta.pixel_delta(window.line_height());
        let data = self.series_data(series);
        let auto = auto_chart_range(&data, min_span);
        let current = self.chart_range(series).unwrap_or(auto);
        let y_fraction = self
            .chart_bounds(series)
            .and_then(|bounds| chart_y_fraction_at(bounds, event.position))
            .unwrap_or(0.5);
        let x_fraction = self
            .chart_bounds(series)
            .and_then(|bounds| chart_x_fraction_at(bounds, event.position))
            .unwrap_or(0.5);
        let next_y = chart_range_after_wheel(current, auto, y_fraction, delta.y.as_f32());
        let auto_x = auto_chart_x_range(data.len());
        let current_x = self.chart_x_range(series).unwrap_or(auto_x);
        let next_x = chart_x_range_after_wheel(current_x, auto_x, x_fraction, delta.y.as_f32());
        self.set_chart_range(series, Some(next_y));
        self.set_chart_x_range(series, Some(next_x));
        cx.stop_propagation();
        cx.notify();
    }

    fn reset_chart_zoom(&mut self, series: MotionSeries, cx: &mut Context<Self>) {
        self.set_chart_range(series, None);
        self.set_chart_x_range(series, None);
        if self.chart_drag.is_some_and(|drag| drag.series == series) {
            self.chart_drag = None;
        }
        cx.notify();
    }

    fn start_chart_drag(
        &mut self,
        series: MotionSeries,
        min_span: f32,
        event: &MouseDownEvent,
        cx: &mut Context<Self>,
    ) {
        let Some(bounds) = self.chart_bounds(series) else {
            return;
        };
        if !bounds.contains(&event.position) {
            return;
        }

        let data = self.series_data(series);
        let auto_y = auto_chart_range(&data, min_span);
        let auto_x = auto_chart_x_range(data.len());
        self.chart_drag = Some(ChartDragState {
            series,
            start_position: event.position,
            bounds,
            y_range: self.chart_range(series).unwrap_or(auto_y),
            x_range: self.chart_x_range(series).unwrap_or(auto_x),
            auto_x_range: auto_x,
        });
        cx.stop_propagation();
        cx.notify();
    }

    fn update_chart_drag(
        &mut self,
        series: MotionSeries,
        event: &MouseMoveEvent,
        cx: &mut Context<Self>,
    ) {
        let Some(drag) = self.chart_drag else {
            return;
        };
        if drag.series != series {
            return;
        }
        if !event.dragging() {
            self.chart_drag = None;
            cx.notify();
            return;
        }

        let next_y = chart_range_after_drag(
            drag.y_range,
            drag.bounds,
            drag.start_position,
            event.position,
        );
        let next_x = chart_x_range_after_drag(
            drag.x_range,
            drag.auto_x_range,
            drag.bounds,
            drag.start_position,
            event.position,
        );
        self.set_chart_range(series, Some(next_y));
        self.set_chart_x_range(series, Some(next_x));
        cx.stop_propagation();
        cx.notify();
    }

    fn end_chart_drag(&mut self, series: MotionSeries, cx: &mut Context<Self>) {
        if self.chart_drag.is_some_and(|drag| drag.series == series) {
            self.chart_drag = None;
            cx.stop_propagation();
            cx.notify();
        }
    }

    fn show_history_intraday_at_position(
        &mut self,
        series: MotionSeries,
        event: &MouseDownEvent,
        cx: &mut Context<Self>,
    ) {
        let Some(bounds) = self.history_bounds(series) else {
            return;
        };
        let Some(bar_index) =
            history_bar_index_at_x(bounds, event.position, self.history_bar_count())
        else {
            return;
        };
        self.show_history_intraday(series, bar_index, cx);
    }

    fn show_history_intraday(
        &mut self,
        series: MotionSeries,
        bar_index: usize,
        cx: &mut Context<Self>,
    ) {
        let bar_count = self.history_bar_count();
        let Some(samples) = self.history_bar_samples(series, bar_index) else {
            return;
        };
        self.history_intraday = Some(HistoryIntradayView {
            series,
            bar_index,
            bar_count,
            samples,
        });
        cx.stop_propagation();
        cx.notify();
    }

    fn hide_history_intraday(&mut self, cx: &mut Context<Self>) {
        self.history_intraday = None;
        cx.notify();
    }

    fn temperature_data(&self) -> Vec<f32> {
        self.samples
            .iter()
            .filter_map(|sample| sample.temp_c.map(|value| value as f32))
            .collect()
    }

    fn latest_temperature_c(&self) -> Option<f64> {
        self.samples.iter().rev().find_map(|sample| sample.temp_c)
    }

    fn has_temperature_data(&self) -> bool {
        self.latest_temperature_c().is_some()
    }

    fn acc_history_data(&self) -> Vec<TripleOhlc> {
        let mut data = self
            .history_bars
            .iter()
            .map(|bar| bar.acc)
            .collect::<Vec<_>>();
        if let Some(bucket) = &self.history_bucket {
            data.push(bucket.acc);
        }
        data
    }

    fn gyro_history_data(&self) -> Vec<TripleOhlc> {
        let mut data = self
            .history_bars
            .iter()
            .map(|bar| bar.gyro)
            .collect::<Vec<_>>();
        if let Some(bucket) = &self.history_bucket {
            data.push(bucket.gyro);
        }
        data
    }

    fn history_bar_count(&self) -> usize {
        self.history_bars.len() + usize::from(self.history_bucket.is_some())
    }

    fn history_bar_samples(&self, series: MotionSeries, bar_index: usize) -> Option<Vec<Vec3>> {
        if let Some(bar) = self.history_bars.get(bar_index) {
            let samples = bar.samples_for(series);
            return (!samples.is_empty()).then(|| samples.to_vec());
        }

        if bar_index == self.history_bars.len() {
            let bucket = self.history_bucket.as_ref()?;
            let samples = match series {
                MotionSeries::Acc => &bucket.acc_samples,
                MotionSeries::Gyro => &bucket.gyro_samples,
            };
            return (!samples.is_empty()).then(|| samples.clone());
        }

        None
    }

    fn set_link_mode(&mut self, mode: LinkMode, window: &mut Window, cx: &mut Context<Self>) {
        if self.session.is_some() {
            push_bounded(
                &mut self.logs,
                "disconnect before changing link mode".to_string(),
                MAX_TEXT_LINES,
            );
            return;
        }

        self.link_mode = mode;
        self.cli.demo = mode == LinkMode::Demo;
        if mode == LinkMode::Serial {
            self.refresh_serial_ports(window, cx);
        } else {
            push_bounded(
                &mut self.logs,
                format!("selected {} link", mode.label()),
                MAX_TEXT_LINES,
            );
        }
    }

    fn refresh_serial_ports(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.serial_ports = rseq_host::available_serial_ports();
        match self.serial_ports.len() {
            0 => push_bounded(
                &mut self.logs,
                "no serial ports found".to_string(),
                MAX_TEXT_LINES,
            ),
            1 => {
                let port = self.serial_ports[0].port_name.clone();
                if self.serial_port_input.read(cx).value().trim().is_empty() {
                    self.serial_port_input.update(cx, |input, cx| {
                        input.set_value(port.clone(), window, cx);
                    });
                }
                push_bounded(
                    &mut self.logs,
                    format!("found 1 serial port: {port}"),
                    MAX_TEXT_LINES,
                );
            }
            count => {
                push_bounded(
                    &mut self.logs,
                    format!("found {count} serial ports; choose one from Ports"),
                    MAX_TEXT_LINES,
                );
            }
        }
    }

    fn select_serial_port(&mut self, port: String, window: &mut Window, cx: &mut Context<Self>) {
        if port.is_empty() {
            return;
        }
        self.serial_port_input.update(cx, |input, cx| {
            input.set_value(port.clone(), window, cx);
        });
        push_bounded(
            &mut self.logs,
            format!("selected serial port {port}"),
            MAX_TEXT_LINES,
        );
    }

    fn select_serial_baud(&mut self, baud: u32, window: &mut Window, cx: &mut Context<Self>) {
        self.serial_baud_input.update(cx, |input, cx| {
            input.set_value(baud.to_string(), window, cx);
        });
        push_bounded(
            &mut self.logs,
            format!("selected serial baud {baud}"),
            MAX_TEXT_LINES,
        );
    }

    fn render_link_mode_picker(&self, locked: bool, cx: &Context<Self>) -> impl IntoElement {
        let current = self.link_mode;
        h_flex()
            .gap_1()
            .items_center()
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("Link"),
            )
            .child(
                DropdownButton::new("link-mode-dropdown")
                    .xsmall()
                    .button(Button::new("link-mode-button").label(current.label()))
                    .disabled(locked)
                    .tooltip(current.tooltip())
                    .dropdown_menu_with_anchor(Anchor::BottomLeft, move |menu, _, _| {
                        LinkMode::ALL.iter().copied().fold(menu, |menu, mode| {
                            menu.menu_with_check(
                                mode.label(),
                                current == mode,
                                Box::new(SelectLinkModeAction(mode.id())),
                            )
                        })
                    }),
            )
    }

    fn render_endpoint_input(
        &self,
        label: &str,
        input: &Entity<InputState>,
        width: f32,
        disabled: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        h_flex()
            .gap_1()
            .items_center()
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(label.to_string()),
            )
            .child(
                div()
                    .w(px(width))
                    .child(Input::new(input).xsmall().disabled(disabled)),
            )
    }

    fn render_serial_port_dropdown(&self, locked: bool, cx: &Context<Self>) -> impl IntoElement {
        let selected_port = self.serial_port_input.read(cx).value().to_string();
        let ports = self.serial_ports.clone();
        let label = if ports.is_empty() {
            "Ports".to_string()
        } else {
            format!("Ports ({})", ports.len())
        };

        DropdownButton::new("serial-port-dropdown")
            .xsmall()
            .button(
                Button::new("serial-port-button").label(label).on_click(
                    cx.listener(|this, _, window, cx| this.refresh_serial_ports(window, cx)),
                ),
            )
            .disabled(locked)
            .tooltip("Scan local serial ports or choose a scanned port")
            .dropdown_menu_with_anchor(Anchor::BottomLeft, move |menu, _, _| {
                if ports.is_empty() {
                    menu.menu_with_check_and_disabled(
                        "No scanned ports",
                        false,
                        Box::new(SelectSerialPortAction(String::new())),
                        true,
                    )
                } else {
                    ports.iter().fold(menu, |menu, port| {
                        menu.menu_with_check(
                            serial_port_menu_label(port),
                            selected_port == port.port_name,
                            Box::new(SelectSerialPortAction(port.port_name.clone())),
                        )
                    })
                }
            })
    }

    fn render_serial_baud_dropdown(&self, locked: bool, cx: &Context<Self>) -> impl IntoElement {
        let selected_baud = self
            .serial_baud_input
            .read(cx)
            .value()
            .trim()
            .parse::<u32>()
            .ok();

        DropdownButton::new("serial-baud-dropdown")
            .xsmall()
            .button(Button::new("serial-baud-button").label("Rates"))
            .disabled(locked)
            .tooltip("Choose a common serial baud rate")
            .dropdown_menu_with_anchor(Anchor::BottomLeft, move |menu, _, _| {
                COMMON_SERIAL_BAUDS.iter().fold(menu, |menu, baud| {
                    menu.menu_with_check(
                        baud.to_string(),
                        selected_baud == Some(*baud),
                        Box::new(SelectSerialBaudAction(*baud)),
                    )
                })
            })
    }

    fn render_link_endpoint(&self, locked: bool, cx: &Context<Self>) -> AnyElement {
        match self.link_mode {
            LinkMode::Demo => h_flex()
                .h_5()
                .items_center()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child("No endpoint required")
                .into_any_element(),
            LinkMode::Serial => h_flex()
                .gap_1()
                .items_center()
                .flex_wrap()
                .child(self.render_endpoint_input(
                    "Port",
                    &self.serial_port_input,
                    190.,
                    locked,
                    cx,
                ))
                .child(self.render_serial_port_dropdown(locked, cx))
                .child(self.render_endpoint_input("Baud", &self.serial_baud_input, 82., locked, cx))
                .child(self.render_serial_baud_dropdown(locked, cx))
                .into_any_element(),
            mode => h_flex()
                .h_5()
                .items_center()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(format!("{} transport is a reserved slot", mode.label()))
                .into_any_element(),
        }
    }

    fn render_connection_bar(&self, cx: &Context<Self>) -> impl IntoElement {
        let has_startup_source = self.has_startup_source(cx);
        let locked = self.session.is_some();
        let can_connect = self.link_mode.can_connect();
        v_flex()
            .gap_2()
            .p_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .gap_3()
                    .justify_between()
                    .flex_wrap()
                    .child(
                        h_flex()
                            .gap_2()
                            .flex_wrap()
                            .items_center()
                            .child(status_dot(self.connected, cx))
                            .child(
                                div()
                                    .text_sm()
                                    .font_semibold()
                                    .child(self.connection_label.clone()),
                            )
                            .child(self.render_link_mode_picker(locked, cx))
                            .child(self.render_link_endpoint(locked, cx))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(format!(
                                        "reports={} dropped={} dt={}",
                                        self.health.total_reports,
                                        self.health.dropped_frames,
                                        self.health
                                            .last_dt_us
                                            .map(|dt| format!("{dt}us"))
                                            .unwrap_or_else(|| "-".to_string())
                                    )),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("connect")
                                    .small()
                                    .label("Connect")
                                    .disabled(!can_connect)
                                    .on_click(cx.listener(|this, _, _window, cx| {
                                        this.connect_from_current_source(cx);
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("load-run")
                                    .small()
                                    .primary()
                                    .label("Load & Run")
                                    .disabled(!has_startup_source || !can_connect)
                                    .on_click(cx.listener(|this, _, _window, cx| {
                                        this.load_and_run_from_current_source(cx);
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("watch")
                                    .small()
                                    .label("Watch")
                                    .disabled(!can_connect)
                                    .on_click(cx.listener(|this, _, _window, cx| {
                                        this.watch_from_current_source(cx);
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("ping")
                                    .small()
                                    .label("Ping")
                                    .disabled(!self.connected)
                                    .on_click(cx.listener(|this, _, _window, cx| {
                                        this.send_command(SessionCommand::Ping);
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("stop-reports")
                                    .small()
                                    .label("Stop")
                                    .disabled(!self.connected)
                                    .on_click(cx.listener(|this, _, _window, cx| {
                                        this.send_command(SessionCommand::StopReports);
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("reset-mcu")
                                    .small()
                                    .label("Reset")
                                    .disabled(!self.connected)
                                    .on_click(cx.listener(|this, _, _window, cx| {
                                        this.send_command(SessionCommand::ResetMcu);
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("disconnect")
                                    .small()
                                    .label("Disconnect")
                                    .disabled(self.session.is_none())
                                    .on_click(cx.listener(|this, _, _window, cx| {
                                        this.stop_session();
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
    }

    fn render_chart(
        &self,
        series: MotionSeries,
        title: &str,
        data: Vec<Vec3>,
        min_span: f32,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let colors = Self::axis_colors(cx);
        let latest = data.last().copied().unwrap_or([0.0; 3]);
        let stats = axis_stddev(&data);
        let range = self.chart_display_range(series, &data, min_span);
        let x_range = self.chart_display_x_range(series, data.len());
        let zoom = self.chart_display_zoom(series, &data, min_span);
        let labels = ["x", "y", "z"];
        let view = cx.entity();
        v_flex()
            .flex_1()
            .min_h(px(150.))
            .border_1()
            .border_color(cx.theme().border)
            .rounded(cx.theme().radius)
            .overflow_hidden()
            .child(
                h_flex()
                    .justify_between()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(div().text_sm().font_semibold().child(title.to_string()))
                    .child(
                        h_flex()
                            .gap_3()
                            .children((0..3).map(|idx| {
                                h_flex()
                                    .gap_1()
                                    .child(div().size_2().rounded_full().bg(colors[idx]))
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(format!(
                                                "{} {:+.2} σ{:.2}",
                                                labels[idx], latest[idx], stats[idx]
                                            )),
                                    )
                            }))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(format!("{zoom:.2}x")),
                            )
                            .child(
                                Button::new(format!("reset-{}-zoom", series.label()))
                                    .xsmall()
                                    .label("Reset")
                                    .disabled(
                                        self.chart_range(series).is_none()
                                            && self.chart_x_range(series).is_none(),
                                    )
                                    .on_click(cx.listener(move |this, _, _window, cx| {
                                        this.reset_chart_zoom(series, cx);
                                    })),
                            ),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .p_2()
                    .cursor_grab()
                    .on_prepaint(move |bounds, _window, cx| {
                        view.update(cx, |this, _| this.set_chart_bounds(series, bounds));
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseDownEvent, _window, cx| {
                            this.start_chart_drag(series, min_span, event, cx);
                        }),
                    )
                    .on_mouse_move(
                        cx.listener(move |this, event: &MouseMoveEvent, _window, cx| {
                            this.update_chart_drag(series, event, cx);
                        }),
                    )
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(move |this, _event: &MouseUpEvent, _window, cx| {
                            this.end_chart_drag(series, cx);
                        }),
                    )
                    .on_mouse_up_out(
                        MouseButton::Left,
                        cx.listener(move |this, _event: &MouseUpEvent, _window, cx| {
                            this.end_chart_drag(series, cx);
                        }),
                    )
                    .on_scroll_wheel(cx.listener(
                        move |this, event: &ScrollWheelEvent, window, cx| {
                            this.zoom_chart_from_wheel(series, min_span, event, window, cx);
                        },
                    ))
                    .child(TripleLineChart::new_with_ranges(
                        data,
                        colors,
                        x_range.x_min,
                        x_range.x_max,
                        range.y_min,
                        range.y_max,
                    )),
            )
    }

    fn render_temperature_panel(&self, cx: &Context<Self>) -> impl IntoElement {
        let data = self.temperature_data();
        let latest = self.latest_temperature_c().unwrap_or_default();
        let color = cx.theme().yellow;

        v_flex()
            .h(px(116.))
            .min_h(px(104.))
            .flex_none()
            .border_1()
            .border_color(cx.theme().border)
            .rounded(cx.theme().radius)
            .overflow_hidden()
            .child(
                h_flex()
                    .justify_between()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().size_2().rounded_full().bg(color))
                            .child(
                                div()
                                    .text_sm()
                                    .font_semibold()
                                    .child(format!("Temperature {latest:.2} C")),
                            ),
                    )
                    .child(
                        Button::new("hide-temperature")
                            .small()
                            .label("Hide")
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.show_temperature_panel = false;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .p_2()
                    .child(ScalarLineChart::new(data, color, 1.0)),
            )
    }

    fn render_temperature_collapsed(&self, cx: &Context<Self>) -> impl IntoElement {
        let latest = self.latest_temperature_c().unwrap_or_default();
        h_flex()
            .flex_none()
            .justify_between()
            .px_3()
            .py_1()
            .border_1()
            .border_color(cx.theme().border)
            .rounded(cx.theme().radius)
            .child(
                h_flex()
                    .gap_2()
                    .child(div().size_2().rounded_full().bg(cx.theme().yellow))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!("Temperature hidden · latest {latest:.2} C")),
                    ),
            )
            .child(
                Button::new("show-temperature")
                    .small()
                    .label("Show")
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.show_temperature_panel = true;
                        cx.notify();
                    })),
            )
    }

    fn render_history_chart(
        &self,
        series: MotionSeries,
        title: &str,
        data: Vec<TripleOhlc>,
        min_span: f32,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let colors = Self::axis_colors(cx);
        let labels = ["x", "y", "z"];
        let latest = data.last().copied();
        let view = cx.entity();

        v_flex()
            .flex_1()
            .min_w(px(240.))
            .h_full()
            .overflow_hidden()
            .child(
                h_flex()
                    .justify_between()
                    .px_2()
                    .pb_1()
                    .child(div().text_xs().font_semibold().child(title.to_string()))
                    .child(h_flex().gap_2().children((0..3).map(|idx| {
                        let range = latest
                            .map(|bucket| bucket[idx].high - bucket[idx].low)
                            .unwrap_or(0.0);
                        h_flex()
                            .gap_1()
                            .child(div().size_2().rounded_full().bg(colors[idx]))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(format!("{} d{:.2}", labels[idx], range)),
                            )
                    }))),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .on_prepaint(move |bounds, _window, cx| {
                        view.update(cx, |this, _| this.set_history_bounds(series, bounds));
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseDownEvent, _window, cx| {
                            if event.click_count >= 2 {
                                this.show_history_intraday_at_position(series, event, cx);
                            }
                        }),
                    )
                    .child(TripleOhlcChart::new(data, colors, min_span)),
            )
    }

    fn render_history_panel(&self, cx: &Context<Self>) -> impl IntoElement {
        let acc_data = self.acc_history_data();
        let gyro_data = self.gyro_history_data();
        let bars = acc_data.len().max(gyro_data.len());
        let intraday = self.history_intraday.as_ref();

        v_flex()
            .h(px(170.))
            .min_h(px(150.))
            .flex_none()
            .border_1()
            .border_color(cx.theme().border)
            .rounded(cx.theme().radius)
            .overflow_hidden()
            .child(
                h_flex()
                    .justify_between()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        div()
                            .text_sm()
                            .font_semibold()
                            .child(format!("{HISTORY_BUCKET_SECS}s OHLC History")),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(match intraday {
                                        Some(view) => format!(
                                            "{} intraday · bar {}/{} · {} samples",
                                            view.series.label(),
                                            view.bar_index + 1,
                                            view.bar_count,
                                            view.samples.len()
                                        ),
                                        None => format!(
                                            "{bars}/{MAX_HISTORY_BARS} bars · double-click a chart"
                                        ),
                                    }),
                            )
                            .when(intraday.is_some(), |this| {
                                this.child(
                                    Button::new("history-ohlc-view")
                                        .xsmall()
                                        .label("OHLC")
                                        .on_click(cx.listener(|this, _, _window, cx| {
                                            this.hide_history_intraday(cx);
                                        })),
                                )
                            }),
                    ),
            )
            .child(match intraday {
                Some(view) => self.render_history_intraday(view, cx).into_any_element(),
                None => div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h_0()
                    .gap_3()
                    .p_2()
                    .overflow_hidden()
                    .child(self.render_history_chart(
                        MotionSeries::Acc,
                        "Accelerometer (m/s^2)",
                        acc_data,
                        12.0,
                        cx,
                    ))
                    .child(self.render_history_chart(
                        MotionSeries::Gyro,
                        "Gyroscope (rad/s)",
                        gyro_data,
                        2.0,
                        cx,
                    ))
                    .into_any_element(),
            })
    }

    fn render_history_intraday(
        &self,
        view: &HistoryIntradayView,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let colors = Self::axis_colors(cx);
        let series = view.series;
        let data = view.samples.clone();
        let min_span = match series {
            MotionSeries::Acc => 12.0,
            MotionSeries::Gyro => 2.0,
        };
        let range = self.chart_display_range(series, &data, min_span);
        let x_range = auto_chart_x_range(data.len());

        div()
            .flex_1()
            .min_h_0()
            .p_2()
            .child(TripleLineChart::new_with_ranges(
                data,
                colors,
                x_range.x_min,
                x_range.x_max,
                range.y_min,
                range.y_max,
            ))
    }

    fn render_motion(&self, cx: &Context<Self>) -> AnyElement {
        v_flex()
            .size_full()
            .min_h_0()
            .overflow_hidden()
            .p_3()
            .gap_3()
            .child(self.render_motion_toolbar(cx))
            .child(self.render_chart(
                MotionSeries::Acc,
                "Accelerometer (m/s^2)",
                self.acc_data(),
                12.0,
                cx,
            ))
            .child(self.render_chart(
                MotionSeries::Gyro,
                "Gyroscope (rad/s)",
                self.gyro_data(),
                2.0,
                cx,
            ))
            .when(
                self.has_temperature_data() && self.show_temperature_panel,
                |this| this.child(self.render_temperature_panel(cx)),
            )
            .when(
                self.has_temperature_data() && !self.show_temperature_panel,
                |this| this.child(self.render_temperature_collapsed(cx)),
            )
            .child(self.render_history_panel(cx))
            .into_any_element()
    }

    fn render_motion_toolbar(&self, cx: &Context<Self>) -> impl IntoElement {
        h_flex()
            .justify_between()
            .items_center()
            .gap_2()
            .flex_wrap()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().text_sm().font_semibold().child("Motion Stream"))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!(
                                "samples={} skip_remaining={}",
                                self.samples.len(),
                                self.sample_skip_remaining
                            )),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("Skip samples"),
                    )
                    .child(
                        div()
                            .w(px(70.))
                            .child(Input::new(&self.skip_samples_input).xsmall()),
                    )
                    .child(
                        Button::new("apply-sample-skip")
                            .xsmall()
                            .label("Apply")
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.apply_sample_skip_setting(cx);
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("clear-motion")
                            .xsmall()
                            .label("Clear")
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.clear_motion_samples();
                                cx.notify();
                            })),
                    ),
            )
    }

    fn render_reports(&self, cx: &Context<Self>) -> AnyElement {
        v_flex()
            .size_full()
            .min_h_0()
            .overflow_hidden()
            .child(self.render_reports_toolbar(cx))
            .child(text_panel("Reports", &self.reports, cx))
            .into_any_element()
    }

    fn render_reports_toolbar(&self, cx: &Context<Self>) -> impl IntoElement {
        h_flex()
            .flex_none()
            .justify_between()
            .items_center()
            .gap_2()
            .p_3()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().text_sm().font_semibold().child("Report Capture"))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!(
                                "{} record(s), {} decoder(s)",
                                self.capture_records.len(),
                                self.metadata.report_decoders.len()
                            )),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("save-capture")
                            .small()
                            .label("Save Capture")
                            .disabled(self.capture_records.is_empty())
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.save_capture_file_as(window, cx);
                            })),
                    )
                    .child(
                        Button::new("replay-capture")
                            .small()
                            .label("Replay Capture")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.replay_capture_file(window, cx);
                            })),
                    ),
            )
    }

    fn render_logs(&self, cx: &Context<Self>) -> AnyElement {
        text_panel("Logs", &self.logs, cx).into_any_element()
    }

    fn render_sequences(&self, cx: &Context<Self>) -> AnyElement {
        div()
            .flex()
            .flex_row()
            .size_full()
            .min_h_0()
            .overflow_hidden()
            .child(self.render_sequence_sidebar(cx))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .overflow_hidden()
                    .p_3()
                    .gap_2()
                    .child(self.render_sequence_view_toolbar(cx))
                    .child(match self.sequence_view_mode {
                        SequenceViewMode::Text => self.render_sequence_text_view(cx),
                        SequenceViewMode::Blocks => self.render_sequence_blocks_view(cx),
                    }),
            )
            .into_any_element()
    }

    fn render_sequence_view_toolbar(&self, cx: &Context<Self>) -> impl IntoElement {
        h_flex()
            .justify_between()
            .items_center()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .text_sm()
                            .font_semibold()
                            .truncate()
                            .child(self.sequence_title()),
                    )
                    .children(SequenceViewMode::ALL.into_iter().map(|mode| {
                        Button::new(format!("sequence-view-{}", mode.label()))
                            .small()
                            .label(mode.label())
                            .selected(self.sequence_view_mode == mode)
                            .on_click(cx.listener(move |this, _, _window, cx| {
                                this.set_sequence_view_mode(mode, cx);
                            }))
                    })),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(match self.sequence_view_mode {
                        SequenceViewMode::Text => self.sequence_editor_stats(cx),
                        SequenceViewMode::Blocks => self.visual_sequence_stats(cx),
                    }),
            )
    }

    fn render_sequence_text_view(&self, cx: &Context<Self>) -> AnyElement {
        div()
            .flex_1()
            .min_h_0()
            .border_1()
            .border_color(cx.theme().border)
            .rounded(cx.theme().radius)
            .text_color(cx.theme().foreground)
            .font_family("monospace")
            .text_sm()
            .overflow_hidden()
            .child(Input::new(&self.sequence_editor).w_full().h_full())
            .into_any_element()
    }

    fn render_sequence_blocks_view(&self, cx: &Context<Self>) -> AnyElement {
        div()
            .flex_1()
            .min_h_0()
            .border_1()
            .border_color(cx.theme().border)
            .rounded(cx.theme().radius)
            .overflow_hidden()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .size_full()
                    .min_h_0()
                    .overflow_hidden()
                    .child(self.render_visual_sequence_list(cx))
                    .child(self.render_visual_sequence_editor(cx)),
            )
            .into_any_element()
    }

    fn render_visual_sequence_list(&self, cx: &Context<Self>) -> impl IntoElement {
        v_flex()
            .w(px(250.))
            .h_full()
            .flex_none()
            .min_h_0()
            .border_r_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().secondary)
            .child(
                h_flex()
                    .flex_none()
                    .p_2()
                    .gap_1()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        Button::new("visual-add-sequence")
                            .xsmall()
                            .label("Add Sequence")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.add_visual_sequence(window, cx);
                            })),
                    )
                    .child(
                        Button::new("visual-delete-sequence")
                            .xsmall()
                            .danger()
                            .label("Delete")
                            .disabled(self.visual_sequences.is_empty())
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.delete_active_visual_sequence(cx);
                            })),
                    ),
            )
            .child(
                div()
                    .id("visual-sequence-scroll-area")
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .child(
                        v_flex()
                            .id("visual-sequence-list")
                            .size_full()
                            .overflow_y_scrollbar()
                            .p_2()
                            .gap_1()
                            .children(self.visual_sequences.iter().enumerate().map(
                                |(index, sequence)| {
                                    let selected = self.active_visual_sequence == index;
                                    let sequence_name = sequence.name(cx);
                                    let entity_id = cx.entity_id();
                                    let drag = DragVisualSequence {
                                        entity_id,
                                        sequence_index: index,
                                        label: sequence_name.clone(),
                                    };
                                    h_flex()
                                        .id(("visual-sequence", index))
                                        .w_full()
                                        .h(px(34.))
                                        .flex_none()
                                        .px_2()
                                        .py_1()
                                        .gap_2()
                                        .items_center()
                                        .border_t_2()
                                        .border_color(cx.theme().transparent)
                                        .rounded(cx.theme().radius)
                                        .cursor_pointer()
                                        .when(selected, |this| this.bg(cx.theme().accent))
                                        .hover(|this| this.bg(cx.theme().muted))
                                        .can_drop(move |dragged, _, _| {
                                            dragged
                                                .downcast_ref::<DragVisualSequence>()
                                                .is_some_and(|drag| drag.entity_id == entity_id)
                                        })
                                        .drag_over::<DragVisualSequence>(
                                            move |this, drag, _, cx| {
                                                if drag.entity_id == entity_id {
                                                    this.border_color(cx.theme().drag_border)
                                                        .bg(cx.theme().drop_target)
                                                } else {
                                                    this
                                                }
                                            },
                                        )
                                        .on_drop(cx.listener(
                                            move |this, drag: &DragVisualSequence, _window, cx| {
                                                this.drop_visual_sequence_before(drag, index, cx)
                                            },
                                        ))
                                        .on_click(cx.listener(move |this, _, _window, cx| {
                                            this.set_active_visual_sequence(index, cx);
                                        }))
                                        .child(self.render_visual_sequence_drag_handle(drag, cx))
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .text_sm()
                                                .font_semibold()
                                                .truncate()
                                                .child(sequence_name),
                                        )
                                        .child(
                                            div()
                                                .font_family("monospace")
                                                .text_xs()
                                                .text_color(cx.theme().muted_foreground)
                                                .child(sequence.steps.len().to_string()),
                                        )
                                },
                            ))
                            .child(self.render_visual_sequence_end_drop_zone(cx)),
                    ),
            )
            .child(
                v_flex()
                    .flex_none()
                    .p_2()
                    .gap_1()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .child(
                        Button::new("visual-apply-text")
                            .xsmall()
                            .icon(IconName::Check)
                            .label("Apply To Text")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.apply_visual_to_text(window, cx);
                            })),
                    )
                    .child(
                        Button::new("visual-run-active")
                            .xsmall()
                            .primary()
                            .icon(IconName::Play)
                            .label("Run Active")
                            .disabled(!self.connected && self.cli.serial.is_some())
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.load_and_run_active_visual_sequence(cx);
                                cx.notify();
                            })),
                    ),
            )
    }

    fn render_visual_sequence_drag_handle(
        &self,
        drag: DragVisualSequence,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        div()
            .id(format!(
                "visual-sequence-drag-handle-{}",
                drag.sequence_index
            ))
            .w(px(24.))
            .h_6()
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().secondary)
            .cursor_grab()
            .text_color(cx.theme().muted_foreground)
            .hover(|this| this.bg(cx.theme().muted))
            .tooltip(|window, cx| Tooltip::new("Drag to reorder").build(window, cx))
            .child(Icon::new(IconName::Menu).xsmall())
            .on_drag(drag, |drag, _, _, cx| {
                cx.stop_propagation();
                cx.new(|_| drag.clone())
            })
    }

    fn render_visual_sequence_end_drop_zone(&self, cx: &Context<Self>) -> impl IntoElement {
        let entity_id = cx.entity_id();

        div()
            .id("visual-sequence-end-drop")
            .h(px(10.))
            .w_full()
            .flex_none()
            .border_t_2()
            .border_color(cx.theme().transparent)
            .can_drop(move |dragged, _, _| {
                dragged
                    .downcast_ref::<DragVisualSequence>()
                    .is_some_and(|drag| drag.entity_id == entity_id)
            })
            .drag_over::<DragVisualSequence>(move |this, drag, _, cx| {
                if drag.entity_id == entity_id {
                    this.border_color(cx.theme().drag_border)
                        .bg(cx.theme().drop_target)
                } else {
                    this
                }
            })
            .on_drop(
                cx.listener(move |this, drag: &DragVisualSequence, _window, cx| {
                    this.drop_visual_sequence_at_end(drag, cx)
                }),
            )
    }

    fn render_visual_sequence_editor(&self, cx: &Context<Self>) -> impl IntoElement {
        let Some(sequence) = self.visual_sequences.get(self.active_visual_sequence) else {
            return v_flex()
                .flex_1()
                .h_full()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child("Add a sequence to begin.")
                .into_any_element();
        };

        v_flex()
            .flex_1()
            .h_full()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .child(
                h_flex()
                    .flex_none()
                    .px_3()
                    .py_2()
                    .gap_2()
                    .items_center()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        div()
                            .w(px(220.))
                            .child(Input::new(&sequence.name_input).small()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!("{} steps", sequence.steps.len())),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("visual-add-read")
                            .small()
                            .label("Add Read")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.add_visual_step(VisualStepKind::Read, window, cx);
                            })),
                    )
                    .child(
                        Button::new("visual-add-write")
                            .small()
                            .label("Add Write")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.add_visual_step(VisualStepKind::Write, window, cx);
                            })),
                    ),
            )
            .child(self.render_visual_validation(cx))
            .child(self.render_visual_table_header(cx))
            .child(
                div()
                    .id("visual-step-scroll-area")
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .child(
                        v_flex()
                            .id("visual-step-list")
                            .size_full()
                            .overflow_y_scrollbar()
                            .children(
                                sequence.steps.iter().enumerate().map(|(index, step)| {
                                    self.render_visual_step_row(index, step, cx)
                                }),
                            )
                            .child(self.render_visual_step_end_drop_zone(cx)),
                    ),
            )
            .into_any_element()
    }

    fn render_visual_validation(&self, cx: &Context<Self>) -> impl IntoElement {
        let blueprints = self
            .visual_sequences
            .iter()
            .map(|sequence| sequence.to_blueprint(cx))
            .collect::<Vec<_>>();
        let mut errors = visual_sequence_errors(&blueprints);
        errors.extend(self.visual_safety_errors(&blueprints));

        v_flex()
            .flex_none()
            .px_3()
            .py_2()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().border)
            .when(errors.is_empty(), |this| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().success)
                        .child("Valid blocks; ready to generate rseq."),
                )
            })
            .when(!errors.is_empty(), |this| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().red)
                        .child(format!("{} issue(s)", errors.len())),
                )
                .children(errors.iter().take(4).map(|err| {
                    div()
                        .text_xs()
                        .whitespace_normal()
                        .text_color(cx.theme().muted_foreground)
                        .child(err.clone())
                }))
            })
    }

    fn render_visual_table_header(&self, cx: &Context<Self>) -> impl IntoElement {
        h_flex()
            .flex_none()
            .h(px(30.))
            .px_3()
            .gap_2()
            .items_center()
            .border_b_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().tab_bar)
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(div().w(px(32.)).flex_none())
            .child(div().w(px(42.)).flex_none().child("#"))
            .child(div().w(px(64.)).flex_none().child("Kind"))
            .child(div().w(px(150.)).flex_none().child("Address"))
            .child(div().w(px(86.)).flex_none().child("Length"))
            .child(div().flex_1().min_w(px(180.)).child("Data / fields"))
            .child(div().w(px(100.)).flex_none().child("Delay us"))
            .child(div().w(px(104.)).flex_none().child("Actions"))
    }

    fn render_visual_step_row(
        &self,
        index: usize,
        step: &VisualStepEditor,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let is_read = step.kind == VisualStepKind::Read;
        let entity_id = cx.entity_id();
        let sequence_index = self.active_visual_sequence;
        let drag = DragVisualStep {
            entity_id,
            sequence_index,
            step_index: index,
            label: format!("#{:02} {}", index + 1, step.kind.label()),
        };

        h_flex()
            .id(("visual-step", index))
            .w_full()
            .min_w(px(800.))
            .h(px(38.))
            .flex_none()
            .px_3()
            .gap_2()
            .items_center()
            .border_t_2()
            .border_b_1()
            .border_color(cx.theme().border.opacity(0.45))
            .hover(|this| this.bg(cx.theme().muted.opacity(0.35)))
            .can_drop(move |dragged, _, _| {
                dragged
                    .downcast_ref::<DragVisualStep>()
                    .is_some_and(|drag| {
                        drag.entity_id == entity_id && drag.sequence_index == sequence_index
                    })
            })
            .drag_over::<DragVisualStep>(move |this, drag, _, cx| {
                if drag.entity_id == entity_id && drag.sequence_index == sequence_index {
                    this.border_color(cx.theme().drag_border)
                        .bg(cx.theme().drop_target)
                } else {
                    this
                }
            })
            .on_drop(
                cx.listener(move |this, drag: &DragVisualStep, _window, cx| {
                    this.drop_visual_step_before(drag, index, cx)
                }),
            )
            .child(self.render_visual_step_drag_handle(drag, cx))
            .child(
                div()
                    .w(px(42.))
                    .flex_none()
                    .font_family("monospace")
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!("{:02}", index + 1)),
            )
            .child(
                div()
                    .w(px(64.))
                    .flex_none()
                    .text_xs()
                    .font_semibold()
                    .text_color(if is_read {
                        cx.theme().blue
                    } else {
                        cx.theme().success
                    })
                    .child(step.kind.label()),
            )
            .child(
                div()
                    .w(px(150.))
                    .flex_none()
                    .child(Input::new(&step.address_input).xsmall()),
            )
            .child(div().w(px(86.)).flex_none().child(if is_read {
                Input::new(&step.read_len_input).xsmall().into_any_element()
            } else {
                div()
                    .h_6()
                    .flex()
                    .items_center()
                    .font_family("monospace")
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(
                        parse_register_write_bytes(&step.data_input.read(cx).value())
                            .map(|bytes| format!("{} B", bytes.len()))
                            .unwrap_or_else(|_| "--".to_string()),
                    )
                    .into_any_element()
            }))
            .child(
                div()
                    .flex_1()
                    .min_w(px(180.))
                    .child(Input::new(&step.data_input).xsmall().disabled(is_read)),
            )
            .child(
                div()
                    .w(px(100.))
                    .flex_none()
                    .child(Input::new(&step.delay_us_input).xsmall()),
            )
            .child(
                h_flex()
                    .w(px(104.))
                    .flex_none()
                    .gap_1()
                    .child(
                        Button::new(format!("visual-dup-{index}"))
                            .xsmall()
                            .label("Dup")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.duplicate_visual_step(index, window, cx);
                            })),
                    )
                    .child(
                        Button::new(format!("visual-del-{index}"))
                            .xsmall()
                            .danger()
                            .label("Del")
                            .on_click(cx.listener(move |this, _, _window, cx| {
                                this.delete_visual_step(index, cx);
                            })),
                    ),
            )
    }

    fn render_visual_step_drag_handle(
        &self,
        drag: DragVisualStep,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        div()
            .id(format!("visual-step-drag-handle-{}", drag.step_index))
            .w(px(28.))
            .h_6()
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().secondary)
            .cursor_grab()
            .text_color(cx.theme().muted_foreground)
            .hover(|this| this.bg(cx.theme().muted))
            .tooltip(|window, cx| Tooltip::new("Drag to reorder").build(window, cx))
            .child(Icon::new(IconName::Menu).xsmall())
            .on_drag(drag, |drag, _, _, cx| {
                cx.stop_propagation();
                cx.new(|_| drag.clone())
            })
    }

    fn render_visual_step_end_drop_zone(&self, cx: &Context<Self>) -> impl IntoElement {
        let entity_id = cx.entity_id();
        let sequence_index = self.active_visual_sequence;

        div()
            .id("visual-step-end-drop")
            .h(px(10.))
            .w_full()
            .flex_none()
            .border_t_2()
            .border_color(cx.theme().transparent)
            .can_drop(move |dragged, _, _| {
                dragged
                    .downcast_ref::<DragVisualStep>()
                    .is_some_and(|drag| {
                        drag.entity_id == entity_id && drag.sequence_index == sequence_index
                    })
            })
            .drag_over::<DragVisualStep>(move |this, drag, _, cx| {
                if drag.entity_id == entity_id && drag.sequence_index == sequence_index {
                    this.border_color(cx.theme().drag_border)
                        .bg(cx.theme().drop_target)
                } else {
                    this
                }
            })
            .on_drop(
                cx.listener(move |this, drag: &DragVisualStep, _window, cx| {
                    this.drop_visual_step_at_end(drag, cx)
                }),
            )
    }

    fn render_sequence_sidebar(&self, cx: &Context<Self>) -> impl IntoElement {
        let path = self
            .sequence_path
            .as_deref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "untitled.rseq".to_string());
        let dirty = self.sequence_dirty;
        let source_ready = self.current_view_has_source(cx);
        let chips = self.chip_paths_label();

        v_flex()
            .w(px(286.))
            .h_full()
            .flex_none()
            .min_h_0()
            .border_r_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().secondary)
            .child(
                v_flex()
                    .flex_none()
                    .p_3()
                    .gap_2()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        h_flex()
                            .gap_2()
                            .items_baseline()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_sm()
                                    .font_semibold()
                                    .truncate()
                                    .child(self.sequence_title()),
                            )
                            .when(dirty, |this| {
                                this.child(
                                    div()
                                        .flex_none()
                                        .text_xs()
                                        .font_semibold()
                                        .text_color(cx.theme().warning)
                                        .child("Unsaved"),
                                )
                            }),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .flex_wrap()
                            .child(Button::new("sequence-new").xsmall().label("New").on_click(
                                cx.listener(|this, _, window, cx| {
                                    this.new_sequence(window, cx);
                                }),
                            ))
                            .child(
                                Button::new("sequence-open")
                                    .xsmall()
                                    .icon(IconName::FolderOpen)
                                    .label("Open")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.open_sequence_file(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("sequence-save")
                                    .xsmall()
                                    .primary()
                                    .label("Save")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.save_sequence_file(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("sequence-save-as")
                                    .xsmall()
                                    .label("Save As")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.save_sequence_file_as(window, cx);
                                    })),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .flex_wrap()
                            .child(
                                Button::new("sequence-compile")
                                    .xsmall()
                                    .icon(IconName::Check)
                                    .label("Compile")
                                    .disabled(!source_ready)
                                    .on_click(cx.listener(|this, _, _window, cx| {
                                        this.reload_workspace_from_sequence_editor(true, cx);
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("sequence-load-run")
                                    .xsmall()
                                    .primary()
                                    .icon(IconName::Play)
                                    .label("Load & Run")
                                    .disabled(!source_ready)
                                    .on_click(cx.listener(|this, _, _window, cx| {
                                        this.load_and_run_sequence_editor(cx);
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("sequence-watch")
                                    .xsmall()
                                    .label("Watch")
                                    .disabled(!source_ready)
                                    .on_click(cx.listener(|this, _, _window, cx| {
                                        if this.reload_workspace_from_sequence_editor(false, cx) {
                                            this.start_session(true);
                                        }
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            .child(
                v_flex()
                    .flex_none()
                    .p_3()
                    .gap_2()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        div()
                            .text_xs()
                            .font_semibold()
                            .text_color(cx.theme().muted_foreground)
                            .child("Chip Metadata"),
                    )
                    .child(path_picker_row(
                        "Chip",
                        &self.chip_path_input,
                        "sequence-open-chip-file",
                        "Open",
                        cx.listener(|this, _, window, cx| this.open_chip_file(window, cx)),
                        cx,
                    ))
                    .child(
                        h_flex().justify_end().child(
                            Button::new("sequence-apply-chip")
                                .xsmall()
                                .icon(IconName::Check)
                                .label("Apply")
                                .on_click(cx.listener(|this, _, _window, cx| {
                                    this.reload_workspace_from_sequence_editor(true, cx);
                                    cx.notify();
                                })),
                        ),
                    ),
            )
            .child(
                div()
                    .id("sequence-sidebar-info-scroll-area")
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .child(
                        v_flex()
                            .id("sequence-sidebar-info-list")
                            .size_full()
                            .overflow_y_scrollbar()
                            .p_3()
                            .gap_3()
                            .child(sequence_info_block("Source", path, cx))
                            .child(sequence_info_block("Chip", chips, cx))
                            .child(sequence_info_block(
                                "Compile",
                                self.sequence_status.clone(),
                                cx,
                            ))
                            .child(sequence_info_block(
                                "Program",
                                self.startup_program
                                    .as_ref()
                                    .map(|program| {
                                        format!(
                                            "main={} bytes\nirq_handlers={}",
                                            program.main.len(),
                                            program.irq_bytecodes.len()
                                        )
                                    })
                                    .unwrap_or_else(|| "not compiled".to_string()),
                                cx,
                            ))
                            .child(sequence_info_block(
                                "Metadata",
                                format!(
                                    "reports={}\nregisters={}",
                                    self.metadata.report_decoders.len(),
                                    self.metadata.register_catalog.registers().len()
                                ),
                                cx,
                            )),
                    ),
            )
    }

    fn sequence_title(&self) -> String {
        self.sequence_path
            .as_deref()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .map(ToString::to_string)
            .unwrap_or_else(|| "untitled.rseq".to_string())
    }

    fn chip_paths_label(&self) -> String {
        if self.cli.chip.is_empty() {
            "from chip!(...) or none".to_string()
        } else {
            self.cli
                .chip
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join("\n")
        }
    }

    fn set_sequence_view_mode(&mut self, mode: SequenceViewMode, cx: &mut Context<Self>) {
        self.sequence_view_mode = mode;
        cx.notify();
    }

    fn set_active_visual_sequence(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.visual_sequences.len() {
            self.active_visual_sequence = index;
        }
        cx.notify();
    }

    fn add_visual_sequence(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = format!("Sequence {}", self.visual_sequences.len() + 1);
        self.visual_sequences
            .push(VisualSequence::new(name, window, cx));
        self.active_visual_sequence = self.visual_sequences.len().saturating_sub(1);
        self.sequence_dirty = true;
        self.sequence_status = "visual sequence added".to_string();
        cx.notify();
    }

    fn delete_active_visual_sequence(&mut self, cx: &mut Context<Self>) {
        if self.visual_sequences.len() <= 1 {
            if let Some(sequence) = self.visual_sequences.first_mut() {
                sequence.steps.clear();
            }
            self.active_visual_sequence = 0;
        } else if self.active_visual_sequence < self.visual_sequences.len() {
            self.visual_sequences.remove(self.active_visual_sequence);
            self.active_visual_sequence = self
                .active_visual_sequence
                .min(self.visual_sequences.len().saturating_sub(1));
        }
        self.sequence_dirty = true;
        self.sequence_status = "visual sequence deleted".to_string();
        cx.notify();
    }

    fn add_visual_step(
        &mut self,
        kind: VisualStepKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.visual_sequences.is_empty() {
            self.visual_sequences
                .push(VisualSequence::new("Sequence 1", window, cx));
            self.active_visual_sequence = 0;
        }
        let step = VisualStepEditor::default_kind(kind, window, cx);
        if let Some(sequence) = self.visual_sequences.get_mut(self.active_visual_sequence) {
            sequence.steps.push(step);
        }
        self.sequence_dirty = true;
        self.sequence_status = format!("{} step added", kind.label());
        cx.notify();
    }

    fn duplicate_visual_step(
        &mut self,
        step_index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(sequence) = self.visual_sequences.get_mut(self.active_visual_sequence) else {
            return;
        };
        let Some(step) = sequence.steps.get(step_index) else {
            return;
        };
        let duplicate = step.duplicate(window, cx);
        sequence.steps.insert(step_index + 1, duplicate);
        self.sequence_dirty = true;
        self.sequence_status = "visual step duplicated".to_string();
        cx.notify();
    }

    fn delete_visual_step(&mut self, step_index: usize, cx: &mut Context<Self>) {
        let Some(sequence) = self.visual_sequences.get_mut(self.active_visual_sequence) else {
            return;
        };
        if step_index < sequence.steps.len() {
            sequence.steps.remove(step_index);
            self.sequence_dirty = true;
            self.sequence_status = "visual step deleted".to_string();
        }
        cx.notify();
    }

    fn drop_visual_sequence_before(
        &mut self,
        drag: &DragVisualSequence,
        target_index: usize,
        cx: &mut Context<Self>,
    ) {
        self.drop_visual_sequence_at_index(drag, target_index, cx);
    }

    fn drop_visual_sequence_at_end(&mut self, drag: &DragVisualSequence, cx: &mut Context<Self>) {
        self.drop_visual_sequence_at_index(drag, self.visual_sequences.len(), cx);
    }

    fn drop_visual_sequence_at_index(
        &mut self,
        drag: &DragVisualSequence,
        target_index: usize,
        cx: &mut Context<Self>,
    ) {
        if drag.entity_id != cx.entity_id() {
            return;
        }

        let source_index = drag.sequence_index;
        let len = self.visual_sequences.len();
        if source_index >= len || target_index > len {
            return;
        }
        if source_index == target_index || source_index + 1 == target_index {
            self.active_visual_sequence = source_index;
            cx.notify();
            return;
        }

        let old_active = self.active_visual_sequence;
        let sequence = self.visual_sequences.remove(source_index);
        let insert_index = if source_index < target_index {
            target_index.saturating_sub(1)
        } else {
            target_index
        };
        self.visual_sequences.insert(insert_index, sequence);

        self.active_visual_sequence = if old_active == source_index {
            insert_index
        } else {
            let mut active = old_active;
            if active > source_index {
                active = active.saturating_sub(1);
            }
            if active >= insert_index {
                active += 1;
            }
            active.min(self.visual_sequences.len().saturating_sub(1))
        };
        self.sequence_dirty = true;
        self.sequence_status = "visual sequence reordered".to_string();
        cx.notify();
    }

    fn drop_visual_step_before(
        &mut self,
        drag: &DragVisualStep,
        target_index: usize,
        cx: &mut Context<Self>,
    ) {
        self.drop_visual_step_at_index(drag, target_index, cx);
    }

    fn drop_visual_step_at_end(&mut self, drag: &DragVisualStep, cx: &mut Context<Self>) {
        let Some(sequence) = self.visual_sequences.get(self.active_visual_sequence) else {
            return;
        };
        self.drop_visual_step_at_index(drag, sequence.steps.len(), cx);
    }

    fn drop_visual_step_at_index(
        &mut self,
        drag: &DragVisualStep,
        target_index: usize,
        cx: &mut Context<Self>,
    ) {
        if drag.entity_id != cx.entity_id() || drag.sequence_index != self.active_visual_sequence {
            return;
        }

        let Some(sequence) = self.visual_sequences.get_mut(self.active_visual_sequence) else {
            return;
        };
        let source_index = drag.step_index;
        let len = sequence.steps.len();
        if source_index >= len || target_index > len {
            return;
        }
        if source_index == target_index || source_index + 1 == target_index {
            return;
        }

        let step = sequence.steps.remove(source_index);
        let insert_index = if source_index < target_index {
            target_index.saturating_sub(1)
        } else {
            target_index
        };
        sequence.steps.insert(insert_index, step);
        self.sequence_dirty = true;
        self.sequence_status = "visual step reordered".to_string();
        cx.notify();
    }

    fn apply_visual_to_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.visual_source_all(cx) {
            Ok(source) => {
                self.sequence_editor.update(cx, |input, cx| {
                    input.set_value(source, window, cx);
                });
                self.sequence_view_mode = SequenceViewMode::Text;
                self.sequence_dirty = true;
                self.sequence_status = "visual sequence applied to text".to_string();
            }
            Err(errors) => {
                self.sequence_status = errors.join("\n");
            }
        }
        cx.notify();
    }

    fn load_and_run_active_visual_sequence(&mut self, cx: &Context<Self>) {
        match self.visual_source_for_active(cx) {
            Ok(source) => {
                let loaded =
                    load_workspace_from_sources(&[source], &self.chip_files_from_input(cx), true);
                if !self.apply_loaded_workspace(loaded, "visual sequence") {
                    self.sequence_status = self.compile_status.clone();
                    return;
                }
                self.sequence_status = self.compile_status.clone();
                self.start_session(false);
            }
            Err(errors) => {
                self.sequence_status = errors.join("\n");
                push_bounded(
                    &mut self.logs,
                    format!("active visual sequence failed: {}", self.sequence_status),
                    MAX_TEXT_LINES,
                );
            }
        }
    }

    fn visual_source_for_active(&self, cx: &Context<Self>) -> Result<RseqSource, Vec<String>> {
        let Some(sequence) = self.visual_sequences.get(self.active_visual_sequence) else {
            return Err(vec!["no active visual sequence".to_string()]);
        };
        let blueprint = sequence.to_blueprint(cx);
        let safety_errors = self.visual_safety_errors(std::slice::from_ref(&blueprint));
        if !safety_errors.is_empty() {
            return Err(safety_errors);
        }
        let source = visual_source_from_blueprints(&[blueprint], &self.chip_files_from_input(cx))?;
        Ok(RseqSource::new(
            "visual-active-sequence.rseq",
            source,
            self.visual_source_base_dir(),
        ))
    }

    fn visual_source_all(&self, cx: &Context<Self>) -> Result<String, Vec<String>> {
        let blueprints = self
            .visual_sequences
            .iter()
            .map(|sequence| sequence.to_blueprint(cx))
            .collect::<Vec<_>>();
        let mut errors = visual_sequence_errors(&blueprints);
        errors.extend(self.visual_safety_errors(&blueprints));
        if !errors.is_empty() {
            return Err(errors);
        }
        visual_source_from_blueprints(&blueprints, &self.chip_files_from_input(cx))
    }

    fn visual_source_base_dir(&self) -> Option<PathBuf> {
        self.sequence_path
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .or_else(|| std::env::current_dir().ok())
    }

    fn visual_safety_errors(&self, sequences: &[VisualSequenceBlueprint]) -> Vec<String> {
        let mut errors = Vec::new();
        for sequence in sequences {
            for (step_index, step) in sequence.steps.iter().enumerate() {
                let regs = self.register_infos_for_visual_address(&step.address);
                if regs.is_empty() {
                    continue;
                }
                let label = format!("{} step {}", sequence.name, step_index + 1);
                match step.kind {
                    VisualStepKind::Read => {
                        if regs.iter().all(|reg| !register_is_readable(&reg.access)) {
                            errors.push(format!(
                                "{label}: {} is not readable according to chip metadata",
                                step.address.trim()
                            ));
                        }
                    }
                    VisualStepKind::Write => {
                        if regs.iter().all(|reg| !register_is_writable(&reg.access)) {
                            errors.push(format!(
                                "{label}: {} is not writable according to chip metadata",
                                step.address.trim()
                            ));
                        }
                    }
                }
            }
        }
        errors
    }

    fn register_infos_for_visual_address(&self, address: &str) -> Vec<RegisterInfo> {
        let address = address.trim();
        if address.is_empty() {
            return Vec::new();
        }
        if let Ok(addr) = parse_visual_number_u32(address) {
            return self
                .metadata
                .register_catalog
                .registers_for_addr(addr)
                .into_iter()
                .cloned()
                .collect();
        }

        let (page, name) = address
            .split_once('.')
            .map(|(page, name)| (Some(page.trim()), name.trim()))
            .unwrap_or((None, address));
        self.metadata
            .register_catalog
            .registers()
            .iter()
            .filter(|reg| {
                reg.name.eq_ignore_ascii_case(name)
                    && page.is_none_or(|page| reg.page.eq_ignore_ascii_case(page))
            })
            .cloned()
            .collect()
    }

    fn sequence_editor_stats(&self, cx: &Context<Self>) -> String {
        let source = self.sequence_editor.read(cx).value();
        let text = source.as_ref();
        let lines = text.lines().count().max(1);
        format!("{lines} lines, {} bytes", text.len())
    }

    fn visual_sequence_stats(&self, cx: &Context<Self>) -> String {
        let sequence_count = self.visual_sequences.len();
        let step_count = self
            .visual_sequences
            .iter()
            .map(|sequence| sequence.steps.len())
            .sum::<usize>();
        let active = self
            .visual_sequences
            .get(self.active_visual_sequence)
            .map(|sequence| sequence.name(cx))
            .unwrap_or_else(|| "none".to_string());
        format!("{sequence_count} sequences, {step_count} steps, active {active}")
    }

    fn render_registers(&self, cx: &Context<Self>) -> AnyElement {
        v_flex()
            .size_full()
            .min_h_0()
            .overflow_hidden()
            .p_3()
            .gap_3()
            .child(self.render_register_toolbar(cx))
            .child(match self.register_view_mode {
                RegisterViewMode::Dump => self.render_register_dump_grid(cx),
                RegisterViewMode::Map => self.render_register_map_view(cx),
            })
            .child(self.render_register_detail(cx))
            .into_any_element()
    }

    fn render_register_dump_grid(&self, cx: &Context<Self>) -> AnyElement {
        let max_addr = self.register_grid_max_addr().max(0x0f);
        let max_base = max_addr & !0x0f;
        let rows = max_base / 16 + 1;

        div()
            .id("register-matrix-scroll-area")
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .child(
                v_flex()
                    .id("register-matrix-grid")
                    .size_full()
                    .overflow_y_scrollbar()
                    .child(
                        v_flex()
                            .flex_none()
                            .gap_1()
                            .min_w(px(640.))
                            .child(self.render_register_header(cx))
                            .children((0..rows).map(|row| {
                                let base = row * 16;
                                h_flex()
                                    .flex_none()
                                    .gap_1()
                                    .child(
                                        div()
                                            .w(px(52.))
                                            .h(px(28.))
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .font_family("monospace")
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(format!("0x{base:02x}")),
                                    )
                                    .children((0..16).map(move |offset| {
                                        self.render_register_cell(base + offset, cx)
                                    }))
                            })),
                    ),
            )
            .into_any_element()
    }

    fn render_register_header(&self, cx: &Context<Self>) -> impl IntoElement {
        h_flex()
            .flex_none()
            .gap_1()
            .child(
                div()
                    .w(px(52.))
                    .h(px(24.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("base"),
            )
            .children((0..16).map(|offset| {
                div()
                    .w(px(34.))
                    .h(px(24.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .font_family("monospace")
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!("{offset:02x}"))
            }))
    }

    fn render_register_cell(&self, addr: u32, cx: &Context<Self>) -> AnyElement {
        let selected = self.selected_register_addr == addr;
        let no_dump = self.register_is_no_dump(addr);
        let regs = self.register_infos_for_addr(addr);
        let width = register_display_width(&regs, addr);
        let bytes = self.register_bytes(addr, width);
        let has_value = self.registers.contains_key(&addr);
        let active_fields = register_has_active_fields(&regs, &bytes);
        let label = if no_dump {
            "??".to_string()
        } else {
            bytes
                .first()
                .map(|byte| format!("{byte:02x}"))
                .unwrap_or_else(|| {
                    if regs.is_empty() {
                        "··".to_string()
                    } else {
                        "--".to_string()
                    }
                })
        };

        if self.inline_write_addr == Some(addr) {
            return div()
                .id(("reg-edit", addr as usize))
                .w(px(34.))
                .h(px(28.))
                .child(Input::new(&self.inline_write_input).xsmall().w_full())
                .into_any_element();
        }

        let tooltip_regs = regs.clone();
        let tooltip_bytes = bytes.clone();
        div()
            .id(("reg-cell", addr as usize))
            .w(px(34.))
            .h(px(28.))
            .flex()
            .items_center()
            .justify_center()
            .border_1()
            .border_color(if selected {
                cx.theme().primary
            } else {
                cx.theme().border
            })
            .rounded(px(3.))
            .font_family("monospace")
            .text_xs()
            .cursor_pointer()
            .bg(if selected {
                cx.theme().accent
            } else if no_dump {
                cx.theme().warning.opacity(0.12)
            } else if active_fields {
                cx.theme().success.opacity(0.14)
            } else if has_value {
                cx.theme().secondary
            } else if regs.is_empty() {
                cx.theme().muted.opacity(0.25)
            } else {
                cx.theme().background
            })
            .text_color(if no_dump {
                cx.theme().warning
            } else if has_value {
                cx.theme().foreground
            } else {
                cx.theme().muted_foreground
            })
            .hover(|this| this.border_color(cx.theme().primary))
            .child(label)
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                if event.click_count() == 2 {
                    this.begin_inline_register_write(addr, window, cx);
                } else {
                    this.selected_register_addr = addr;
                }
                cx.notify();
            }))
            .tooltip(move |window, cx| {
                let regs = tooltip_regs.clone();
                let bytes = tooltip_bytes.clone();
                Tooltip::element(move |_window, app| {
                    register_tooltip_view(addr, regs.clone(), bytes.clone(), no_dump, app)
                })
                .build(window, cx)
            })
            .into_any_element()
    }

    fn render_register_map_view(&self, cx: &Context<Self>) -> AnyElement {
        let mut registers = self.active_register_page_registers();
        registers.sort_by(|a, b| {
            a.page
                .cmp(&b.page)
                .then(a.addr.cmp(&b.addr))
                .then(a.name.cmp(&b.name))
        });

        if registers.is_empty() {
            return div()
                .flex_1()
                .min_h_0()
                .border_1()
                .border_color(cx.theme().border)
                .rounded(cx.theme().radius)
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child("no register metadata loaded")
                .into_any_element();
        }

        div()
            .id("register-map-scroll-area")
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .child(
                v_flex()
                    .id("register-map-list")
                    .size_full()
                    .overflow_y_scrollbar()
                    .min_w(px(900.))
                    .border_1()
                    .border_color(cx.theme().border)
                    .rounded(cx.theme().radius)
                    .child(self.render_register_map_header(cx))
                    .children(
                        registers
                            .iter()
                            .enumerate()
                            .map(|(row_ix, reg)| self.render_register_map_row(row_ix, reg, cx)),
                    ),
            )
            .into_any_element()
    }

    fn render_register_map_header(&self, cx: &Context<Self>) -> impl IntoElement {
        h_flex()
            .flex_none()
            .w_full()
            .px_3()
            .py_2()
            .gap_3()
            .bg(cx.theme().secondary)
            .border_b_1()
            .border_color(cx.theme().border)
            .text_xs()
            .font_semibold()
            .text_color(cx.theme().muted_foreground)
            .child(div().w(px(86.)).flex_none().child("Page"))
            .child(div().w(px(220.)).flex_none().child("Register"))
            .child(div().w(px(72.)).flex_none().child("Addr"))
            .child(div().w(px(118.)).flex_none().child("Value"))
            .child(div().flex_1().min_w(px(300.)).child("Bit fields"))
    }

    fn render_register_map_row(
        &self,
        row_ix: usize,
        reg: &RegisterInfo,
        cx: &Context<Self>,
    ) -> AnyElement {
        let addr = reg.addr;
        let selected = self.selected_register_addr == addr;
        let width = reg.width.max(1) as usize;
        let bytes = self.register_bytes(addr, width);
        let value_text = register_value_text(&bytes, width, reg.no_dump);
        let tooltip_reg = reg.clone();
        let tooltip_bytes = bytes.clone();

        h_flex()
            .id(("register-map-row", row_ix))
            .w_full()
            .flex_none()
            .items_start()
            .px_3()
            .py_2()
            .gap_3()
            .border_b_1()
            .border_color(cx.theme().border)
            .cursor_pointer()
            .when(selected, |this| this.bg(cx.theme().accent))
            .hover(|this| this.bg(cx.theme().muted.opacity(0.45)))
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                if event.click_count() == 2 {
                    this.begin_inline_register_write(addr, window, cx);
                } else {
                    this.selected_register_addr = addr;
                }
                cx.notify();
            }))
            .tooltip(move |window, cx| {
                let reg = tooltip_reg.clone();
                let bytes = tooltip_bytes.clone();
                Tooltip::element(move |_window, app| {
                    register_tooltip_view(
                        reg.addr,
                        vec![reg.clone()],
                        bytes.clone(),
                        reg.no_dump,
                        app,
                    )
                })
                .build(window, cx)
            })
            .child(
                div()
                    .w(px(86.))
                    .flex_none()
                    .text_xs()
                    .font_semibold()
                    .text_color(cx.theme().muted_foreground)
                    .child(reg.page.clone()),
            )
            .child(
                v_flex()
                    .w(px(220.))
                    .flex_none()
                    .gap_1()
                    .child(
                        div()
                            .text_sm()
                            .font_semibold()
                            .whitespace_normal()
                            .text_color(cx.theme().foreground)
                            .child(reg.name.clone()),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                div()
                                    .px_1p5()
                                    .rounded(cx.theme().radius)
                                    .bg(cx.theme().muted)
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(reg.access.clone()),
                            )
                            .child(
                                div()
                                    .px_1p5()
                                    .rounded(cx.theme().radius)
                                    .bg(cx.theme().muted)
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(format!("{}B", reg.width.max(1))),
                            ),
                    )
                    .when(!reg.desc.is_empty(), |this| {
                        this.child(
                            div()
                                .text_xs()
                                .whitespace_normal()
                                .text_color(cx.theme().muted_foreground)
                                .child(reg.desc.clone()),
                        )
                    }),
            )
            .child(
                div()
                    .w(px(72.))
                    .flex_none()
                    .font_family("monospace")
                    .text_sm()
                    .text_color(cx.theme().foreground)
                    .child(format!("0x{addr:02x}")),
            )
            .child(
                div()
                    .w(px(118.))
                    .flex_none()
                    .font_family("monospace")
                    .text_xs()
                    .text_color(if bytes.is_empty() || reg.no_dump {
                        cx.theme().muted_foreground
                    } else {
                        cx.theme().foreground
                    })
                    .child(if self.inline_write_addr == Some(addr) {
                        Input::new(&self.inline_write_input)
                            .xsmall()
                            .w_full()
                            .into_any_element()
                    } else {
                        value_text.into_any_element()
                    }),
            )
            .child(self.render_register_field_cell(row_ix, reg, &bytes, cx))
            .into_any_element()
    }

    fn render_register_field_cell(
        &self,
        row_ix: usize,
        reg: &RegisterInfo,
        bytes: &[u8],
        cx: &Context<Self>,
    ) -> AnyElement {
        if reg.fields.is_empty() {
            return div()
                .flex_1()
                .min_w(px(300.))
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child("--")
                .into_any_element();
        }

        v_flex()
            .flex_1()
            .min_w(px(300.))
            .gap_1()
            .children(reg.fields.iter().enumerate().map(|(field_ix, field)| {
                let value = field_value(bytes, field.bit_hi, field.bit_lo);
                let active = value.is_some_and(|value| value != 0);
                let tooltip_field = field.clone();
                let tooltip_bytes = bytes.to_vec();

                h_flex()
                    .id(("reg-field", row_ix * 256 + field_ix))
                    .w_full()
                    .items_start()
                    .gap_2()
                    .px_1()
                    .py_0p5()
                    .rounded(px(3.))
                    .when(active, |this| this.bg(cx.theme().success.opacity(0.12)))
                    .tooltip(move |window, cx| {
                        let field = tooltip_field.clone();
                        let bytes = tooltip_bytes.clone();
                        Tooltip::element(move |_window, app| {
                            field_tooltip_view(&field, &bytes, app)
                        })
                        .build(window, cx)
                    })
                    .child(
                        div()
                            .w(px(54.))
                            .flex_none()
                            .font_family("monospace")
                            .text_xs()
                            .text_color(cx.theme().blue)
                            .child(field_bit_label(field)),
                    )
                    .child(
                        div()
                            .w(px(58.))
                            .flex_none()
                            .font_family("monospace")
                            .text_xs()
                            .text_color(if active {
                                cx.theme().success
                            } else {
                                cx.theme().muted_foreground
                            })
                            .child(field_value_text(field, bytes)),
                    )
                    .child(
                        div()
                            .w(px(130.))
                            .flex_none()
                            .text_xs()
                            .font_semibold()
                            .whitespace_normal()
                            .text_color(cx.theme().foreground)
                            .child(field.name.clone()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_xs()
                            .whitespace_normal()
                            .text_color(cx.theme().muted_foreground)
                            .child(field.desc.clone()),
                    )
            }))
            .into_any_element()
    }

    fn render_register_toolbar(&self, cx: &Context<Self>) -> impl IntoElement {
        let pages = self.metadata.register_catalog.pages();
        let active_page = self.active_register_page.clone();

        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("Page"),
                    )
                    .when(pages.is_empty(), |this| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child("no register metadata"),
                        )
                    })
                    .children(pages.into_iter().map(move |page| {
                        let selected = active_page.as_ref().is_some_and(|active| active == &page);
                        let label = page.clone();
                        Button::new(format!("reg-page-{label}"))
                            .xsmall()
                            .label(label.clone())
                            .selected(selected)
                            .on_click(cx.listener(move |this, _, _window, cx| {
                                this.set_active_register_page(page.clone(), cx);
                            }))
                    }))
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(match self.register_view_mode {
                                RegisterViewMode::Dump => {
                                    "hover for decoded fields; double click a cell to edit"
                                }
                                RegisterViewMode::Map => {
                                    "rows are YAML registers; active bit fields are highlighted"
                                }
                            }),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .justify_between()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                div().text_sm().font_semibold().child(format!(
                                    "selected 0x{:02x}",
                                    self.selected_register_addr
                                )),
                            )
                            .children(RegisterViewMode::ALL.into_iter().map(|mode| {
                                Button::new(format!("reg-view-{}", mode.label()))
                                    .small()
                                    .label(mode.label())
                                    .selected(self.register_view_mode == mode)
                                    .on_click(cx.listener(move |this, _, _window, cx| {
                                        this.register_view_mode = mode;
                                        cx.notify();
                                    }))
                            }))
                            .child(
                                Button::new("reg-read")
                                    .small()
                                    .label("Dump Selected")
                                    .disabled(!self.connected)
                                    .on_click(cx.listener(|this, _, _window, cx| {
                                        this.request_selected_register_dump();
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("reg-read-page")
                                    .small()
                                    .label("Dump Page")
                                    .disabled(!self.connected)
                                    .on_click(cx.listener(|this, _, _window, cx| {
                                        this.request_active_register_page_dump();
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(Input::new(&self.write_input).small().w(px(180.)))
                            .child(
                                Button::new("reg-write")
                                    .small()
                                    .label("Write")
                                    .disabled(!self.connected)
                                    .on_click(cx.listener(|this, _, _window, cx| {
                                        this.write_selected_register(cx);
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
    }

    fn render_register_detail(&self, cx: &Context<Self>) -> impl IntoElement {
        let addr = self.selected_register_addr;
        let regs = self.register_infos_for_addr(addr);
        let stored = self.registers.get(&addr);
        let value = if self.register_is_no_dump(addr) {
            "??".to_string()
        } else {
            stored
                .map(|value| hex_bytes(&value.data))
                .unwrap_or_else(|| "--".to_string())
        };
        let last_access = stored
            .map(|value| match value.access {
                AccessKind::Read => "read",
                AccessKind::Write => "write",
            })
            .unwrap_or("-");
        let mut lines = vec![format!(
            "addr 0x{addr:02x} value {value} last={last_access}"
        )];
        if regs.is_empty() {
            lines.push("no register metadata".to_string());
        } else {
            for reg in &regs {
                lines.push(format!(
                    "{}.{} access={} width={} {}",
                    reg.page, reg.name, reg.access, reg.width, reg.desc
                ));
                if reg.no_dump {
                    lines.push(format!("no_dump: {}", reg.no_dump_reason));
                }
                for field in &reg.fields {
                    let bits = if field.bit_hi == field.bit_lo {
                        field.bit_lo.to_string()
                    } else {
                        format!("{}:{}", field.bit_hi, field.bit_lo)
                    };
                    let event = field
                        .event
                        .as_ref()
                        .map(|event| format!(" event={event}"))
                        .unwrap_or_default();
                    lines.push(format!("[{bits}] {}{} {}", field.name, event, field.desc));
                }
            }
        }
        info_block("Register Detail", lines.join("\n"), cx)
    }

    fn register_grid_max_addr(&self) -> u32 {
        let page_max = self
            .active_register_page_registers()
            .into_iter()
            .map(|reg| reg.addr + reg.width.max(1) - 1)
            .max();

        self.registers
            .keys()
            .next_back()
            .copied()
            .into_iter()
            .chain(page_max)
            .max()
            .unwrap_or(0x0f)
    }

    fn render_status_bar(&self, cx: &Context<Self>) -> impl IntoElement {
        h_flex()
            .gap_4()
            .px_3()
            .py_1()
            .border_t_1()
            .border_color(cx.theme().border)
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(format!("samples {}", self.samples.len()))
            .child(format!("reports {}", self.health.total_reports))
            .child(format!("dropped {}", self.health.dropped_frames))
            .child(format!("regs {}", self.registers.len()))
            .child(format!("mode {}", self.session_mode))
    }
}

impl Render for RseqGpui {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match self.selected_tab {
            PanelTab::Motion => self.render_motion(cx),
            PanelTab::Reports => self.render_reports(cx),
            PanelTab::Registers => self.render_registers(cx),
            PanelTab::Sequences => self.render_sequences(cx),
            PanelTab::Logs => self.render_logs(cx),
        };

        v_flex()
            .size_full()
            .key_context("RseqGpui")
            .on_action(
                cx.listener(|this, action: &SelectLinkModeAction, window, cx| {
                    if let Some(mode) = LinkMode::from_id(action.0) {
                        this.set_link_mode(mode, window, cx);
                    }
                    cx.notify();
                }),
            )
            .on_action(
                cx.listener(|this, action: &SelectSerialPortAction, window, cx| {
                    this.select_serial_port(action.0.clone(), window, cx);
                    cx.notify();
                }),
            )
            .on_action(
                cx.listener(|this, action: &SelectSerialBaudAction, window, cx| {
                    this.select_serial_baud(action.0, window, cx);
                    cx.notify();
                }),
            )
            .bg(cx.theme().background)
            .child(
                TitleBar::new().child(
                    TabBar::new("rseq-tabs")
                        .segmented()
                        .selected_index(self.selected_tab.index())
                        .on_click(cx.listener(|this, index: &usize, _window, cx| {
                            this.selected_tab = PanelTab::from_index(*index);
                            cx.notify();
                        }))
                        .children(
                            PanelTab::ALL
                                .into_iter()
                                .map(|tab| Tab::new().label(tab.label())),
                        ),
                ),
            )
            .child(self.render_connection_bar(cx))
            .child(div().flex_1().min_h_0().child(content))
            .child(self.render_status_bar(cx))
    }
}

fn motion_acc_vec3(sample: &MotionSample) -> Vec3 {
    [
        sample.acc[0] as f32,
        sample.acc[1] as f32,
        sample.acc[2] as f32,
    ]
}

fn motion_gyro_vec3(sample: &MotionSample) -> Vec3 {
    [
        sample.gyro[0] as f32,
        sample.gyro[1] as f32,
        sample.gyro[2] as f32,
    ]
}

fn axis_stddev(data: &[Vec3]) -> Vec3 {
    if data.is_empty() {
        return [0.0; 3];
    }

    let inv_len = 1.0 / data.len() as f32;
    let mut mean = [0.0f32; 3];
    for sample in data {
        for axis in 0..3 {
            mean[axis] += sample[axis] * inv_len;
        }
    }

    let mut variance = [0.0f32; 3];
    for sample in data {
        for axis in 0..3 {
            let delta = sample[axis] - mean[axis];
            variance[axis] += delta * delta * inv_len;
        }
    }

    [variance[0].sqrt(), variance[1].sqrt(), variance[2].sqrt()]
}

fn auto_chart_range(data: &[Vec3], min_span: f32) -> ChartRange {
    let peak = data
        .iter()
        .flat_map(|sample| sample.iter())
        .fold(0f32, |max, value| max.max(value.abs()));
    let y_abs = (peak * 1.15).max(min_span);
    ChartRange {
        y_min: -y_abs,
        y_max: y_abs,
    }
}

fn chart_y_fraction_at(bounds: Bounds<Pixels>, position: Point<Pixels>) -> Option<f32> {
    let height = bounds.size.height.as_f32();
    if height <= 0.0 {
        return None;
    }

    Some(((position.y.as_f32() - bounds.origin.y.as_f32()) / height).clamp(0.0, 1.0))
}

fn chart_x_fraction_at(bounds: Bounds<Pixels>, position: Point<Pixels>) -> Option<f32> {
    let width = bounds.size.width.as_f32();
    if width <= 0.0 {
        return None;
    }

    Some(((position.x.as_f32() - bounds.origin.x.as_f32()) / width).clamp(0.0, 1.0))
}

fn auto_chart_x_range(data_len: usize) -> ChartXRange {
    ChartXRange {
        x_min: 0.0,
        x_max: data_len.saturating_sub(1).max(1) as f32,
    }
}

fn chart_range_after_wheel(
    current: ChartRange,
    auto: ChartRange,
    y_fraction: f32,
    delta_y: f32,
) -> ChartRange {
    if delta_y == 0.0 {
        return current;
    }

    let base_span = auto.span();
    let min_span = base_span / CHART_ZOOM_MAX;
    let max_span = base_span / CHART_ZOOM_MIN;
    let next_span = if delta_y < 0.0 {
        current.span() / CHART_WHEEL_ZOOM_STEP
    } else if delta_y > 0.0 {
        current.span() * CHART_WHEEL_ZOOM_STEP
    } else {
        current.span()
    }
    .clamp(min_span, max_span);

    let y_fraction = y_fraction.clamp(0.0, 1.0);
    let anchor_value = current.value_at_y_fraction(y_fraction);
    ChartRange {
        y_min: anchor_value - (1.0 - y_fraction) * next_span,
        y_max: anchor_value + y_fraction * next_span,
    }
}

fn chart_range_after_drag(
    start: ChartRange,
    bounds: Bounds<Pixels>,
    start_position: Point<Pixels>,
    current_position: Point<Pixels>,
) -> ChartRange {
    let height = bounds.size.height.as_f32();
    if height <= 0.0 {
        return start;
    }

    let dy = current_position.y.as_f32() - start_position.y.as_f32();
    let shift = dy / height * start.span();
    ChartRange {
        y_min: start.y_min + shift,
        y_max: start.y_max + shift,
    }
}

fn chart_x_range_after_wheel(
    current: ChartXRange,
    auto: ChartXRange,
    x_fraction: f32,
    delta_y: f32,
) -> ChartXRange {
    if delta_y == 0.0 {
        return current;
    }

    let base_span = auto.span();
    let min_span = (base_span / CHART_ZOOM_MAX).max(1.0).min(base_span);
    let next_span = if delta_y < 0.0 {
        current.span() / CHART_WHEEL_ZOOM_STEP
    } else if delta_y > 0.0 {
        current.span() * CHART_WHEEL_ZOOM_STEP
    } else {
        current.span()
    }
    .clamp(min_span, base_span);

    let x_fraction = x_fraction.clamp(0.0, 1.0);
    let anchor_value = current.value_at_x_fraction(x_fraction);
    let x_min = anchor_value - x_fraction * next_span;
    let x_max = anchor_value + (1.0 - x_fraction) * next_span;
    clamp_chart_x_range_to_auto(ChartXRange { x_min, x_max }, auto)
}

fn chart_x_range_after_drag(
    start: ChartXRange,
    auto: ChartXRange,
    bounds: Bounds<Pixels>,
    start_position: Point<Pixels>,
    current_position: Point<Pixels>,
) -> ChartXRange {
    let width = bounds.size.width.as_f32();
    if width <= 0.0 {
        return start;
    }

    let dx = current_position.x.as_f32() - start_position.x.as_f32();
    let shift = -dx / width * start.span();
    clamp_chart_x_range_to_auto(
        ChartXRange {
            x_min: start.x_min + shift,
            x_max: start.x_max + shift,
        },
        auto,
    )
}

fn clamp_chart_x_range_to_auto(mut range: ChartXRange, auto: ChartXRange) -> ChartXRange {
    let span = range.span().min(auto.span());
    if range.x_min < auto.x_min {
        range.x_min = auto.x_min;
        range.x_max = auto.x_min + span;
    }
    if range.x_max > auto.x_max {
        range.x_max = auto.x_max;
        range.x_min = auto.x_max - span;
    }
    range.x_min = range.x_min.max(auto.x_min);
    range.x_max = range.x_max.min(auto.x_max);
    range
}

fn history_bar_index_at_x(
    bounds: Bounds<Pixels>,
    position: Point<Pixels>,
    bar_count: usize,
) -> Option<usize> {
    if bar_count == 0 {
        return None;
    }

    let width = bounds.size.width.as_f32();
    if width <= 0.0 {
        return None;
    }

    let x_fraction =
        ((position.x.as_f32() - bounds.origin.x.as_f32()) / width).clamp(0.0, 0.999_999);
    Some((x_fraction * bar_count as f32).floor() as usize)
}

fn push_history_capped(buf: &mut VecDeque<HistoryBar>, value: HistoryBar) {
    if buf.len() >= MAX_HISTORY_BARS {
        buf.pop_front();
    }
    buf.push_back(value);
}

fn push_vec_capped<T>(buf: &mut Vec<T>, value: T, cap: usize) {
    if cap == 0 {
        return;
    }
    if buf.len() >= cap {
        buf.remove(0);
    }
    buf.push(value);
}

fn register_display_width(regs: &[RegisterInfo], addr: u32) -> usize {
    regs.iter()
        .find(|reg| reg.addr == addr)
        .or_else(|| regs.first())
        .map(|reg| reg.width.max(1) as usize)
        .unwrap_or(1)
}

fn register_value_text(bytes: &[u8], expected_width: usize, no_dump: bool) -> String {
    if no_dump {
        return "??".to_string();
    }
    if bytes.is_empty() {
        return "--".to_string();
    }
    let suffix = if bytes.len() < expected_width {
        " ..."
    } else {
        ""
    };
    if bytes.len() == 1 {
        format!("0x{:02x}{suffix}", bytes[0])
    } else {
        let hex = bytes
            .iter()
            .rev()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        format!("0x{hex}{suffix}")
    }
}

fn register_has_active_fields(regs: &[RegisterInfo], bytes: &[u8]) -> bool {
    !bytes.is_empty()
        && regs.iter().any(|reg| {
            reg.fields
                .iter()
                .any(|field| field_value(bytes, field.bit_hi, field.bit_lo).is_some_and(|v| v != 0))
        })
}

fn register_is_readable(access: &str) -> bool {
    access.is_empty() || access.chars().any(|ch| ch == 'r' || ch == 'R')
}

fn register_is_writable(access: &str) -> bool {
    access.is_empty() || access.chars().any(|ch| ch == 'w' || ch == 'W')
}

fn register_dump_addresses(reg: &RegisterInfo) -> Vec<u32> {
    (0..reg.width.max(1))
        .map(|offset| reg.addr + offset)
        .collect()
}

fn register_dump_ranges(
    registers: &[RegisterInfo],
    max_range_len: usize,
) -> (Vec<RegisterDumpRange>, Vec<String>) {
    let max_range_len = max_range_len.max(1);
    let mut excluded = BTreeSet::new();
    let mut skipped = Vec::new();

    for reg in registers {
        let reason = if reg.no_dump {
            if reg.no_dump_reason.is_empty() {
                "no_dump=true".to_string()
            } else {
                reg.no_dump_reason.clone()
            }
        } else if !register_is_readable(&reg.access) {
            "write-only register".to_string()
        } else {
            continue;
        };

        for addr in register_dump_addresses(reg) {
            excluded.insert(addr);
        }
        skipped.push(format!(
            "{}.{}@0x{:02x} skipped: {reason}",
            reg.page, reg.name, reg.addr
        ));
    }

    let mut addresses = BTreeSet::new();
    for reg in registers {
        if reg.no_dump || !register_is_readable(&reg.access) {
            continue;
        }
        for addr in register_dump_addresses(reg) {
            if !excluded.contains(&addr) {
                addresses.insert(addr);
            }
        }
    }

    let mut ranges = Vec::new();
    let mut start = None;
    let mut last = 0u32;
    let mut len = 0usize;

    for addr in addresses {
        let continues = start.is_some()
            && last.checked_add(1) == Some(addr)
            && len < max_range_len
            && len < u16::MAX as usize;
        if continues {
            last = addr;
            len += 1;
            continue;
        }

        if let Some(start) = start {
            ranges.push(RegisterDumpRange {
                start,
                len: len as u16,
            });
        }
        start = Some(addr);
        last = addr;
        len = 1;
    }

    if let Some(start) = start {
        ranges.push(RegisterDumpRange {
            start,
            len: len as u16,
        });
    }

    (ranges, skipped)
}

fn field_bit_label(field: &FieldInfo) -> String {
    if field.bit_hi == field.bit_lo {
        field.bit_lo.to_string()
    } else {
        format!("{}:{}", field.bit_hi, field.bit_lo)
    }
}

fn field_value(bytes: &[u8], bit_hi: u8, bit_lo: u8) -> Option<u128> {
    let hi = bit_hi.max(bit_lo);
    let lo = bit_hi.min(bit_lo);
    let need = hi as usize / 8 + 1;
    if bytes.len() < need {
        return None;
    }

    let mut raw = 0u128;
    for (idx, byte) in bytes.iter().take(16).enumerate() {
        raw |= (*byte as u128) << (idx * 8);
    }
    let width = hi - lo + 1;
    let mask = if width >= 128 {
        u128::MAX
    } else {
        (1u128 << width) - 1
    };
    Some((raw >> lo) & mask)
}

fn field_value_text(field: &FieldInfo, bytes: &[u8]) -> String {
    let Some(value) = field_value(bytes, field.bit_hi, field.bit_lo) else {
        return "--".to_string();
    };

    let hi = field.bit_hi.max(field.bit_lo);
    let lo = field.bit_hi.min(field.bit_lo);
    let width = hi - lo + 1;
    if width == 1 {
        format!("{value}")
    } else {
        let nibbles = width.div_ceil(4) as usize;
        format!("0x{value:0nibbles$x}")
    }
}

fn field_tooltip_view(field: &FieldInfo, bytes: &[u8], cx: &App) -> AnyElement {
    let hi = field.bit_hi.max(field.bit_lo);
    let lo = field.bit_hi.min(field.bit_lo);
    let width = hi - lo + 1;
    let value = field_value(bytes, field.bit_hi, field.bit_lo);
    let value_text = field_value_text(field, bytes);
    let decimal_text = value
        .map(|value| format!("decimal {value}"))
        .unwrap_or_else(|| "read register first".to_string());

    v_flex()
        .w(px(380.))
        .max_w(px(520.))
        .gap_1p5()
        .child(
            h_flex()
                .gap_2()
                .items_baseline()
                .child(
                    div()
                        .font_family("monospace")
                        .text_xs()
                        .text_color(cx.theme().blue)
                        .child(field_bit_label(field)),
                )
                .child(
                    div()
                        .font_semibold()
                        .text_color(cx.theme().foreground)
                        .child(field.name.clone()),
                ),
        )
        .child(
            div()
                .font_family("monospace")
                .text_xs()
                .text_color(cx.theme().foreground)
                .child(format!(
                    "value {value_text} ({decimal_text}) · width {width}b · bits {hi}:{lo}"
                )),
        )
        .when_some(field.event.as_ref(), |this, event| {
            this.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().success)
                    .child(format!("event {event}")),
            )
        })
        .when(!field.desc.is_empty(), |this| {
            this.child(
                div()
                    .text_xs()
                    .w_full()
                    .whitespace_normal()
                    .text_color(cx.theme().muted_foreground)
                    .child(field.desc.clone()),
            )
        })
        .into_any_element()
}

fn register_tooltip_view(
    addr: u32,
    regs: Vec<RegisterInfo>,
    bytes: Vec<u8>,
    no_dump: bool,
    cx: &App,
) -> AnyElement {
    let value_line = if no_dump {
        "value ?? (marked no_dump)".to_string()
    } else if bytes.is_empty() {
        "value -- (not read)".to_string()
    } else if bytes.len() == 1 {
        format!("value 0x{0:02x} (0b{0:08b})", bytes[0])
    } else {
        format!("value {}", register_value_text(&bytes, bytes.len(), false))
    };

    let title = regs
        .first()
        .map(|reg| format!("{}.{}", reg.page, reg.name))
        .unwrap_or_else(|| format!("0x{addr:02x}"));
    let subtitle = if regs.len() > 1 {
        format!("0x{addr:02x} · {} overlapping definitions", regs.len())
    } else {
        regs.first()
            .map(|reg| format!("0x{addr:02x} · {} · {}B", reg.access, reg.width.max(1)))
            .unwrap_or_else(|| "no register metadata".to_string())
    };

    v_flex()
        .w(px(440.))
        .max_w(px(560.))
        .gap_2()
        .child(
            h_flex()
                .gap_2()
                .items_baseline()
                .w_full()
                .child(
                    div()
                        .font_semibold()
                        .flex_shrink_0()
                        .text_color(cx.theme().foreground)
                        .child(title),
                )
                .child(
                    div()
                        .font_family("monospace")
                        .text_xs()
                        .flex_1()
                        .min_w_0()
                        .whitespace_normal()
                        .text_color(cx.theme().muted_foreground)
                        .child(subtitle),
                ),
        )
        .child(
            div()
                .font_family("monospace")
                .text_xs()
                .text_color(cx.theme().foreground)
                .child(value_line),
        )
        .when(regs.is_empty(), |this| {
            this.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(
                        "single click selects this numeric address; double click writes one byte",
                    ),
            )
        })
        .children(regs.into_iter().map(|reg| {
            v_flex()
                .gap_1()
                .pt_1()
                .when(!reg.desc.is_empty(), |this| {
                    this.child(
                        div()
                            .text_xs()
                            .w_full()
                            .whitespace_normal()
                            .text_color(cx.theme().muted_foreground)
                            .child(reg.desc.clone()),
                    )
                })
                .when(reg.no_dump, |this| {
                    this.child(div().text_xs().text_color(cx.theme().warning).child(
                        if reg.no_dump_reason.is_empty() {
                            "no_dump".to_string()
                        } else {
                            format!("no_dump: {}", reg.no_dump_reason)
                        },
                    ))
                })
                .children(reg.fields.iter().map(|field| {
                    let value = field_value(&bytes, field.bit_hi, field.bit_lo);
                    let active = value.is_some_and(|value| value != 0);
                    h_flex()
                        .gap_2()
                        .items_baseline()
                        .w_full()
                        .px_1()
                        .py_0p5()
                        .rounded(px(3.))
                        .when(active, |this| this.bg(cx.theme().success.opacity(0.12)))
                        .child(
                            div()
                                .font_family("monospace")
                                .text_xs()
                                .text_color(cx.theme().blue)
                                .w(px(44.))
                                .flex_shrink_0()
                                .child(field_bit_label(field)),
                        )
                        .child(
                            div()
                                .font_family("monospace")
                                .text_xs()
                                .text_color(if active {
                                    cx.theme().success
                                } else {
                                    cx.theme().foreground
                                })
                                .w(px(52.))
                                .flex_shrink_0()
                                .child(field_value_text(field, &bytes)),
                        )
                        .child(
                            div()
                                .text_xs()
                                .flex_1()
                                .min_w_0()
                                .whitespace_normal()
                                .text_color(cx.theme().muted_foreground)
                                .child(if field.desc.is_empty() {
                                    field.name.clone()
                                } else {
                                    format!("{} - {}", field.name, field.desc)
                                }),
                        )
                }))
        }))
        .into_any_element()
}

fn status_dot(connected: bool, cx: &Context<RseqGpui>) -> impl IntoElement {
    div().size_2().rounded_full().bg(if connected {
        cx.theme().success
    } else {
        cx.theme().muted
    })
}

fn info_block(title: &str, body: String, cx: &Context<RseqGpui>) -> impl IntoElement {
    v_flex()
        .gap_2()
        .p_3()
        .border_1()
        .border_color(cx.theme().border)
        .rounded(cx.theme().radius)
        .child(div().text_sm().font_semibold().child(title.to_string()))
        .child(
            div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(body),
        )
}

fn sequence_info_block(title: &str, body: String, cx: &Context<RseqGpui>) -> impl IntoElement {
    v_flex()
        .gap_1()
        .child(
            div()
                .text_xs()
                .font_semibold()
                .text_color(cx.theme().muted_foreground)
                .child(title.to_string()),
        )
        .child(
            div()
                .font_family("monospace")
                .text_xs()
                .whitespace_normal()
                .text_color(cx.theme().foreground)
                .child(body),
        )
}

fn text_panel(title: &str, lines: &VecDeque<String>, cx: &Context<RseqGpui>) -> impl IntoElement {
    v_flex()
        .size_full()
        .min_h_0()
        .overflow_hidden()
        .p_3()
        .gap_2()
        .child(div().text_sm().font_semibold().child(title.to_string()))
        .child(
            div()
                .flex_1()
                .min_h_0()
                .overflow_hidden()
                .border_1()
                .border_color(cx.theme().border)
                .rounded(cx.theme().radius)
                .child(
                    v_flex()
                        .size_full()
                        .overflow_y_scrollbar()
                        .p_3()
                        .text_sm()
                        .children(lines.iter().rev().take(240).rev().map(|line| {
                            div()
                                .py_0p5()
                                .text_color(cx.theme().muted_foreground)
                                .child(line.clone())
                        })),
                ),
        )
}

fn path_picker_row(
    label: &str,
    input: &Entity<InputState>,
    button_id: &'static str,
    button_label: &'static str,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &Context<RseqGpui>,
) -> impl IntoElement {
    h_flex()
        .gap_2()
        .items_center()
        .child(
            div()
                .w(px(48.))
                .text_xs()
                .font_semibold()
                .text_color(cx.theme().muted_foreground)
                .child(label.to_string()),
        )
        .child(Input::new(input).small().flex_1())
        .child(
            Button::new(button_id)
                .small()
                .icon(IconName::FolderOpen)
                .label(button_label)
                .on_click(on_click),
        )
}

impl VisualSequence {
    fn new(name: impl Into<String>, window: &mut Window, cx: &mut Context<RseqGpui>) -> Self {
        Self {
            name_input: visual_input(window, cx, "Sequence name", name.into()),
            steps: Vec::new(),
        }
    }

    fn from_blueprint(
        blueprint: VisualSequenceBlueprint,
        window: &mut Window,
        cx: &mut Context<RseqGpui>,
    ) -> Self {
        Self {
            name_input: visual_input(window, cx, "Sequence name", blueprint.name),
            steps: blueprint
                .steps
                .into_iter()
                .map(|step| VisualStepEditor::from_blueprint(step, window, cx))
                .collect(),
        }
    }

    fn name(&self, cx: &Context<RseqGpui>) -> String {
        let name = self.name_input.read(cx).value().trim().to_string();
        if name.is_empty() {
            "Untitled".to_string()
        } else {
            name
        }
    }

    fn to_blueprint(&self, cx: &Context<RseqGpui>) -> VisualSequenceBlueprint {
        VisualSequenceBlueprint {
            name: self.name(cx),
            steps: self
                .steps
                .iter()
                .map(|step| step.to_blueprint(cx))
                .collect(),
        }
    }
}

impl VisualStepEditor {
    fn default_kind(kind: VisualStepKind, window: &mut Window, cx: &mut Context<RseqGpui>) -> Self {
        let (read_len, data) = match kind {
            VisualStepKind::Read => ("1", ""),
            VisualStepKind::Write => ("1", "0x00"),
        };
        Self::from_blueprint(
            VisualStepBlueprint {
                kind,
                address: "0x00".to_string(),
                read_len: read_len.to_string(),
                data: data.to_string(),
                delay_us: "0".to_string(),
            },
            window,
            cx,
        )
    }

    fn from_blueprint(
        blueprint: VisualStepBlueprint,
        window: &mut Window,
        cx: &mut Context<RseqGpui>,
    ) -> Self {
        Self {
            kind: blueprint.kind,
            address_input: visual_input(window, cx, "0x00 or UI.REG", blueprint.address),
            read_len_input: visual_input(window, cx, "1", blueprint.read_len),
            data_input: visual_input(window, cx, "0x00 or { field: 1 }", blueprint.data),
            delay_us_input: visual_input(window, cx, "0", blueprint.delay_us),
        }
    }

    fn duplicate(&self, window: &mut Window, cx: &mut Context<RseqGpui>) -> Self {
        Self::from_blueprint(self.to_blueprint(cx), window, cx)
    }

    fn to_blueprint(&self, cx: &Context<RseqGpui>) -> VisualStepBlueprint {
        VisualStepBlueprint {
            kind: self.kind,
            address: self.address_input.read(cx).value().trim().to_string(),
            read_len: self.read_len_input.read(cx).value().trim().to_string(),
            data: self.data_input.read(cx).value().trim().to_string(),
            delay_us: self.delay_us_input.read(cx).value().trim().to_string(),
        }
    }
}

fn visual_input(
    window: &mut Window,
    cx: &mut Context<RseqGpui>,
    placeholder: &str,
    value: String,
) -> Entity<InputState> {
    let input = cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(placeholder.to_string())
            .default_value(value)
    });
    cx.subscribe_in(
        &input,
        window,
        |this, _, event: &InputEvent, _window, cx| {
            if matches!(event, InputEvent::Change) {
                this.sequence_dirty = true;
                this.sequence_status = "visual sequence modified".to_string();
                cx.notify();
            }
        },
    )
    .detach();
    input
}

fn visual_sequences_from_source(
    source: &str,
    window: &mut Window,
    cx: &mut Context<RseqGpui>,
) -> Vec<VisualSequence> {
    let mut blueprints = visual_blueprints_from_source(source);
    if blueprints.is_empty() {
        blueprints.push(VisualSequenceBlueprint {
            name: "Startup".to_string(),
            steps: Vec::new(),
        });
    }

    blueprints
        .into_iter()
        .map(|blueprint| VisualSequence::from_blueprint(blueprint, window, cx))
        .collect()
}

fn visual_blueprints_from_source(source: &str) -> Vec<VisualSequenceBlueprint> {
    let mut sequences = Vec::<VisualSequenceBlueprint>::new();
    let mut current = VisualSequenceBlueprint {
        name: "Imported Text".to_string(),
        steps: Vec::new(),
    };

    for line in source.lines() {
        let trimmed = line.trim();
        if let Some(name) = trimmed.strip_prefix("// rseq-gpui:sequence ") {
            if !current.steps.is_empty() || current.name != "Imported Text" {
                sequences.push(current);
            }
            current = VisualSequenceBlueprint {
                name: name.trim().to_string(),
                steps: Vec::new(),
            };
            continue;
        }

        if let Some(args) = extract_macro_args(trimmed, "read!") {
            if args.len() >= 2 {
                current.steps.push(VisualStepBlueprint {
                    kind: VisualStepKind::Read,
                    address: args[0].clone(),
                    read_len: args[1].clone(),
                    data: String::new(),
                    delay_us: args.get(2).cloned().unwrap_or_else(|| "0".to_string()),
                });
            }
            continue;
        }

        if let Some(args) = extract_macro_args(trimmed, "write!") {
            if args.len() >= 2 {
                current.steps.push(VisualStepBlueprint {
                    kind: VisualStepKind::Write,
                    address: args[0].clone(),
                    read_len: "1".to_string(),
                    data: args[1].clone(),
                    delay_us: args.get(2).cloned().unwrap_or_else(|| "0".to_string()),
                });
            }
        }
    }

    if !current.steps.is_empty() || current.name != "Imported Text" {
        sequences.push(current);
    }

    sequences
}

fn visual_source_from_blueprints(
    sequences: &[VisualSequenceBlueprint],
    chip_paths: &[PathBuf],
) -> Result<String, Vec<String>> {
    let errors = visual_sequence_errors(sequences);
    if !errors.is_empty() {
        return Err(errors);
    }

    let mut out = String::new();
    out.push_str("// Generated by rseq-gpui Blocks view.\n");
    if let Some(chip) = chip_paths.first() {
        out.push_str(&format!(
            "chip!(\"{}\");\n\n",
            escape_rseq_string(&chip.display().to_string())
        ));
    }

    for sequence in sequences {
        out.push_str(&format!("// rseq-gpui:sequence {}\n", sequence.name));
        out.push_str(&format!(
            "print!(\"sequence: {}\\n\");\n",
            escape_rseq_string(&sequence.name)
        ));
        for step in &sequence.steps {
            out.push_str(&visual_step_source(step)?);
            out.push('\n');
        }
        out.push('\n');
    }

    Ok(out)
}

fn visual_step_source(step: &VisualStepBlueprint) -> Result<String, Vec<String>> {
    let address = validate_visual_address(&step.address).map_err(|err| vec![err])?;
    let delay = parse_visual_delay(&step.delay_us).map_err(|err| vec![err])?;
    let call = match step.kind {
        VisualStepKind::Read => {
            let len =
                validate_visual_expr(&step.read_len, "read length").map_err(|err| vec![err])?;
            match delay {
                Some(delay) => format!("read!({address}, {len}, {delay});"),
                None => format!("read!({address}, {len});"),
            }
        }
        VisualStepKind::Write => {
            let data = format_visual_write_data(&step.data).map_err(|err| vec![err])?;
            match delay {
                Some(delay) => format!("write!({address}, {data}, {delay});"),
                None => format!("write!({address}, {data});"),
            }
        }
    };
    Ok(call)
}

fn visual_sequence_errors(sequences: &[VisualSequenceBlueprint]) -> Vec<String> {
    let mut errors = Vec::new();
    if sequences.is_empty() {
        errors.push("add at least one visual sequence".to_string());
    }

    for sequence in sequences {
        if sequence.name.trim().is_empty() {
            errors.push("sequence name is required".to_string());
        }
        if sequence.steps.is_empty() {
            errors.push(format!(
                "{}: add at least one read or write step",
                sequence.name
            ));
        }
        for (idx, step) in sequence.steps.iter().enumerate() {
            let label = format!("{} step {}", sequence.name, idx + 1);
            if let Err(err) = validate_visual_address(&step.address) {
                errors.push(format!("{label}: {err}"));
            }
            if let Err(err) = parse_visual_delay(&step.delay_us) {
                errors.push(format!("{label}: delay {err}"));
            }
            match step.kind {
                VisualStepKind::Read => {
                    if let Err(err) = validate_visual_expr(&step.read_len, "read length") {
                        errors.push(format!("{label}: {err}"));
                    }
                }
                VisualStepKind::Write => {
                    if let Err(err) = format_visual_write_data(&step.data) {
                        errors.push(format!("{label}: data {err}"));
                    }
                }
            }
        }
    }

    errors
}

fn validate_visual_address(raw: &str) -> Result<String, String> {
    validate_visual_expr(raw, "address")
}

fn validate_visual_expr(raw: &str, label: &str) -> Result<String, String> {
    let value = raw.trim();
    if value.is_empty() {
        return Err(format!("{label} is required"));
    }
    if value.contains(';') || value.contains('\n') {
        return Err(format!("{label} contains an invalid separator"));
    }
    Ok(value.to_string())
}

fn parse_visual_delay(raw: &str) -> Result<Option<u32>, String> {
    let value = raw.trim();
    if value.is_empty() {
        return Ok(None);
    }
    let parsed = parse_visual_number_u32(value)?;
    Ok((parsed != 0).then_some(parsed))
}

fn parse_visual_number_u32(raw: &str) -> Result<u32, String> {
    let text = raw.trim().replace('_', "");
    if text.is_empty() {
        return Err("value is required".to_string());
    }
    let value = if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        u32::from_str_radix(hex, 16).map_err(|_| format!("invalid hex value `{}`", raw.trim()))?
    } else if let Some(bin) = text.strip_prefix("0b").or_else(|| text.strip_prefix("0B")) {
        u32::from_str_radix(bin, 2).map_err(|_| format!("invalid binary value `{}`", raw.trim()))?
    } else {
        text.parse::<u32>()
            .map_err(|_| format!("invalid decimal value `{}`", raw.trim()))?
    };
    Ok(value)
}

fn format_visual_write_data(raw: &str) -> Result<String, String> {
    let value = raw.trim();
    if value.is_empty() {
        return Err("write data is required".to_string());
    }
    if value.starts_with('{') || value.starts_with('[') {
        return Ok(value.to_string());
    }

    let bytes = parse_register_write_bytes(value)?;
    if bytes.len() == 1 {
        Ok(format!("0x{:02x}", bytes[0]))
    } else {
        Ok(format!(
            "[{}]",
            bytes
                .iter()
                .map(|byte| format!("0x{byte:02x}"))
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }
}

fn extract_macro_args(line: &str, macro_name: &str) -> Option<Vec<String>> {
    let start = line.find(macro_name)?;
    let open = start + macro_name.len();
    let bytes = line.as_bytes();
    if bytes.get(open).copied() != Some(b'(') {
        return None;
    }

    let mut depth = 0i32;
    let mut end = None;
    for (idx, ch) in line[open..].char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(open + idx);
                    break;
                }
            }
            _ => {}
        }
    }
    let end = end?;
    Some(split_macro_args(&line[open + 1..end]))
}

fn split_macro_args(args: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut square = 0i32;
    let mut curly = 0i32;
    let mut paren = 0i32;

    for (idx, ch) in args.char_indices() {
        match ch {
            '[' => square += 1,
            ']' => square -= 1,
            '{' => curly += 1,
            '}' => curly -= 1,
            '(' => paren += 1,
            ')' => paren -= 1,
            ',' if square == 0 && curly == 0 && paren == 0 => {
                out.push(args[start..idx].trim().to_string());
                start = idx + ch.len_utf8();
            }
            _ => {}
        }
    }

    if start <= args.len() {
        let tail = args[start..].trim();
        if !tail.is_empty() {
            out.push(tail.to_string());
        }
    }

    out
}

fn escape_rseq_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

struct LoadedWorkspace {
    metadata: HostMetadata,
    startup_program: Option<rseq::CompiledProgram>,
    status: String,
    ok: bool,
}

fn load_workspace(files: &[PathBuf], chips: &[PathBuf], compile_program: bool) -> LoadedWorkspace {
    let metadata = match load_host_metadata(files, chips) {
        Ok(metadata) => metadata,
        Err(err) => {
            let mut metadata = HostMetadata::default();
            let mut catalog = RegisterCatalog::default();
            std::mem::swap(&mut metadata.register_catalog, &mut catalog);
            return LoadedWorkspace {
                metadata,
                startup_program: None,
                status: format!("metadata error: {err}"),
                ok: false,
            };
        }
    };

    if files.is_empty() {
        return LoadedWorkspace {
            metadata,
            startup_program: None,
            status: "no rseq file selected".to_string(),
            ok: true,
        };
    }

    if !compile_program {
        return LoadedWorkspace {
            metadata,
            startup_program: None,
            status: "metadata loaded; startup program not compiled".to_string(),
            ok: true,
        };
    }

    match compile_rseq_files(files) {
        Ok(program) => {
            let status = format!(
                "compiled main={} bytes, irq_handlers={}",
                program.main.len(),
                program.irq_bytecodes.len()
            );
            LoadedWorkspace {
                metadata,
                startup_program: Some(program),
                status,
                ok: true,
            }
        }
        Err(err) => LoadedWorkspace {
            metadata,
            startup_program: None,
            status: format!("compile error: {err}"),
            ok: false,
        },
    }
}

fn load_workspace_from_sources(
    sources: &[RseqSource],
    chips: &[PathBuf],
    compile_program: bool,
) -> LoadedWorkspace {
    let metadata = match load_host_metadata_from_sources(sources, chips) {
        Ok(metadata) => metadata,
        Err(err) => {
            let mut metadata = HostMetadata::default();
            let mut catalog = RegisterCatalog::default();
            std::mem::swap(&mut metadata.register_catalog, &mut catalog);
            return LoadedWorkspace {
                metadata,
                startup_program: None,
                status: format!("metadata error: {err}"),
                ok: false,
            };
        }
    };

    if sources.is_empty() {
        return LoadedWorkspace {
            metadata,
            startup_program: None,
            status: "no rseq source selected".to_string(),
            ok: true,
        };
    }

    if !compile_program {
        return LoadedWorkspace {
            metadata,
            startup_program: None,
            status: "metadata loaded; startup program not compiled".to_string(),
            ok: true,
        };
    }

    match compile_rseq_sources(sources) {
        Ok(program) => {
            let status = format!(
                "compiled main={} bytes, irq_handlers={}",
                program.main.len(),
                program.irq_bytecodes.len()
            );
            LoadedWorkspace {
                metadata,
                startup_program: Some(program),
                status,
                ok: true,
            }
        }
        Err(err) => LoadedWorkspace {
            metadata,
            startup_program: None,
            status: format!("compile error: {err}"),
            ok: false,
        },
    }
}

fn load_startup(cli: &Cli) -> LoadedWorkspace {
    load_workspace(&cli.file, &cli.chip, !cli.watch)
}

fn initial_sequence_source(files: &[PathBuf]) -> (Option<PathBuf>, String, String) {
    if files.len() > 1 {
        let mut combined = String::new();
        let mut errors = Vec::new();
        for path in files {
            match std::fs::read_to_string(path) {
                Ok(source) => {
                    combined.push_str(&format!("// rseq-gpui:file {}\n", path.display()));
                    combined.push_str(&source);
                    if !combined.ends_with('\n') {
                        combined.push('\n');
                    }
                    combined.push('\n');
                }
                Err(err) => errors.push(format!("{}: {err}", path.display())),
            }
        }
        if errors.is_empty() {
            return (
                None,
                combined,
                format!("opened {} rseq files as combined source", files.len()),
            );
        }
        return (
            None,
            if combined.is_empty() {
                DEFAULT_RSEQ_SOURCE.to_string()
            } else {
                combined
            },
            format!("some rseq files failed to read: {}", errors.join("; ")),
        );
    }

    if let Some(path) = files.first() {
        match std::fs::read_to_string(path) {
            Ok(source) => {
                return (
                    Some(path.clone()),
                    source,
                    format!("opened {}", display_path_name(path)),
                );
            }
            Err(err) => {
                return (
                    Some(path.clone()),
                    DEFAULT_RSEQ_SOURCE.to_string(),
                    format!("failed to read {}: {err}", path.display()),
                );
            }
        }
    }

    (
        None,
        DEFAULT_RSEQ_SOURCE.to_string(),
        "new untitled sequence".to_string(),
    )
}

fn path_list_value(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join("; ")
}

fn parse_path_list(value: &str) -> Vec<PathBuf> {
    value
        .split([';', '\n'])
        .map(|part| part.trim().trim_matches('"').trim_matches('\''))
        .filter(|part| !part.is_empty())
        .map(PathBuf::from)
        .collect()
}

fn display_path_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(ToString::to_string)
        .unwrap_or_else(|| path.display().to_string())
}

fn parse_serial_baud(value: &str) -> Result<u32, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err("serial baud is required".to_string());
    }
    match trimmed.parse::<u32>() {
        Ok(baud) if baud > 0 => Ok(baud),
        _ => Err(format!("invalid serial baud: {trimmed}")),
    }
}

fn parse_nonnegative_usize(value: &str, label: &str) -> Result<usize, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(0);
    }
    trimmed
        .parse::<usize>()
        .map_err(|_| format!("invalid {label}: {trimmed}"))
}

fn serial_port_menu_label(port: &rseq_host::SerialPortInfo) -> String {
    if port.detail.is_empty() {
        port.label.clone()
    } else {
        format!("{} ({})", port.label, port.detail)
    }
}

fn capture_sidecar_path(path: &Path) -> PathBuf {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some(ext) if !ext.is_empty() => path.with_extension(format!("{ext}.json")),
        _ => path.with_extension("json"),
    }
}

fn read_capture_sidecar(path: &Path) -> Option<CaptureSidecar> {
    let sidecar_path = capture_sidecar_path(path);
    let text = std::fs::read_to_string(sidecar_path).ok()?;
    serde_json::from_str(&text).ok()
}

fn capture_decoder_meta(registry: &ReportDecoderRegistry) -> Vec<CaptureDecoderMeta> {
    registry
        .iter()
        .map(|(kind, decoder)| match decoder {
            ReportDecoder::I16Le(decoder) => CaptureDecoderMeta {
                kind: *kind,
                kind_label: rseq_host::report_kind_label(*kind),
                decoder: decoder.label.clone(),
                fields: decoder.fields.clone(),
                gyro_fields: decoder.gyro_fields.clone(),
                accel_fields: decoder.accel_fields.clone(),
                temp_field: decoder.temp_field.clone(),
                accel_fs_g: decoder.accel_fs_g,
                gyro_fs_dps: decoder.gyro_fs_dps,
                temp_lsb_per_c: decoder.temp_lsb_per_c,
                temp_offset_c: decoder.temp_offset_c,
                output: decoder.output.as_str().to_string(),
            },
        })
        .collect()
}

fn report_decoder_registry_from_sidecar(
    sidecar: &CaptureSidecar,
) -> Result<ReportDecoderRegistry, String> {
    let mut registry = ReportDecoderRegistry::default();
    for decoder in &sidecar.report_decoders {
        let output = match decoder.output.as_str() {
            "physical_f32" => ReportOutputMode::PhysicalF32,
            "raw_i16" => ReportOutputMode::RawI16,
            other => {
                return Err(format!(
                    "capture metadata decoder output must be physical_f32 or raw_i16, got {other}"
                ));
            }
        };
        let decoder_value = make_i16_le_decoder(
            &decoder.decoder,
            decoder.fields.clone(),
            decoder.gyro_fields.clone(),
            decoder.accel_fields.clone(),
            decoder.temp_field.clone(),
            decoder.accel_fs_g,
            decoder.gyro_fs_dps,
            decoder.temp_lsb_per_c,
            decoder.temp_offset_c,
            output,
        )?;
        registry.insert(decoder.kind, decoder_value);
    }
    Ok(registry)
}

fn main() {
    let cli = Cli::parse();
    let loaded = load_startup(&cli);
    let app = gpui_platform::application().with_assets(gpui_component_assets::Assets);

    app.run(move |cx| {
        gpui_component::init(cx);

        let window_options = WindowOptions {
            titlebar: Some(TitleBar::title_bar_options()),
            window_bounds: Some(WindowBounds::centered(size(px(1180.), px(780.)), cx)),
            ..Default::default()
        };

        cx.spawn(async move |cx| {
            cx.open_window(window_options, |window, cx| {
                window.activate_window();
                window.set_window_title("rseq GPUI");
                Theme::change(ThemeMode::Dark, Some(window), cx);

                let view = cx.new(|cx| {
                    RseqGpui::new(
                        cli,
                        loaded.metadata,
                        loaded.startup_program,
                        loaded.status,
                        window,
                        cx,
                    )
                });
                cx.new(|cx| Root::new(view, window, cx))
            })
            .expect("failed to open rseq-gpui window");
        })
        .detach();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_register(
        page: &str,
        name: &str,
        addr: u32,
        width: u32,
        access: &str,
        no_dump: bool,
    ) -> RegisterInfo {
        RegisterInfo {
            page: page.to_string(),
            name: name.to_string(),
            addr,
            access: access.to_string(),
            width,
            desc: String::new(),
            no_dump,
            no_dump_reason: String::new(),
            fields: Vec::new(),
        }
    }

    fn test_language_facts() -> rseq_lsp::LanguageFacts {
        rseq_lsp::LanguageFacts {
            pages: vec!["UI".to_string()],
            registers: vec![rseq_lsp::RegisterFact {
                page: "UI".to_string(),
                name: "FIFO_DATA".to_string(),
                addr: 0x30,
                access: "RO".to_string(),
                width: 1,
                desc: String::new(),
                no_dump: true,
                fields: Vec::new(),
            }],
            fields: vec![rseq_lsp::FieldFact {
                page: "UI".to_string(),
                register: "FIFO_STATUSH".to_string(),
                name: "fifo_wtm".to_string(),
                bit_hi: 6,
                bit_lo: 6,
                desc: String::new(),
                event: Some("fifo_watermark".to_string()),
            }],
            events: vec![rseq_lsp::EventFact {
                name: "fifo_watermark".to_string(),
                page: "UI".to_string(),
                register: "FIFO_STATUSH".to_string(),
                field: "fifo_wtm".to_string(),
                desc: String::new(),
            }],
            ..Default::default()
        }
    }

    fn semantic_labels(
        source: &str,
        facts: &rseq_lsp::LanguageFacts,
    ) -> Vec<(String, RseqSemanticKind)> {
        rseq_semantic_hits(source, 0..source.len(), facts)
            .into_iter()
            .map(|hit| (source[hit.range].to_string(), hit.kind))
            .collect()
    }

    fn gpui_position_to_offset_like_component(source: &str, line: u32, character: u32) -> usize {
        let mut current_line = 0u32;
        let mut line_start = 0usize;
        for (idx, ch) in source.char_indices() {
            if current_line == line {
                break;
            }
            if ch == '\n' {
                current_line += 1;
                line_start = idx + ch.len_utf8();
            }
        }
        if current_line != line {
            return source.len();
        }

        let line_end = source[line_start..]
            .find('\n')
            .map(|rel| line_start + rel)
            .unwrap_or(source.len());
        line_start
            + source[line_start..line_end]
                .chars()
                .take(character as usize)
                .map(char::len_utf8)
                .sum::<usize>()
    }

    fn semantic_token_byte_ranges_like_gpui(
        source: &str,
        tokens: &lsp_types::SemanticTokens,
    ) -> Vec<Range<usize>> {
        let mut line = 0u32;
        let mut character = 0u32;
        let mut ranges = Vec::new();

        for token in &tokens.data {
            if token.delta_line > 0 {
                line += token.delta_line;
                character = token.delta_start;
            } else {
                character += token.delta_start;
            }

            let start = gpui_position_to_offset_like_component(source, line, character);
            let end =
                gpui_position_to_offset_like_component(source, line, character + token.length);
            ranges.push(start..end);
        }

        ranges
    }

    #[::core::prelude::v1::test]
    fn rseq_gpui_completion_items_insert_plain_labels_not_snippets() {
        let range = lsp_types::Range::new(
            lsp_types::Position::new(0, 0),
            lsp_types::Position::new(0, 5),
        );
        let item = rseq_gpui_completion_item(
            "",
            lsp_types::CompletionItem {
                label: "write!".to_string(),
                insert_text_format: Some(lsp_types::InsertTextFormat::SNIPPET),
                text_edit: Some(lsp_types::CompletionTextEdit::Edit(lsp_types::TextEdit {
                    range,
                    new_text: "write!(${1:REG}, ${2:[0x00]}, ${3:50});".to_string(),
                })),
                ..Default::default()
            },
        );

        assert_eq!(
            item.insert_text_format,
            Some(lsp_types::InsertTextFormat::PLAIN_TEXT)
        );
        let Some(lsp_types::CompletionTextEdit::Edit(edit)) = item.text_edit else {
            panic!("expected edit");
        };
        assert_eq!(edit.new_text, "write!");
    }

    #[::core::prelude::v1::test]
    fn rseq_lsp_ranges_convert_utf16_columns_to_gpui_character_columns() {
        let source = "// 🎉 comment\nwrite!(UI.FIFO_DATA, 1);\n";
        let lsp_range = lsp_types::Range::new(
            lsp_types::Position::new(0, 3),
            lsp_types::Position::new(0, 5),
        );
        let gpui_range = rseq_lsp_range_to_gpui_range(source, lsp_range);

        assert_eq!(gpui_range.start, lsp_types::Position::new(0, 3));
        assert_eq!(gpui_range.end, lsp_types::Position::new(0, 4));
    }

    #[::core::prelude::v1::test]
    fn rseq_completion_item_converts_edit_range_for_gpui_component() {
        let source = "// 🎉 comment\nwri";
        let item = rseq_gpui_completion_item(
            source,
            lsp_types::CompletionItem {
                label: "write!".to_string(),
                text_edit: Some(lsp_types::CompletionTextEdit::Edit(lsp_types::TextEdit {
                    range: lsp_types::Range::new(
                        lsp_types::Position::new(1, 0),
                        lsp_types::Position::new(1, 3),
                    ),
                    new_text: "write!(${1:REG})".to_string(),
                })),
                ..Default::default()
            },
        );

        let Some(lsp_types::CompletionTextEdit::Edit(edit)) = item.text_edit else {
            panic!("expected edit");
        };
        assert_eq!(
            edit.range,
            lsp_types::Range::new(
                lsp_types::Position::new(1, 0),
                lsp_types::Position::new(1, 3)
            )
        );
        assert_eq!(edit.new_text, "write!");
    }

    #[::core::prelude::v1::test]
    fn rseq_completion_offer_state_avoids_finished_statements() {
        assert!(rseq_should_offer_completion("wri", 3));
        assert!(rseq_should_offer_completion("read!(", 6));
        assert!(!rseq_should_offer_completion("read!(UI.FIFO_DATA, 1);", 23));
        assert!(!rseq_should_offer_completion("let data = ", 11));
        assert!(!rseq_should_offer_completion("\n    ", 5));
    }

    #[::core::prelude::v1::test]
    fn rseq_semantic_highlight_marks_dsl_and_chip_symbols() {
        let facts = test_language_facts();
        let source = r#"
            // irq handler
            irq!(int1) {
                on(fifo_watermark) {
                    let data = read!(UI.FIFO_DATA, 0x0e);
                    report!(FIFO_RAW, data);
                }
            }
        "#;
        let labels = semantic_labels(source, &facts);

        assert!(labels.contains(&("// irq handler".to_string(), RseqSemanticKind::Comment)));
        assert!(labels.contains(&("irq!".to_string(), RseqSemanticKind::Function)));
        assert!(labels.contains(&("on".to_string(), RseqSemanticKind::Keyword)));
        assert!(labels.contains(&("fifo_watermark".to_string(), RseqSemanticKind::Label)));
        assert!(labels.contains(&("read!".to_string(), RseqSemanticKind::Function)));
        assert!(labels.contains(&("UI.FIFO_DATA".to_string(), RseqSemanticKind::Property)));
        assert!(labels.contains(&("0x0e".to_string(), RseqSemanticKind::Number)));
        assert!(labels.contains(&("FIFO_RAW".to_string(), RseqSemanticKind::Constant)));
        assert!(labels.contains(&("data".to_string(), RseqSemanticKind::Variable)));
    }

    #[::core::prelude::v1::test]
    fn rseq_semantic_highlight_ignores_commands_inside_strings_and_comments() {
        let facts = test_language_facts();
        let source =
            "print!(\"read!(UI.FIFO_DATA)\"); // write!(UI.FIFO_DATA)\nread!(UI.FIFO_DATA, 1);";
        let labels = semantic_labels(source, &facts);
        let read_functions = labels
            .iter()
            .filter(|(label, kind)| label == "read!" && *kind == RseqSemanticKind::Function)
            .count();
        let register_refs = labels
            .iter()
            .filter(|(label, kind)| label == "UI.FIFO_DATA" && *kind == RseqSemanticKind::Property)
            .count();

        assert!(labels.contains(&(
            "\"read!(UI.FIFO_DATA)\"".to_string(),
            RseqSemanticKind::String
        )));
        assert!(labels.contains(&(
            "// write!(UI.FIFO_DATA)".to_string(),
            RseqSemanticKind::Comment
        )));
        assert_eq!(read_functions, 1);
        assert_eq!(register_refs, 1);
    }

    #[::core::prelude::v1::test]
    fn rseq_semantic_tokens_use_gpui_character_columns_for_non_ascii_text() {
        let facts = test_language_facts();
        let source = "// 中断处理 🎉 ±4096 dps\nwrite!(UI.FIFO_DATA, 1);\n";
        let tokens = rseq_semantic_tokens(source, 0..source.len(), &facts);

        assert_eq!(tokens.data[0].delta_line, 0);
        assert_eq!(tokens.data[0].delta_start, 0);
        assert_eq!(
            tokens.data[0].length,
            "// 中断处理 🎉 ±4096 dps".chars().count() as u32
        );

        let write = tokens
            .data
            .iter()
            .find(|token| {
                token.delta_line == 1 && token.token_type == RseqSemanticKind::Function.token_type()
            })
            .expect("write! should be highlighted on the second line");
        assert_eq!(write.delta_start, 0);
        assert_eq!(write.length, "write!".chars().count() as u32);
    }

    #[::core::prelude::v1::test]
    fn rseq_semantic_token_ranges_are_utf8_boundaries_for_qmi_style_comments() {
        let facts = test_language_facts();
        let source = r#"
chip!("qmi8660.yaml");
write!(UI.ACTL1, { afs_ui: 2 }, 50);  // Accel: ODR 100 Hz, full-scale ±16 g.

irq!(int1) {
    // FIFO 水位线：先读取 FIFO 长度，再读取 FIFO_DATA，把 FIFO 水位线条件撤销。
    on(fifo_watermark) {
        // 按当前 FIFO 长度精确读取 FIFO_DATA，撤销 watermark 条件，并上报原始 FIFO。
        let data = read!(UI.FIFO_DATA, 14);
        report!(FIFO_RAW, 14, data);
    }
}
"#;
        let tokens = rseq_semantic_tokens(source, 0..source.len(), &facts);
        let ranges = semantic_token_byte_ranges_like_gpui(source, &tokens);

        assert!(!ranges.is_empty());
        for range in ranges {
            assert!(
                source.is_char_boundary(range.start),
                "semantic token starts inside utf-8 codepoint: {range:?}"
            );
            assert!(
                source.is_char_boundary(range.end),
                "semantic token ends inside utf-8 codepoint: {range:?}"
            );
        }
    }

    #[::core::prelude::v1::test]
    fn visual_parser_collects_read_write_steps_from_rseq() {
        let source = r#"
            chip!("qmi8660.yaml");
            // rseq-gpui:sequence Init
            write!(UI.ACTL1, { afs_ui: 2, ast: 0 }, 50);
            let whoami = read!(UI.WHOAMI, 1, 100);
        "#;

        let sequences = visual_blueprints_from_source(source);
        assert_eq!(sequences.len(), 1);
        assert_eq!(sequences[0].name, "Init");
        assert_eq!(sequences[0].steps.len(), 2);
        assert_eq!(sequences[0].steps[0].kind, VisualStepKind::Write);
        assert_eq!(sequences[0].steps[0].address, "UI.ACTL1");
        assert_eq!(sequences[0].steps[0].data, "{ afs_ui: 2, ast: 0 }");
        assert_eq!(sequences[0].steps[1].kind, VisualStepKind::Read);
        assert_eq!(sequences[0].steps[1].read_len, "1");
    }

    #[::core::prelude::v1::test]
    fn visual_source_generation_compiles_numeric_register_steps() {
        let sequences = vec![VisualSequenceBlueprint {
            name: "Smoke".to_string(),
            steps: vec![
                VisualStepBlueprint {
                    kind: VisualStepKind::Write,
                    address: "0x10".to_string(),
                    read_len: "1".to_string(),
                    data: "0xaa 0x55".to_string(),
                    delay_us: "10".to_string(),
                },
                VisualStepBlueprint {
                    kind: VisualStepKind::Read,
                    address: "0x20".to_string(),
                    read_len: "2".to_string(),
                    data: String::new(),
                    delay_us: "0".to_string(),
                },
            ],
        }];

        let source = visual_source_from_blueprints(&sequences, &[]).unwrap();
        assert!(source.contains("write!(0x10, [0xaa, 0x55], 10);"));
        assert!(source.contains("read!(0x20, 2);"));
        let parsed = rseq::parse_detailed(&source).unwrap();
        let unit = rseq::ProgramUnit {
            program: &parsed,
            base_dir: None,
        };
        rseq::compile_program_units(&[unit]).unwrap();
    }

    #[::core::prelude::v1::test]
    fn register_dump_ranges_batch_contiguous_readable_bytes() {
        let registers = vec![
            test_register("UI", "WHOAMI", 0x00, 1, "ro", false),
            test_register("UI", "CTRL", 0x01, 2, "rw", false),
            test_register("UI", "SECRET", 0x03, 1, "ro", true),
            test_register("UI", "FIFO_DATA", 0x04, 1, "ro", false),
            test_register("UI", "CMD", 0x08, 1, "wo", false),
            test_register("UI", "STATUS", 0x09, 1, "ro", false),
        ];

        let (ranges, skipped) = register_dump_ranges(&registers, 64);
        assert_eq!(
            ranges,
            vec![
                RegisterDumpRange {
                    start: 0x00,
                    len: 3,
                },
                RegisterDumpRange {
                    start: 0x04,
                    len: 1,
                },
                RegisterDumpRange {
                    start: 0x09,
                    len: 1,
                },
            ]
        );
        assert_eq!(skipped.len(), 2);
    }

    #[::core::prelude::v1::test]
    fn register_dump_ranges_split_at_batch_limit() {
        let registers = vec![test_register("UI", "BLOCK", 0x20, 10, "ro", false)];
        let (ranges, skipped) = register_dump_ranges(&registers, 4);
        assert!(skipped.is_empty());
        assert_eq!(
            ranges,
            vec![
                RegisterDumpRange {
                    start: 0x20,
                    len: 4,
                },
                RegisterDumpRange {
                    start: 0x24,
                    len: 4,
                },
                RegisterDumpRange {
                    start: 0x28,
                    len: 2,
                },
            ]
        );
    }

    #[::core::prelude::v1::test]
    fn axis_stddev_computes_xyz_population_stddev() {
        let data = vec![[1.0, 2.0, 3.0], [3.0, 2.0, -1.0], [5.0, 2.0, 3.0]];
        let std = axis_stddev(&data);
        assert!((std[0] - 1.6329932).abs() < 0.00001);
        assert_eq!(std[1], 0.0);
        assert!((std[2] - 1.8856181).abs() < 0.00001);
    }

    #[::core::prelude::v1::test]
    fn chart_range_after_wheel_keeps_mouse_anchor_value_stable() {
        let current = ChartRange {
            y_min: -10.0,
            y_max: 10.0,
        };
        let auto = current;
        let y_fraction = 0.25;
        let before = current.value_at_y_fraction(y_fraction);
        let after = chart_range_after_wheel(current, auto, y_fraction, -10.0);

        assert!(after.span() < current.span());
        assert!((after.value_at_y_fraction(y_fraction) - before).abs() < 0.0001);
    }

    #[::core::prelude::v1::test]
    fn chart_range_after_wheel_clamps_to_zoom_limits() {
        let auto = ChartRange {
            y_min: -10.0,
            y_max: 10.0,
        };
        let min = chart_range_after_wheel(
            ChartRange {
                y_min: -1.25,
                y_max: 1.25,
            },
            auto,
            0.5,
            -10.0,
        );
        let max = chart_range_after_wheel(
            ChartRange {
                y_min: -40.0,
                y_max: 40.0,
            },
            auto,
            0.5,
            10.0,
        );

        assert!((min.span() - auto.span() / CHART_ZOOM_MAX).abs() < 0.0001);
        assert!((max.span() - auto.span() / CHART_ZOOM_MIN).abs() < 0.0001);
    }

    #[::core::prelude::v1::test]
    fn chart_x_range_after_wheel_keeps_mouse_anchor_index_stable() {
        let current = ChartXRange {
            x_min: 0.0,
            x_max: 99.0,
        };
        let auto = current;
        let x_fraction = 0.75;
        let before = current.value_at_x_fraction(x_fraction);
        let after = chart_x_range_after_wheel(current, auto, x_fraction, -10.0);

        assert!(after.span() < current.span());
        assert!((after.value_at_x_fraction(x_fraction) - before).abs() < 0.0001);
    }

    #[::core::prelude::v1::test]
    fn chart_x_range_after_wheel_stays_inside_data_bounds() {
        let auto = ChartXRange {
            x_min: 0.0,
            x_max: 99.0,
        };
        let left = chart_x_range_after_wheel(auto, auto, 0.0, -10.0);
        let right = chart_x_range_after_wheel(auto, auto, 1.0, -10.0);
        let max = chart_x_range_after_wheel(
            ChartXRange {
                x_min: 10.0,
                x_max: 90.0,
            },
            auto,
            0.5,
            10.0,
        );

        assert_eq!(left.x_min, auto.x_min);
        assert_eq!(right.x_max, auto.x_max);
        assert!(max.x_min >= auto.x_min);
        assert!(max.x_max <= auto.x_max);
    }

    #[::core::prelude::v1::test]
    fn chart_range_after_drag_pans_y_by_pixel_delta() {
        let bounds = Bounds {
            origin: Point {
                x: px(0.0),
                y: px(0.0),
            },
            size: Size {
                width: px(400.0),
                height: px(100.0),
            },
        };
        let start = ChartRange {
            y_min: -10.0,
            y_max: 10.0,
        };
        let after = chart_range_after_drag(
            start,
            bounds,
            Point {
                x: px(200.0),
                y: px(50.0),
            },
            Point {
                x: px(200.0),
                y: px(60.0),
            },
        );

        assert!((after.y_min - -8.0).abs() < 0.0001);
        assert!((after.y_max - 12.0).abs() < 0.0001);
    }

    #[::core::prelude::v1::test]
    fn chart_x_range_after_drag_pans_and_clamps_to_data_bounds() {
        let bounds = Bounds {
            origin: Point {
                x: px(0.0),
                y: px(0.0),
            },
            size: Size {
                width: px(400.0),
                height: px(100.0),
            },
        };
        let auto = ChartXRange {
            x_min: 0.0,
            x_max: 99.0,
        };
        let start = ChartXRange {
            x_min: 20.0,
            x_max: 60.0,
        };
        let after = chart_x_range_after_drag(
            start,
            auto,
            bounds,
            Point {
                x: px(200.0),
                y: px(50.0),
            },
            Point {
                x: px(160.0),
                y: px(50.0),
            },
        );
        let clamped = chart_x_range_after_drag(
            start,
            auto,
            bounds,
            Point {
                x: px(200.0),
                y: px(50.0),
            },
            Point {
                x: px(-1200.0),
                y: px(50.0),
            },
        );

        assert!((after.x_min - 24.0).abs() < 0.0001);
        assert!((after.x_max - 64.0).abs() < 0.0001);
        assert!(clamped.x_min >= auto.x_min);
        assert_eq!(clamped.x_max, auto.x_max);
    }

    #[::core::prelude::v1::test]
    fn history_bar_index_at_x_uses_horizontal_position() {
        let bounds = Bounds {
            origin: Point {
                x: px(10.0),
                y: px(20.0),
            },
            size: Size {
                width: px(400.0),
                height: px(100.0),
            },
        };

        assert_eq!(
            history_bar_index_at_x(
                bounds,
                Point {
                    x: px(10.0),
                    y: px(20.0)
                },
                4
            ),
            Some(0)
        );
        assert_eq!(
            history_bar_index_at_x(
                bounds,
                Point {
                    x: px(210.0),
                    y: px(20.0)
                },
                4
            ),
            Some(2)
        );
        assert_eq!(
            history_bar_index_at_x(
                bounds,
                Point {
                    x: px(999.0),
                    y: px(20.0)
                },
                4
            ),
            Some(3)
        );
        assert_eq!(
            history_bar_index_at_x(
                bounds,
                Point {
                    x: px(10.0),
                    y: px(20.0)
                },
                0
            ),
            None
        );
    }

    #[::core::prelude::v1::test]
    fn parse_serial_baud_rejects_empty_zero_and_text() {
        assert_eq!(parse_serial_baud("115200").unwrap(), 115_200);
        assert!(parse_serial_baud("").is_err());
        assert!(parse_serial_baud("0").is_err());
        assert!(parse_serial_baud("fast").is_err());
    }

    #[::core::prelude::v1::test]
    fn parse_nonnegative_usize_treats_empty_as_zero() {
        assert_eq!(parse_nonnegative_usize("", "skip samples").unwrap(), 0);
        assert_eq!(parse_nonnegative_usize("12", "skip samples").unwrap(), 12);
        assert!(parse_nonnegative_usize("-1", "skip samples").is_err());
        assert!(parse_nonnegative_usize("abc", "skip samples").is_err());
    }

    #[::core::prelude::v1::test]
    fn serial_port_menu_label_includes_detail_when_present() {
        let plain = rseq_host::SerialPortInfo {
            port_name: "/dev/cu.usbmodem1".to_string(),
            label: "cu.usbmodem1".to_string(),
            detail: String::new(),
        };
        assert_eq!(serial_port_menu_label(&plain), "cu.usbmodem1");

        let detailed = rseq_host::SerialPortInfo {
            port_name: "/dev/cu.usbmodem2".to_string(),
            label: "cu.usbmodem2 - STLINK".to_string(),
            detail: "USB 0483:374b STMicroelectronics".to_string(),
        };
        assert_eq!(
            serial_port_menu_label(&detailed),
            "cu.usbmodem2 - STLINK (USB 0483:374b STMicroelectronics)"
        );
    }

    #[::core::prelude::v1::test]
    fn capture_sidecar_path_keeps_bin_extension_visible() {
        assert_eq!(
            capture_sidecar_path(Path::new("/tmp/fifo.bin")),
            PathBuf::from("/tmp/fifo.bin.json")
        );
    }

    #[::core::prelude::v1::test]
    fn capture_decoder_meta_round_trips_i16_le_decoder() {
        let mut registry = ReportDecoderRegistry::default();
        registry.insert(
            rseq::REPORT_KIND_FIFO_RAW,
            make_i16_le_decoder(
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
            .unwrap(),
        );
        let sidecar = CaptureSidecar {
            version: 1,
            format: "rseq-report-capture-bin-v1".to_string(),
            rseq_files: vec!["examples/qmi8660_fifo.rseq".to_string()],
            chip_files: vec!["qmi8660.yaml".to_string()],
            skip_samples: 3,
            report_decoders: capture_decoder_meta(&registry),
        };

        let restored = report_decoder_registry_from_sidecar(&sidecar).unwrap();
        assert_eq!(restored, registry);
    }

    #[::core::prelude::v1::test]
    fn macro_arg_split_keeps_nested_field_maps() {
        let args = extract_macro_args(
            "write!(UI.INT1_CTL0, { i1en_f_full:1, i1en_f_wtm: 1, i1en_f_ovf:1 }, 5);",
            "write!",
        )
        .unwrap();
        assert_eq!(
            args,
            vec![
                "UI.INT1_CTL0".to_string(),
                "{ i1en_f_full:1, i1en_f_wtm: 1, i1en_f_ovf:1 }".to_string(),
                "5".to_string(),
            ]
        );
    }
}
