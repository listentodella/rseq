use std::collections::{HashMap, HashSet};
use std::ops::Range as ByteRange;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::{
    CompletionItem, CompletionItemKind, CompletionList, CompletionOptions, CompletionParams,
    CompletionResponse, CompletionTextEdit, Diagnostic, DiagnosticSeverity,
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DidSaveTextDocumentParams, Documentation, Hover, HoverContents, HoverParams, InitializeParams,
    InitializeResult, InitializedParams, InsertTextFormat, MarkupContent, MarkupKind, MessageType,
    Position, Range, ServerCapabilities, TextDocumentContentChangeEvent,
    TextDocumentSyncCapability, TextDocumentSyncKind, TextDocumentSyncOptions,
    TextDocumentSyncSaveOptions, TextEdit, Url,
};
use tower_lsp::{Client, LanguageServer, LspService, Server};

#[derive(Debug, Clone, Default)]
pub struct ServerOptions {
    pub chips: Vec<PathBuf>,
}

pub async fn run_stdio(options: ServerOptions) {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(|client| Backend::new(client, options.clone()));
    Server::new(stdin, stdout, socket).serve(service).await;
}

struct Backend {
    client: Client,
    options: ServerOptions,
    state: Arc<Mutex<BackendState>>,
}

impl Backend {
    fn new(client: Client, options: ServerOptions) -> Self {
        Self {
            client,
            options,
            state: Arc::new(Mutex::new(BackendState::default())),
        }
    }

    fn snapshot(&self, uri: &Url) -> Option<DocumentSnapshot> {
        let state = self.state.lock().ok()?;
        if let Some(doc) = state.documents.get(uri) {
            return Some(DocumentSnapshot {
                text: doc.text.clone(),
                version: doc.version,
                root_dir: state.root_dir.clone(),
                configured_chips: state.configured_chips.clone(),
            });
        }

        let path = uri.to_file_path().ok()?;
        let text = std::fs::read_to_string(path).ok()?;
        Some(DocumentSnapshot {
            text,
            version: None,
            root_dir: state.root_dir.clone(),
            configured_chips: state.configured_chips.clone(),
        })
    }

    async fn publish_document_diagnostics(&self, uri: Url) {
        let Some(snapshot) = self.snapshot(&uri) else {
            return;
        };
        let analysis = analyze_document(
            &snapshot.text,
            document_base_dir(&uri).as_deref(),
            snapshot.root_dir.as_deref(),
            &snapshot.configured_chips,
        );
        self.client
            .publish_diagnostics(uri, analysis.diagnostics, snapshot.version)
            .await;
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        let root_dir = params.root_uri.as_ref().and_then(uri_to_dir).or_else(|| {
            params
                .workspace_folders
                .as_ref()
                .and_then(|folders| folders.first())
                .and_then(|folder| uri_to_dir(&folder.uri))
        });

        let mut configured_chips = self.options.chips.clone();
        configured_chips.extend(initialization_chip_paths(
            params.initialization_options.as_ref(),
        ));

        if let Ok(mut state) = self.state.lock() {
            state.root_dir = root_dir;
            state.configured_chips = configured_chips;
        }

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::FULL),
                        save: Some(TextDocumentSyncSaveOptions::Supported(true)),
                        ..Default::default()
                    },
                )),
                completion_provider: Some(CompletionOptions {
                    resolve_provider: Some(false),
                    trigger_characters: Some(vec![
                        ".".to_string(),
                        "!".to_string(),
                        "(".to_string(),
                        "{".to_string(),
                        ",".to_string(),
                    ]),
                    ..Default::default()
                }),
                hover_provider: Some(tower_lsp::lsp_types::HoverProviderCapability::Simple(true)),
                ..Default::default()
            },
            server_info: Some(tower_lsp::lsp_types::ServerInfo {
                name: "rseq-lsp".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(
                MessageType::INFO,
                "rseq-lsp initialized: diagnostics, completions, and hover are active",
            )
            .await;
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri;
        if let Ok(mut state) = self.state.lock() {
            state.documents.insert(
                uri.clone(),
                OpenDocument {
                    text: params.text_document.text,
                    version: Some(params.text_document.version),
                },
            );
        }
        self.publish_document_diagnostics(uri).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        let version = Some(params.text_document.version);
        let text = latest_full_text(params.content_changes);
        if let (Some(text), Ok(mut state)) = (text, self.state.lock()) {
            state
                .documents
                .entry(uri.clone())
                .and_modify(|doc| {
                    doc.text = text.clone();
                    doc.version = version;
                })
                .or_insert(OpenDocument { text, version });
        }
        self.publish_document_diagnostics(uri).await;
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        if let Some(text) = params.text {
            if let Ok(mut state) = self.state.lock() {
                state
                    .documents
                    .entry(params.text_document.uri.clone())
                    .and_modify(|doc| doc.text = text.clone())
                    .or_insert(OpenDocument {
                        text,
                        version: None,
                    });
            }
        }
        self.publish_document_diagnostics(params.text_document.uri)
            .await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        if let Ok(mut state) = self.state.lock() {
            state.documents.remove(&params.text_document.uri);
        }
        self.client
            .publish_diagnostics(params.text_document.uri, Vec::new(), None)
            .await;
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let uri = params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        let Some(snapshot) = self.snapshot(&uri) else {
            return Ok(None);
        };
        let analysis = analyze_document(
            &snapshot.text,
            document_base_dir(&uri).as_deref(),
            snapshot.root_dir.as_deref(),
            &snapshot.configured_chips,
        );
        let items = completion_items(&snapshot.text, position, &analysis.facts);
        Ok(Some(CompletionResponse::List(CompletionList {
            is_incomplete: false,
            items,
        })))
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        let Some(snapshot) = self.snapshot(&uri) else {
            return Ok(None);
        };
        let analysis = analyze_document(
            &snapshot.text,
            document_base_dir(&uri).as_deref(),
            snapshot.root_dir.as_deref(),
            &snapshot.configured_chips,
        );
        Ok(hover_at(&snapshot.text, position, &analysis.facts))
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}

#[derive(Debug, Default)]
struct BackendState {
    root_dir: Option<PathBuf>,
    configured_chips: Vec<PathBuf>,
    documents: HashMap<Url, OpenDocument>,
}

#[derive(Debug, Clone)]
struct OpenDocument {
    text: String,
    version: Option<i32>,
}

#[derive(Debug, Clone)]
struct DocumentSnapshot {
    text: String,
    version: Option<i32>,
    root_dir: Option<PathBuf>,
    configured_chips: Vec<PathBuf>,
}

#[derive(Debug, Clone, Default)]
pub struct DocumentAnalysis {
    pub diagnostics: Vec<Diagnostic>,
    pub facts: LanguageFacts,
}

#[derive(Debug, Clone, Default)]
pub struct LanguageFacts {
    pub chips: Vec<ChipFact>,
    pub pages: Vec<String>,
    pub registers: Vec<RegisterFact>,
    pub fields: Vec<FieldFact>,
    pub events: Vec<EventFact>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChipFact {
    pub sensor: String,
    pub source: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterFact {
    pub page: String,
    pub name: String,
    pub addr: u32,
    pub access: String,
    pub width: u32,
    pub desc: String,
    pub no_dump: bool,
    pub fields: Vec<FieldFact>,
}

impl RegisterFact {
    fn qualified_name(&self) -> String {
        format!("{}.{}", self.page, self.name)
    }

    fn hover_markdown(&self) -> String {
        let mut value = format!(
            "**{}**\n\nAddress: `0x{:02x}`  \nAccess: `{}`  \nWidth: `{}` byte(s)",
            self.qualified_name(),
            self.addr,
            self.access,
            self.width
        );
        if self.no_dump {
            value.push_str("  \nDump: `no_dump`");
        }
        if !self.desc.is_empty() {
            value.push_str("\n\n");
            value.push_str(&self.desc);
        }
        if !self.fields.is_empty() {
            value.push_str("\n\nFields:");
            for field in &self.fields {
                value.push_str(&format!(
                    "\n- `{}` `[{}:{}]`{}",
                    field.name,
                    field.bit_hi,
                    field.bit_lo,
                    field
                        .event
                        .as_ref()
                        .map(|event| format!(" event `{event}`"))
                        .unwrap_or_default()
                ));
            }
        }
        value
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldFact {
    pub page: String,
    pub register: String,
    pub name: String,
    pub bit_hi: u8,
    pub bit_lo: u8,
    pub desc: String,
    pub event: Option<String>,
}

impl FieldFact {
    fn qualified_name(&self) -> String {
        format!("{}.{}.{}", self.page, self.register, self.name)
    }

    fn hover_markdown(&self) -> String {
        let mut value = format!(
            "**{}**\n\nBits: `[{}:{}]`",
            self.qualified_name(),
            self.bit_hi,
            self.bit_lo
        );
        if let Some(event) = &self.event {
            value.push_str(&format!("  \nEvent: `{event}`"));
        }
        if !self.desc.is_empty() {
            value.push_str("\n\n");
            value.push_str(&self.desc);
        }
        value
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventFact {
    pub name: String,
    pub page: String,
    pub register: String,
    pub field: String,
    pub desc: String,
}

impl EventFact {
    fn hover_markdown(&self) -> String {
        let source = format!("{}.{}.{}", self.page, self.register, self.field);
        let mut value = format!("**event `{}`**\n\nSource: `{source}`", self.name);
        if !self.desc.is_empty() {
            value.push_str("\n\n");
            value.push_str(&self.desc);
        }
        value
    }
}

#[derive(Debug, Clone)]
struct CompletionSeed {
    label: String,
    insert_text: String,
    kind: CompletionItemKind,
    detail: String,
    documentation: Option<String>,
    snippet: bool,
}

pub fn analyze_document(
    source: &str,
    base_dir: Option<&Path>,
    root_dir: Option<&Path>,
    configured_chips: &[PathBuf],
) -> DocumentAnalysis {
    let parse_result = rseq::parse_detailed(source);
    let parsed_program = parse_result.as_ref().ok();
    let mut diagnostics = Vec::new();
    let mut facts = LanguageFacts::default();

    for chip_path in configured_chips {
        let resolved = resolve_configured_chip_path(chip_path, base_dir, root_dir);
        if let Err(message) = add_chip_facts(&resolved, &mut facts) {
            diagnostics.push(diagnostic_at_start(
                format!(
                    "failed to load chip metadata {}: {message}",
                    resolved.display()
                ),
                DiagnosticSeverity::WARNING,
            ));
        }
    }

    let source_chip_paths = chip_paths_from_source(source, parsed_program);
    for chip_path in &source_chip_paths {
        let resolved = rseq_host::resolve_host_chip_path(chip_path, base_dir);
        if let Err(message) = add_chip_facts(&resolved, &mut facts) {
            diagnostics.push(diagnostic_at_start(
                format!(
                    "failed to load chip metadata {}: {message}",
                    resolved.display()
                ),
                DiagnosticSeverity::WARNING,
            ));
        }
    }
    facts.sort_and_dedup();

    match parse_result {
        Ok(program) => {
            let mut program = program;
            prepend_configured_chips(
                &mut program,
                configured_chips,
                &source_chip_paths,
                base_dir,
                root_dir,
            );
            if let Err(diag) = rseq::compile_program(&program, base_dir) {
                diagnostics.push(Diagnostic {
                    range: byte_range_to_lsp_range(source, diag.span),
                    severity: Some(DiagnosticSeverity::ERROR),
                    source: Some("rseq".to_string()),
                    message: match diag.help {
                        Some(help) => format!("{} ({help})", diag.message),
                        None => diag.message,
                    },
                    ..Default::default()
                });
            }
        }
        Err(errors) => {
            diagnostics.extend(errors.into_iter().map(|diag| Diagnostic {
                range: byte_range_to_lsp_range(source, diag.span),
                severity: Some(DiagnosticSeverity::ERROR),
                source: Some("rseq".to_string()),
                message: diag.message,
                ..Default::default()
            }));
        }
    }

    DocumentAnalysis { diagnostics, facts }
}

pub fn completion_items(
    source: &str,
    position: Position,
    facts: &LanguageFacts,
) -> Vec<CompletionItem> {
    let offset = position_to_byte_offset(source, position);
    completion_items_at_offset(source, offset, facts)
}

pub fn completion_items_at_offset(
    source: &str,
    offset: usize,
    facts: &LanguageFacts,
) -> Vec<CompletionItem> {
    let offset = offset.min(source.len());
    let replace_span = completion_replace_span(source, offset);
    let replace_prefix = &source[replace_span.start..offset];
    let replace_range = byte_range_to_lsp_range(source, replace_span);
    let mut seen = HashSet::new();
    let mut ranked_items = Vec::new();

    for (original_index, seed) in builtin_completion_seeds()
        .into_iter()
        .chain(facts.completion_seeds())
        .enumerate()
    {
        let key = (seed.label.clone(), seed.insert_text.clone());
        if !seen.insert(key) {
            continue;
        }
        let Some(rank) = CompletionRank::for_seed(&seed, replace_prefix, original_index) else {
            continue;
        };
        let mut item = seed.into_completion_item(replace_range);
        item.sort_text = Some(rank.sort_text());
        ranked_items.push((rank, item));
    }

    ranked_items.sort_by(|(left, _), (right, _)| left.cmp(right));
    ranked_items
        .into_iter()
        .map(|(_, item)| item)
        .collect::<Vec<_>>()
}

pub fn hover_at(source: &str, position: Position, facts: &LanguageFacts) -> Option<Hover> {
    let offset = position_to_byte_offset(source, position);
    hover_at_offset(source, offset, facts)
}

pub fn hover_at_offset(source: &str, offset: usize, facts: &LanguageFacts) -> Option<Hover> {
    let token = token_at_offset(source, offset)?;
    let contents = facts.hover_markdown_for_token(&token)?;
    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: contents,
        }),
        range: Some(byte_range_to_lsp_range(source, token.byte_range)),
    })
}

impl LanguageFacts {
    fn sort_and_dedup(&mut self) {
        self.chips.sort_by(|a, b| a.source.cmp(&b.source));
        self.chips.dedup_by(|a, b| a.source == b.source);
        self.pages.sort();
        self.pages.dedup();
        self.registers
            .sort_by(|a, b| a.qualified_name().cmp(&b.qualified_name()));
        self.registers
            .dedup_by(|a, b| a.page == b.page && a.name == b.name && a.addr == b.addr);
        self.fields
            .sort_by(|a, b| a.qualified_name().cmp(&b.qualified_name()));
        self.fields
            .dedup_by(|a, b| a.page == b.page && a.register == b.register && a.name == b.name);
        self.events.sort_by(|a, b| a.name.cmp(&b.name));
        self.events.dedup_by(|a, b| a.name == b.name);
    }

    fn completion_seeds(&self) -> Vec<CompletionSeed> {
        let mut seeds = Vec::new();
        for page in &self.pages {
            seeds.push(CompletionSeed::plain(
                page,
                CompletionItemKind::MODULE,
                "chip register page",
                None,
            ));
        }
        for reg in &self.registers {
            let qualified = reg.qualified_name();
            seeds.push(CompletionSeed::plain(
                &qualified,
                CompletionItemKind::FIELD,
                format!(
                    "0x{:02x}, {}, {} byte(s){}",
                    reg.addr,
                    reg.access,
                    reg.width,
                    if reg.no_dump { ", no_dump" } else { "" }
                ),
                Some(reg.hover_markdown()),
            ));
            seeds.push(CompletionSeed::plain(
                &reg.name,
                CompletionItemKind::FIELD,
                format!("{} at 0x{:02x}", reg.page, reg.addr),
                Some(reg.hover_markdown()),
            ));
        }
        for field in &self.fields {
            seeds.push(CompletionSeed::plain(
                &field.name,
                CompletionItemKind::PROPERTY,
                format!(
                    "{} [{}:{}]",
                    field.qualified_name(),
                    field.bit_hi,
                    field.bit_lo
                ),
                Some(field.hover_markdown()),
            ));
            seeds.push(CompletionSeed::plain(
                &field.qualified_name(),
                CompletionItemKind::PROPERTY,
                format!("[{}:{}]", field.bit_hi, field.bit_lo),
                Some(field.hover_markdown()),
            ));
        }
        for event in &self.events {
            seeds.push(CompletionSeed::plain(
                &event.name,
                CompletionItemKind::EVENT,
                format!(
                    "interrupt event from {}.{}.{}",
                    event.page, event.register, event.field
                ),
                Some(event.hover_markdown()),
            ));
        }
        seeds
    }

    fn hover_markdown_for_token(&self, token: &SourceToken) -> Option<String> {
        self.registers
            .iter()
            .find(|reg| token.text == reg.qualified_name() || token.text == reg.name)
            .map(RegisterFact::hover_markdown)
            .or_else(|| {
                self.fields
                    .iter()
                    .find(|field| token.text == field.qualified_name() || token.text == field.name)
                    .map(FieldFact::hover_markdown)
            })
            .or_else(|| {
                self.events
                    .iter()
                    .find(|event| token.text == event.name)
                    .map(EventFact::hover_markdown)
            })
            .or_else(|| builtin_hover_markdown(&token.text))
    }
}

impl CompletionSeed {
    fn plain(
        label: impl Into<String>,
        kind: CompletionItemKind,
        detail: impl Into<String>,
        documentation: Option<String>,
    ) -> Self {
        let label = label.into();
        Self {
            insert_text: label.clone(),
            label,
            kind,
            detail: detail.into(),
            documentation,
            snippet: false,
        }
    }

    fn snippet(
        label: impl Into<String>,
        insert_text: impl Into<String>,
        kind: CompletionItemKind,
        detail: impl Into<String>,
        documentation: Option<String>,
    ) -> Self {
        Self {
            label: label.into(),
            insert_text: insert_text.into(),
            kind,
            detail: detail.into(),
            documentation,
            snippet: true,
        }
    }

    fn into_completion_item(self, range: Range) -> CompletionItem {
        CompletionItem {
            label: self.label,
            kind: Some(self.kind),
            detail: Some(self.detail),
            documentation: self.documentation.map(|value| {
                Documentation::MarkupContent(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value,
                })
            }),
            insert_text_format: Some(if self.snippet {
                InsertTextFormat::SNIPPET
            } else {
                InsertTextFormat::PLAIN_TEXT
            }),
            text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                range,
                new_text: self.insert_text,
            })),
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone)]
struct SourceToken {
    text: String,
    byte_range: ByteRange<usize>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct CompletionRank {
    bucket: u8,
    label: String,
    original_index: usize,
}

impl CompletionRank {
    fn for_seed(seed: &CompletionSeed, prefix: &str, original_index: usize) -> Option<Self> {
        let prefix = prefix.trim();
        if prefix.is_empty() {
            return Some(Self {
                bucket: 100,
                label: String::new(),
                original_index,
            });
        }

        let query = prefix.to_ascii_lowercase();
        let label = seed.label.to_ascii_lowercase();
        let insert_text = seed.insert_text.to_ascii_lowercase();
        let bucket = if label == query {
            0
        } else if label.starts_with(&query) {
            1
        } else if insert_text.starts_with(&query) {
            2
        } else if !query.contains('.')
            && label
                .split(|ch: char| matches!(ch, '.' | '_' | '!'))
                .any(|segment| segment.starts_with(&query))
        {
            3
        } else if fuzzy_subsequence_match(&label, &query) {
            4
        } else if fuzzy_subsequence_match(&insert_text, &query) {
            5
        } else {
            return None;
        };

        Some(Self {
            bucket,
            label,
            original_index,
        })
    }

    fn sort_text(&self) -> String {
        format!(
            "{:03}:{}:{:06}",
            self.bucket, self.label, self.original_index
        )
    }
}

impl Ord for CompletionRank {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.bucket
            .cmp(&other.bucket)
            .then_with(|| self.label.cmp(&other.label))
            .then_with(|| self.original_index.cmp(&other.original_index))
    }
}

impl PartialOrd for CompletionRank {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

fn fuzzy_subsequence_match(candidate: &str, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }

    let mut query_chars = query.chars();
    let Some(mut needed) = query_chars.next() else {
        return true;
    };
    for ch in candidate.chars() {
        if ch == needed {
            let Some(next) = query_chars.next() else {
                return true;
            };
            needed = next;
        }
    }
    false
}

fn builtin_completion_seeds() -> Vec<CompletionSeed> {
    vec![
        CompletionSeed::snippet(
            "chip!",
            "chip!(\"${1:chip.yaml}\");",
            CompletionItemKind::FUNCTION,
            "load chip YAML metadata",
            Some("Loads register, field, and interrupt-event metadata for compile-time resolution.".to_string()),
        ),
        CompletionSeed::snippet(
            "bus!",
            "bus!(${1|spi,i2c,i3c|});",
            CompletionItemKind::FUNCTION,
            "select the active bus",
            Some("Selects the MCU bus used by following read!/write! operations.".to_string()),
        ),
        CompletionSeed::snippet(
            "bus_probe!",
            "bus_probe!(${1:spi}, { read: ${2:UI.WHOAMI}, expect: ${3:0x06} });",
            CompletionItemKind::FUNCTION,
            "probe a bus using DSL-provided candidates",
            Some("Generic probe command. Chip knowledge stays in DSL/YAML, not MCU firmware.".to_string()),
        ),
        CompletionSeed::snippet(
            "read!",
            "read!(${1:REG}, ${2:1});",
            CompletionItemKind::FUNCTION,
            "read register bytes",
            Some("Can be used as a statement or expression: `let value = read!(REG, len);`.".to_string()),
        ),
        CompletionSeed::snippet(
            "write!",
            "write!(${1:REG}, ${2:[0x00]}, ${3:50});",
            CompletionItemKind::FUNCTION,
            "write register bytes or field map",
            Some("Supports raw bytes and field maps such as `write!(UI.ACTL0, { aodr_ui: 8 }, 50);`.".to_string()),
        ),
        CompletionSeed::snippet(
            "update!",
            "update!(${1:PAGE.REG.FIELD}, ${2:1});",
            CompletionItemKind::FUNCTION,
            "read-modify-write a register field",
            Some("Uses chip YAML field metadata to emit an update operation.".to_string()),
        ),
        CompletionSeed::snippet(
            "irq!",
            "irq!(${1:int1}) {\n    on(${2:event}) {\n        ${0}\n    }\n}",
            CompletionItemKind::FUNCTION,
            "declare an interrupt handler",
            Some("Events inside `on(...)` are loaded from chip YAML field `event` metadata.".to_string()),
        ),
        CompletionSeed::snippet(
            "wait!",
            "wait!(${1:int1});",
            CompletionItemKind::FUNCTION,
            "wait for an interrupt edge",
            Some("Blocks the VM until the selected interrupt pin fires or the timeout expires.".to_string()),
        ),
        CompletionSeed::snippet(
            "repeat!",
            "repeat!(${1:10}) {\n    ${0}\n}",
            CompletionItemKind::FUNCTION,
            "repeat a block",
            None,
        ),
        CompletionSeed::snippet(
            "print!",
            "print!(\"${1:message}\\n\"${0});",
            CompletionItemKind::FUNCTION,
            "emit a VM log message",
            None,
        ),
        CompletionSeed::snippet(
            "report!",
            "report!(${1:FIFO_RAW}, ${2:len}, ${3:data});",
            CompletionItemKind::FUNCTION,
            "emit a structured report",
            Some("Reports travel over the existing rseq-link frame protocol.".to_string()),
        ),
        CompletionSeed::snippet(
            "report_format!",
            "report_format!(${1:FIFO_RAW}, i16_le, {\n    fields: [${2:gx, gy, gz, ax, ay, az}],\n    gyro_fields: [${3:gx, gy, gz}],\n    accel_fields: [${4:ax, ay, az}],\n    accel_fs_g: ${5:16},\n    gyro_fs_dps: ${6:4096},\n    output: ${7|physical_f32,raw_i16|},\n});",
            CompletionItemKind::FUNCTION,
            "host-side report decoder metadata",
            Some("Declares how CLI/TUI/GPUI should decode raw report payloads.".to_string()),
        ),
        CompletionSeed::snippet(
            "let",
            "let ${1:name} = ${2:read!(REG, 1)};",
            CompletionItemKind::KEYWORD,
            "bind a scalar or raw buffer",
            None,
        ),
        CompletionSeed::plain("if", CompletionItemKind::KEYWORD, "conditional block", None),
        CompletionSeed::plain("else", CompletionItemKind::KEYWORD, "conditional fallback", None),
        CompletionSeed::plain("on", CompletionItemKind::KEYWORD, "irq arm selector", None),
        CompletionSeed::plain("spi", CompletionItemKind::VALUE, "bus kind", None),
        CompletionSeed::plain("i2c", CompletionItemKind::VALUE, "bus kind", None),
        CompletionSeed::plain("i3c", CompletionItemKind::VALUE, "bus kind", None),
        CompletionSeed::plain("FIFO_RAW", CompletionItemKind::CONSTANT, "report kind 0x01", None),
        CompletionSeed::plain("AMD", CompletionItemKind::CONSTANT, "report kind 0x02", None),
        CompletionSeed::plain("SMD", CompletionItemKind::CONSTANT, "report kind 0x03", None),
        CompletionSeed::plain("DRDY", CompletionItemKind::CONSTANT, "report kind 0x04", None),
        CompletionSeed::plain("i16_le", CompletionItemKind::VALUE, "report decoder", None),
        CompletionSeed::plain("qmi8660_fifo6", CompletionItemKind::VALUE, "legacy report decoder", None),
        CompletionSeed::plain("fields", CompletionItemKind::PROPERTY, "report_format option", None),
        CompletionSeed::plain("gyro_fields", CompletionItemKind::PROPERTY, "report_format option", None),
        CompletionSeed::plain("accel_fields", CompletionItemKind::PROPERTY, "report_format option", None),
        CompletionSeed::plain("temp_field", CompletionItemKind::PROPERTY, "report_format option", None),
        CompletionSeed::plain("accel_fs_g", CompletionItemKind::PROPERTY, "report_format option", None),
        CompletionSeed::plain("gyro_fs_dps", CompletionItemKind::PROPERTY, "report_format option", None),
        CompletionSeed::plain("temp_lsb_per_c", CompletionItemKind::PROPERTY, "report_format option", None),
        CompletionSeed::plain("temp_offset_c", CompletionItemKind::PROPERTY, "report_format option", None),
        CompletionSeed::plain("output", CompletionItemKind::PROPERTY, "report_format option", None),
        CompletionSeed::plain("physical_f32", CompletionItemKind::VALUE, "decoded physical units", None),
        CompletionSeed::plain("raw_i16", CompletionItemKind::VALUE, "raw i16 sample values", None),
    ]
}

fn builtin_hover_markdown(token: &str) -> Option<String> {
    match token {
        "read!" => Some("**read!**\n\n`read!(REG, len[, delay_us])` reads bytes from the active bus. In `let x = read!(...)`, lengths up to 4 bytes become scalar variables; dynamic FIFO reads can bind one raw buffer.".to_string()),
        "write!" => Some("**write!**\n\n`write!(REG, value[, delay_us])` writes raw bytes, scalar variables, or YAML-backed field maps.".to_string()),
        "report!" => Some("**report!**\n\n`report!(kind, args...)` sends a structured event through rseq-link. Built-in kinds include `FIFO_RAW`, `AMD`, `SMD`, and `DRDY`.".to_string()),
        "report_format!" => Some("**report_format!**\n\nHost-side metadata for decoding reports in CLI/TUI/GPUI. It emits no MCU bytecode.".to_string()),
        "irq!" => Some("**irq!**\n\nDeclares interrupt event arms. Event names come from chip YAML field `event` metadata.".to_string()),
        "bus!" => Some("**bus!**\n\nSelects `spi`, `i2c`, or `i3c` as the active bus.".to_string()),
        _ => None,
    }
}

fn latest_full_text(changes: Vec<TextDocumentContentChangeEvent>) -> Option<String> {
    changes
        .into_iter()
        .rev()
        .find(|change| change.range.is_none())
        .map(|change| change.text)
}

fn uri_to_dir(uri: &Url) -> Option<PathBuf> {
    uri.to_file_path().ok().and_then(|path| {
        if path.is_dir() {
            Some(path)
        } else {
            path.parent().map(Path::to_path_buf)
        }
    })
}

fn document_base_dir(uri: &Url) -> Option<PathBuf> {
    uri.to_file_path()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
}

fn initialization_chip_paths(value: Option<&serde_json::Value>) -> Vec<PathBuf> {
    let Some(value) = value else {
        return Vec::new();
    };
    let mut chips = Vec::new();
    if let Some(chip) = value.get("chip").and_then(|value| value.as_str()) {
        chips.push(PathBuf::from(chip));
    }
    if let Some(values) = value.get("chips").and_then(|value| value.as_array()) {
        for value in values {
            if let Some(chip) = value.as_str() {
                chips.push(PathBuf::from(chip));
            }
        }
    }
    chips
}

fn resolve_configured_chip_path(
    path: &Path,
    base_dir: Option<&Path>,
    root_dir: Option<&Path>,
) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    if let Some(root_dir) = root_dir {
        let candidate = root_dir.join(path);
        if candidate.exists() {
            return candidate;
        }
    }
    if let Some(base_dir) = base_dir {
        let candidate = base_dir.join(path);
        if candidate.exists() {
            return candidate;
        }
    }
    rseq_host::resolve_host_chip_path(&path.to_string_lossy(), base_dir)
}

fn add_chip_facts(path: &Path, facts: &mut LanguageFacts) -> std::result::Result<(), String> {
    let registry = rseq::ChipRegistry::load(path).map_err(|err| err.to_string())?;
    for chip in registry.chips() {
        facts.chips.push(ChipFact {
            sensor: chip.sensor.clone(),
            source: chip.source.clone(),
        });
        for page in &chip.pages {
            facts.pages.push(page.name.clone());
            for reg in &page.registers {
                let fields = reg
                    .fields
                    .iter()
                    .map(|field| FieldFact {
                        page: page.name.clone(),
                        register: reg.name.clone(),
                        name: field.name.clone(),
                        bit_hi: field.bit_hi,
                        bit_lo: field.bit_lo,
                        desc: field.desc.clone(),
                        event: field.event.clone(),
                    })
                    .collect::<Vec<_>>();
                for field in &fields {
                    facts.fields.push(field.clone());
                    if let Some(event) = &field.event {
                        facts.events.push(EventFact {
                            name: event.clone(),
                            page: field.page.clone(),
                            register: field.register.clone(),
                            field: field.name.clone(),
                            desc: field.desc.clone(),
                        });
                    }
                }
                facts.registers.push(RegisterFact {
                    page: page.name.clone(),
                    name: reg.name.clone(),
                    addr: reg.addr,
                    access: reg.access.clone(),
                    width: reg.width,
                    desc: reg.desc.clone(),
                    no_dump: reg.no_dump,
                    fields,
                });
            }
        }
    }
    Ok(())
}

fn prepend_configured_chips(
    program: &mut rseq::Program,
    configured_chips: &[PathBuf],
    source_chip_paths: &[String],
    base_dir: Option<&Path>,
    root_dir: Option<&Path>,
) {
    let source_paths = source_chip_paths
        .iter()
        .map(|path| canonical_or_same(rseq_host::resolve_host_chip_path(path, base_dir)))
        .collect::<HashSet<_>>();

    let mut extra = Vec::new();
    for chip_path in configured_chips {
        let resolved =
            canonical_or_same(resolve_configured_chip_path(chip_path, base_dir, root_dir));
        if source_paths.contains(&resolved) {
            continue;
        }
        extra.push(rseq::Stmt::Chip {
            path: resolved.to_string_lossy().into_owned(),
        });
    }
    if extra.is_empty() {
        return;
    }

    let mut stmts = extra;
    stmts.append(&mut program.stmts);
    let mut spans = vec![0..0; stmts.len() - program.stmt_spans.len()];
    spans.append(&mut program.stmt_spans);
    program.stmts = stmts;
    program.stmt_spans = spans;
}

fn canonical_or_same(path: PathBuf) -> PathBuf {
    std::fs::canonicalize(&path).unwrap_or(path)
}

fn chip_paths_from_source(source: &str, parsed: Option<&rseq::Program>) -> Vec<String> {
    if let Some(program) = parsed {
        let mut paths = Vec::new();
        collect_chip_paths(&program.stmts, &mut paths);
        if !paths.is_empty() {
            return paths;
        }
    }
    extract_chip_paths_textually(source)
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

fn extract_chip_paths_textually(source: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut rest = source;
    while let Some(idx) = rest.find("chip!") {
        rest = &rest[idx + "chip!".len()..];
        let Some(open) = rest.find('(') else {
            break;
        };
        rest = &rest[open + 1..];
        let Some(close) = rest.find(')') else {
            break;
        };
        let raw = rest[..close].trim();
        let trimmed = raw.trim_matches('"').trim_matches('\'').trim();
        if !trimmed.is_empty() {
            paths.push(trimmed.to_string());
        }
        rest = &rest[close + 1..];
    }
    paths
}

fn diagnostic_at_start(message: String, severity: DiagnosticSeverity) -> Diagnostic {
    Diagnostic {
        range: Range::new(Position::new(0, 0), Position::new(0, 0)),
        severity: Some(severity),
        source: Some("rseq-lsp".to_string()),
        message,
        ..Default::default()
    }
}

fn token_at_offset(source: &str, offset: usize) -> Option<SourceToken> {
    let span = completion_replace_span(source, offset.min(source.len()));
    if span.start == span.end {
        return None;
    }
    Some(SourceToken {
        text: source[span.clone()].to_string(),
        byte_range: span,
    })
}

fn completion_replace_span(source: &str, offset: usize) -> ByteRange<usize> {
    let mut start = offset.min(source.len());
    while start > 0 {
        let mut prev = source[..start].char_indices();
        let Some((idx, ch)) = prev.next_back() else {
            break;
        };
        if is_completion_char(ch) {
            start = idx;
        } else {
            break;
        }
    }

    let mut end = offset.min(source.len());
    while end < source.len() {
        let Some(ch) = source[end..].chars().next() else {
            break;
        };
        if is_completion_char(ch) {
            end += ch.len_utf8();
        } else {
            break;
        }
    }

    start..end
}

fn is_completion_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_' || ch == '.' || ch == '!'
}

fn position_to_byte_offset(source: &str, position: Position) -> usize {
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
    let line = &source[line_start..line_end];
    line_start + utf16_col_to_byte(line, position.character)
}

fn utf16_col_to_byte(line: &str, target_col: u32) -> usize {
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

fn byte_range_to_lsp_range(source: &str, span: ByteRange<usize>) -> Range {
    Range::new(
        byte_offset_to_position(source, span.start.min(source.len())),
        byte_offset_to_position(source, span.end.min(source.len())),
    )
}

fn byte_offset_to_position(source: &str, offset: usize) -> Position {
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
            character += ch.len_utf16() as u32;
        }
    }
    Position::new(line, character)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf()
    }

    fn qmi_chip() -> PathBuf {
        repo_root().join("qmi8660.yaml")
    }

    #[test]
    fn completions_include_builtins_and_chip_yaml_symbols() {
        let source = "";
        let analysis = analyze_document(
            source,
            Some(&repo_root()),
            Some(&repo_root()),
            &[qmi_chip()],
        );

        let items = completion_items(
            source,
            Position::new(0, source.len() as u32),
            &analysis.facts,
        );
        let labels = items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<HashSet<_>>();

        assert!(labels.contains("read!"));
        assert!(labels.contains("UI.WHOAMI"));
        assert!(labels.contains("UI.FIFO_DATA"));
        assert!(labels.contains("fifo_watermark"));
    }

    #[test]
    fn completions_filter_and_rank_current_prefix() {
        let source = "read!(UI.FI";
        let analysis = analyze_document(
            source,
            Some(&repo_root()),
            Some(&repo_root()),
            &[qmi_chip()],
        );
        let items = completion_items(
            source,
            Position::new(0, source.len() as u32),
            &analysis.facts,
        );
        let labels = items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>();

        assert!(!labels.is_empty());
        assert!(labels[0].starts_with("UI.FI"));
        assert!(labels.contains(&"UI.FIFO_DATA"));
        assert!(!labels.contains(&"chip!"));
        assert!(!labels.contains(&"read!"));
    }

    #[test]
    fn completions_prefer_direct_prefix_over_segment_match() {
        let source = "read!(FI";
        let analysis = analyze_document(
            source,
            Some(&repo_root()),
            Some(&repo_root()),
            &[qmi_chip()],
        );
        let items = completion_items(
            source,
            Position::new(0, source.len() as u32),
            &analysis.facts,
        );
        let labels = items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>();
        let unqualified = labels
            .iter()
            .position(|label| *label == "FIFO_DATA")
            .expect("FIFO_DATA completion");
        let qualified = labels
            .iter()
            .position(|label| *label == "UI.FIFO_DATA")
            .expect("UI.FIFO_DATA completion");

        assert!(unqualified < qualified);
    }

    #[test]
    fn configured_chip_makes_register_source_compile_without_chip_stmt() {
        let source = "let whoami = read!(UI.WHOAMI, 1);";
        let analysis = analyze_document(
            source,
            Some(&repo_root()),
            Some(&repo_root()),
            &[qmi_chip()],
        );
        assert!(
            analysis.diagnostics.is_empty(),
            "unexpected diagnostics: {:?}",
            analysis.diagnostics
        );
    }

    #[test]
    fn parse_errors_become_lsp_diagnostics() {
        let source = "write!(UI.WHOAMI, [0x00]";
        let analysis = analyze_document(
            source,
            Some(&repo_root()),
            Some(&repo_root()),
            &[qmi_chip()],
        );
        assert!(!analysis.diagnostics.is_empty());
        assert_eq!(
            analysis.diagnostics[0].severity,
            Some(DiagnosticSeverity::ERROR)
        );
    }

    #[test]
    fn hover_describes_chip_registers() {
        let source = "read!(UI.WHOAMI, 1);";
        let analysis = analyze_document(
            source,
            Some(&repo_root()),
            Some(&repo_root()),
            &[qmi_chip()],
        );
        let hover = hover_at(source, Position::new(0, 9), &analysis.facts).expect("hover");
        let HoverContents::Markup(markup) = hover.contents else {
            panic!("expected markup hover");
        };
        assert!(markup.value.contains("UI.WHOAMI"));
        assert!(markup.value.contains("0x02"));
    }

    #[test]
    fn utf16_positions_survive_non_ascii_comments() {
        let source = "// 中文\nread!(UI.WHOAMI, 1);";
        let offset = position_to_byte_offset(source, Position::new(1, 9));
        assert_eq!(&source[offset..offset + "WHOAMI".len()], "WHOAMI");
    }
}
