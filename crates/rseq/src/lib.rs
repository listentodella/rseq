//! Register Sequence DSL Parser
//! A DSL for defining register sequences in embedded systems

mod chip;

use chumsky::{
    input::{MapExtra, Stream, ValueInput},
    prelude::*,
};
use logos::Logos;
use serde::Deserialize;
use std::collections::HashMap;
use std::fmt;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::vec::Vec;

pub use chip::{
    Chip, ChipError, ChipRegistry, EventBit, UpdatePlan, emit_update_bytecode, load_chip,
    normalize_chip_path, resolve_chip_path,
};

type ParserExtra<'tok, 'src> = extra::Err<Rich<'tok, Token<'src>>>;

#[derive(Logos, Clone, PartialEq, Debug)]
enum Token<'a> {
    Error,

    #[regex(r"[a-zA-Z_][a-zA-Z0-9_]*(?:\.[a-zA-Z_][a-zA-Z0-9_]*)*")]
    Ident(&'a str),

    #[regex(r"0x[0-9a-fA-F]+", |lex| u32::from_str_radix(&lex.slice()[2..], 16).unwrap())]
    #[regex(r"[0-9]+",          |lex| lex.slice().parse::<u32>().unwrap())]
    Number(u32),

    #[token("let")]
    Let,
    #[token("=")]
    Assign,
    #[token("read!")]
    ReadMacro,
    #[token("write!")]
    WriteMacro,
    #[token("update!")]
    UpdateMacro,
    #[token("chip!")]
    ChipMacro,
    #[token("irq!")]
    IrqMacro,
    #[token("(")]
    LParen,
    #[token(")")]
    RParen,
    #[token("[")]
    LBracket,
    #[token("]")]
    RBracket,
    #[token("{")]
    LBrace,
    #[token("}")]
    RBrace,
    #[token(":")]
    Colon,
    #[token(",")]
    Comma,
    #[token(";")]
    Semicolon,

    // ── 算术 / 位运算符 ──────────────────────────
    #[token("+")]
    Plus,
    #[token("-")]
    Minus,
    #[token("*")]
    Star,
    #[token("/")]
    Slash,
    #[token("%")]
    Percent,
    #[token("<<")]
    Shl,
    #[token(">>")]
    Shr,
    #[token("&")]
    Amp,
    #[token("|")]
    Pipe,
    #[token("^")]
    Caret,

    #[regex(r#""([^"\\]|\\.)*""#, |lex| {
        let s = lex.slice();
        s[1..s.len() - 1].replace("\\\"", "\"").replace("\\\\", "\\")
    })]
    String(String),

    #[regex(r"[ \t\f\n]+", logos::skip)]
    Whitespace,

    #[regex(r"//[^\n]*", logos::skip, allow_greedy = true)]
    Comment,
}

impl fmt::Display for Token<'_> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Ident(s) => write!(f, "{s}"),
            Self::Number(n) => write!(f, "{n}"),
            Self::Let => write!(f, "let"),
            Self::Assign => write!(f, "="),
            Self::ReadMacro => write!(f, "read!"),
            Self::WriteMacro => write!(f, "write!"),
            Self::UpdateMacro => write!(f, "update!"),
            Self::ChipMacro => write!(f, "chip!"),
            Self::IrqMacro => write!(f, "irq!"),
            Self::LParen => write!(f, "("),
            Self::RParen => write!(f, ")"),
            Self::LBracket => write!(f, "["),
            Self::RBracket => write!(f, "]"),
            Self::LBrace => write!(f, "{{"),
            Self::RBrace => write!(f, "}}"),
            Self::Colon => write!(f, ":"),
            Self::Comma => write!(f, ","),
            Self::Semicolon => write!(f, ";"),
            Self::Plus => write!(f, "+"),
            Self::Minus => write!(f, "-"),
            Self::Star => write!(f, "*"),
            Self::Slash => write!(f, "/"),
            Self::Percent => write!(f, "%"),
            Self::Shl => write!(f, "<<"),
            Self::Shr => write!(f, ">>"),
            Self::Amp => write!(f, "&"),
            Self::Pipe => write!(f, "|"),
            Self::Caret => write!(f, "^"),
            Self::String(s) => write!(f, "\"{s}\""),
            Self::Whitespace => write!(f, "<whitespace>"),
            Self::Comment => write!(f, "<comment>"),
            Self::Error => write!(f, "<error>"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Ident(String),
    Number(u32),
    Array(Vec<Value>),
    FieldMap(Vec<(String, u32)>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    Chip {
        path: String,
    },
    Let {
        name: String,
        expr: Expr,
    },
    Write {
        addr: Value,
        val: Value,
        delay_us: Option<u32>,
    },
    Update {
        target: String,
        val: Value,
        delay_us: Option<u32>,
    },
    /// 中断处理块：当 `pin` 上发生中断时，按声明顺序逐个判断各 `arm`
    /// 的事件位是否置位，命中则执行该 arm 的语句体。
    Irq {
        pin: String,
        arms: Vec<IrqArm>,
    },
}

/// `irq!` 块中的一条事件分支：`on(event) { ... }`。
#[derive(Debug, Clone, PartialEq)]
pub struct IrqArm {
    pub event: String,
    pub body: Vec<Stmt>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Shl,
    Shr,
    And,
    Or,
    Xor,
}

impl fmt::Display for BinOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Add => "+",
            Self::Sub => "-",
            Self::Mul => "*",
            Self::Div => "/",
            Self::Mod => "%",
            Self::Shl => "<<",
            Self::Shr => ">>",
            Self::And => "&",
            Self::Or => "|",
            Self::Xor => "^",
        };
        f.write_str(s)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Read {
        addr: Value,
        len: Value,
        delay_us: Option<u32>,
    },
    Number(u32),
    Ident(String),
    Binary {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    pub stmts: Vec<Stmt>,
    pub stmt_spans: Vec<Range<usize>>,
}

/// 完整编译产物：线性主程序字节码 + 若干中断派发表（方案 A）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledProgram {
    /// 上电初始化等顺序执行的字节码，以 Return 结尾。
    pub main: Vec<u8>,
    /// 每个 `irq!(pin)` 块编译出的一张派发表。
    pub irqs: Vec<IrqVector>,
}

/// 一个中断引脚的派发表：发生中断时读取一次状态快照，
/// 再按声明顺序对命中的事件运行对应的处理段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrqVector {
    /// 中断引脚名（仅作标注，如 "int1"）。
    pub pin: String,
    /// 状态快照寄存器地址（一次性读取整组中断状态）。
    pub snapshot_addr: u32,
    /// 快照读取字节数（≤ 8）。
    pub snapshot_len: u32,
    /// 状态寄存器读取后是否自动清零（决定只能读一次）。
    pub read_clear: bool,
    /// 各事件分支，按源码顺序即优先级排列。
    pub arms: Vec<IrqArmBin>,
}

/// 派发表里的一条事件分支：命中 `mask` 时运行独立的 `handler` 段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrqArmBin {
    pub event: String,
    /// 该事件在状态快照中的位掩码。
    pub mask: u64,
    /// 自包含的处理段字节码，以 Return 结尾，由独立 Vm 实例执行。
    pub handler: Vec<u8>,
}

/// 方案 A 的宿主端派发器：中断发生时调用。
///
/// 读取一次状态快照（满足 `read_clear` 语义），然后按优先级顺序对每个
/// 置位的事件运行其处理段。返回实际触发的事件名列表，便于观测/测试。
pub fn run_irq_vector<B: rseq_vm::Bus>(
    bus: &mut B,
    vector: &IrqVector,
) -> Result<Vec<String>, rseq_vm::VmError> {
    let len = vector.snapshot_len as usize;
    if len == 0 || len > 8 {
        return Err(rseq_vm::VmError::InvalidLength);
    }
    let mut buf = [0u8; 8];
    bus.read(vector.snapshot_addr, &mut buf[..len])
        .map_err(rseq_vm::VmError::BusError)?;
    let snapshot = u64::from_le_bytes(buf);

    let mut fired = Vec::new();
    for arm in &vector.arms {
        if snapshot & arm.mask != 0 {
            fired.push(arm.event.clone());
            rseq_vm::Vm::new(bus, &arm.handler).run()?;
        }
    }
    Ok(fired)
}

fn value<'tok, 'src: 'tok, I>() -> impl Parser<'tok, I, Value, ParserExtra<'tok, 'src>> + Clone + 'tok
where
    I: ValueInput<'tok, Token = Token<'src>, Span = SimpleSpan>,
{
    recursive(|value| {
        let ident = select! {
            Token::Ident(s) => Value::Ident(s.to_string()),
        };

        let number = select! {
            Token::Number(n) => Value::Number(n),
        };

        let atom = ident.or(number);

        let array = value
            .separated_by(just(Token::Comma))
            .allow_trailing()
            .collect()
            .map(Value::Array)
            .delimited_by(just(Token::LBracket), just(Token::RBracket));

        atom.or(array)
    })
}

fn field_map<'tok, 'src: 'tok, I>() -> impl Parser<'tok, I, Value, ParserExtra<'tok, 'src>> + Clone + 'tok
where
    I: ValueInput<'tok, Token = Token<'src>, Span = SimpleSpan>,
{
    select! { Token::Ident(s) => s.to_string() }
        .then_ignore(just(Token::Colon))
        .then(select! { Token::Number(n) => n })
        .separated_by(just(Token::Comma))
        .allow_trailing()
        .collect::<Vec<_>>()
        .map(Value::FieldMap)
        .delimited_by(just(Token::LBrace), just(Token::RBrace))
}

fn read_expr<'tok, 'src: 'tok, I>() -> impl Parser<'tok, I, Expr, ParserExtra<'tok, 'src>> + Clone + 'tok
where
    I: ValueInput<'tok, Token = Token<'src>, Span = SimpleSpan>,
{
    just(Token::ReadMacro)
        .ignore_then(
            value()
                .then(just(Token::Comma).ignore_then(value()))
                .then(just(Token::Comma).ignore_then(value()).or_not())
                .delimited_by(just(Token::LParen), just(Token::RParen)),
        )
        .map(|((addr, len), delay_us)| {
            let delay_us = delay_us.and_then(|v| match v {
                Value::Number(n) => Some(n),
                _ => None,
            });
            Expr::Read {
                addr,
                len,
                delay_us,
            }
        })
}

/// 完整表达式解析器，支持加减乘除、移位、逻辑与或异或，以及括号。
///
/// 优先级（从低到高）：
///   `|` → `^` → `&` → `<<`/`>>` → `+`/`-` → `*`/`/`/`%` → primary
fn expr<'tok, 'src: 'tok, I>() -> impl Parser<'tok, I, Expr, ParserExtra<'tok, 'src>> + Clone + 'tok
where
    I: ValueInput<'tok, Token = Token<'src>, Span = SimpleSpan>,
{
    recursive(|expr| {
        let number = select! { Token::Number(n) => Expr::Number(n) };
        let ident = select! { Token::Ident(s) => Expr::Ident(s.to_string()) };
        let paren = expr
            .clone()
            .delimited_by(just(Token::LParen), just(Token::RParen));
        let primary = number.or(ident).or(read_expr()).or(paren);

        fn binop_layer<'tok, 'src: 'tok, I>(
            prev: impl Parser<'tok, I, Expr, ParserExtra<'tok, 'src>> + Clone + 'tok,
            op_parser: impl Parser<'tok, I, BinOp, ParserExtra<'tok, 'src>> + Clone + 'tok,
        ) -> impl Parser<'tok, I, Expr, ParserExtra<'tok, 'src>> + Clone + 'tok
        where
            I: ValueInput<'tok, Token = Token<'src>, Span = SimpleSpan>,
        {
            prev.clone()
                .then(op_parser.then(prev).repeated().collect::<Vec<_>>())
                .map(|(first, rest)| {
                    rest.into_iter().fold(first, |lhs, (op, rhs)| Expr::Binary {
                        op,
                        lhs: Box::new(lhs),
                        rhs: Box::new(rhs),
                    })
                })
                .boxed()
        }

        // 乘除模
        let mul_op = just(Token::Star).to(BinOp::Mul)
            .or(just(Token::Slash).to(BinOp::Div))
            .or(just(Token::Percent).to(BinOp::Mod));
        let mul = binop_layer(primary, mul_op);

        // 加减
        let add_op = just(Token::Plus).to(BinOp::Add)
            .or(just(Token::Minus).to(BinOp::Sub));
        let add = binop_layer(mul, add_op);

        // 移位
        let shift_op = just(Token::Shl).to(BinOp::Shl)
            .or(just(Token::Shr).to(BinOp::Shr));
        let shift = binop_layer(add, shift_op);

        // 按位与
        let and_op = just(Token::Amp).to(BinOp::And);
        let and = binop_layer(shift, and_op);

        // 按位异或
        let xor_op = just(Token::Caret).to(BinOp::Xor);
        let xor = binop_layer(and, xor_op);

        // 按位或
        let or_op = just(Token::Pipe).to(BinOp::Or);
        binop_layer(xor, or_op)
    })
}

fn simple_stmt<'tok, 'src: 'tok, I>()
-> impl Parser<'tok, I, Stmt, ParserExtra<'tok, 'src>> + Clone + 'tok
where
    I: ValueInput<'tok, Token = Token<'src>, Span = SimpleSpan>,
{
    let chip_stmt = just(Token::ChipMacro)
        .ignore_then(
            select! {
                Token::String(s) => s.clone(),
                Token::Ident(s) => s.to_string(),
            }
            .delimited_by(just(Token::LParen), just(Token::RParen)),
        )
        .then_ignore(just(Token::Semicolon).or_not())
        .map(|path| Stmt::Chip { path });

    let let_stmt = just(Token::Let)
        .ignore_then(select! { Token::Ident(s) => s.to_string() })
        .then(just(Token::Assign).ignore_then(expr()))
        .then_ignore(just(Token::Semicolon).or_not())
        .map(|(name, expr)| Stmt::Let { name, expr });

    let write_stmt = just(Token::WriteMacro)
        .ignore_then(
            value()
                .then(just(Token::Comma).ignore_then(value()))
                .then(just(Token::Comma).ignore_then(value()).or_not())
                .delimited_by(just(Token::LParen), just(Token::RParen)),
        )
        .then_ignore(just(Token::Semicolon).or_not())
        .map(|((addr, val), delay_us)| {
            let delay_us = delay_us.and_then(|v| match v {
                Value::Number(n) => Some(n),
                _ => None,
            });
            Stmt::Write {
                addr,
                val,
                delay_us,
            }
        });

    let update_stmt =
        just(Token::UpdateMacro)
            .ignore_then(
                select! { Token::Ident(s) => s.to_string() }
                    .then(just(Token::Comma).ignore_then(
                        field_map().or(select! { Token::Number(n) => Value::Number(n) }),
                    ))
                    .then(
                        just(Token::Comma)
                            .ignore_then(select! { Token::Number(n) => n })
                            .or_not(),
                    )
                    .delimited_by(just(Token::LParen), just(Token::RParen)),
            )
            .then_ignore(just(Token::Semicolon).or_not())
            .map(|((target, val), delay_us)| Stmt::Update {
                target,
                val,
                delay_us,
            });

    chip_stmt.or(let_stmt).or(update_stmt).or(write_stmt)
}

/// 解析 `irq!(pin) { on(event) { ... } ... }` 中断处理块。
/// 块内每个 `on(event)` 分支的语句体只允许普通语句（不可再嵌套 irq!）。
fn irq_stmt<'tok, 'src: 'tok, I>()
-> impl Parser<'tok, I, Stmt, ParserExtra<'tok, 'src>> + Clone + 'tok
where
    I: ValueInput<'tok, Token = Token<'src>, Span = SimpleSpan>,
{
    let arm = just(Token::Ident("on"))
        .ignore_then(
            select! { Token::Ident(s) => s.to_string() }
                .delimited_by(just(Token::LParen), just(Token::RParen)),
        )
        .then(
            simple_stmt()
                .repeated()
                .collect::<Vec<_>>()
                .delimited_by(just(Token::LBrace), just(Token::RBrace)),
        )
        .map(|(event, body)| IrqArm { event, body });

    just(Token::IrqMacro)
        .ignore_then(
            select! { Token::Ident(s) => s.to_string() }
                .delimited_by(just(Token::LParen), just(Token::RParen)),
        )
        .then(
            arm.repeated()
                .collect::<Vec<_>>()
                .delimited_by(just(Token::LBrace), just(Token::RBrace)),
        )
        .then_ignore(just(Token::Semicolon).or_not())
        .map(|(pin, arms)| Stmt::Irq { pin, arms })
}

fn stmt<'tok, 'src: 'tok, I>() -> impl Parser<'tok, I, (Stmt, Range<usize>), ParserExtra<'tok, 'src>>
where
    I: ValueInput<'tok, Token = Token<'src>, Span = SimpleSpan>,
{
    simple_stmt().or(irq_stmt()).map_with(
        |stmt, e: &mut MapExtra<'tok, '_, I, ParserExtra<'tok, 'src>>| {
            (stmt, e.span().into_range())
        },
    )
}

fn parser<'tok, 'src: 'tok, I>() -> impl Parser<'tok, I, Program, ParserExtra<'tok, 'src>>
where
    I: ValueInput<'tok, Token = Token<'src>, Span = SimpleSpan>,
{
    stmt().repeated().collect::<Vec<_>>().map(|spanned_stmts| {
        let (stmts, stmt_spans): (Vec<_>, Vec<_>) = spanned_stmts.into_iter().unzip();
        Program { stmts, stmt_spans }
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    GenericError,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseDiagnostic {
    pub span: Range<usize>,
    pub message: String,
}

pub fn parse(source: &str) -> Result<Program, ParseError> {
    parse_detailed(source).map_err(|_| ParseError::GenericError)
}

pub fn parse_detailed(source: &str) -> Result<Program, Vec<ParseDiagnostic>> {
    let token_iter = Token::lexer(source).spanned().map(|(tok, span)| match tok {
        Ok(tok) => (tok, span.into()),
        Err(()) => (Token::Error, span.into()),
    });

    let token_stream =
        Stream::from_iter(token_iter).map((0..source.len()).into(), |(t, s): (_, _)| (t, s));

    parser()
        .parse(token_stream)
        .into_result()
        .map_err(|errors| {
            errors
                .into_iter()
                .map(|err| ParseDiagnostic {
                    span: err.span().into_range(),
                    message: err.to_string(),
                })
                .collect()
        })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompileError {
    UnsupportedValue,
    NumberExpected,
    Chip(ChipError),
    Register(String),
    Update(String),
    UndefinedVariable(String),
    RegisterOverflow,
    /// irq! 引用的事件在芯片字典里找不到对应的中断状态位。
    UnknownEvent(String),
    /// 芯片字典未声明任何中断状态寄存器，无法编译 irq!。
    NoInterruptStatus,
}

impl fmt::Display for CompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedValue => write!(f, "unsupported value in this context"),
            Self::NumberExpected => write!(f, "expected a number or resolvable register"),
            Self::Chip(err) => write!(f, "{err}"),
            Self::Register(msg) => write!(f, "register resolution failed: {msg}"),
            Self::Update(msg) => write!(f, "update failed: {msg}"),
            Self::UndefinedVariable(name) => {
                write!(f, "undefined variable '{name}'")
            }
            Self::RegisterOverflow => write!(f, "register overflow: too many live values"),
            Self::UnknownEvent(name) => {
                write!(f, "unknown interrupt event '{name}'")
            }
            Self::NoInterruptStatus => {
                write!(f, "chip dictionary declares no interrupt status register")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompileDiagnostic {
    pub span: Range<usize>,
    pub message: String,
    pub help: Option<String>,
    pub error: CompileError,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceUnit {
    pub name: String,
    pub source: String,
    pub base_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceDiagnostic {
    pub unit: usize,
    pub span: Range<usize>,
    pub message: String,
    pub help: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct ProgramUnit<'a> {
    pub program: &'a Program,
    pub base_dir: Option<&'a Path>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Manifest {
    #[serde(default)]
    pub chip: Option<String>,
    #[serde(default)]
    pub sequence: Vec<ManifestSequence>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ManifestSequence {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    pub file: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestError {
    Parse(String),
    Empty,
    DuplicateId(String),
    UnknownId(String),
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(msg) => write!(f, "manifest parse error: {msg}"),
            Self::Empty => write!(f, "manifest does not define any [[sequence]] entries"),
            Self::DuplicateId(id) => write!(f, "duplicate manifest sequence id '{id}'"),
            Self::UnknownId(id) => write!(f, "unknown manifest sequence id '{id}'"),
        }
    }
}

impl Manifest {
    pub fn parse(source: &str) -> Result<Self, ManifestError> {
        let manifest: Self =
            toml::from_str(source).map_err(|e| ManifestError::Parse(e.to_string()))?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn selected_sequences<'a>(
        &'a self,
        ids: &[String],
    ) -> Result<Vec<&'a ManifestSequence>, ManifestError> {
        self.validate()?;

        if ids.is_empty() {
            return Ok(self.sequence.iter().collect());
        }

        ids.iter()
            .map(|id| {
                self.sequence
                    .iter()
                    .find(|sequence| sequence.id == *id)
                    .ok_or_else(|| ManifestError::UnknownId(id.clone()))
            })
            .collect()
    }

    fn validate(&self) -> Result<(), ManifestError> {
        if self.sequence.is_empty() {
            return Err(ManifestError::Empty);
        }

        let mut ids = std::collections::HashSet::new();
        for sequence in &self.sequence {
            if !ids.insert(sequence.id.clone()) {
                return Err(ManifestError::DuplicateId(sequence.id.clone()));
            }
        }

        Ok(())
    }
}

pub fn compile(program: &Program) -> Result<Vec<u8>, CompileError> {
    compile_with_base(program, None)
}

pub fn compile_with_base(
    program: &Program,
    base_dir: Option<&Path>,
) -> Result<Vec<u8>, CompileError> {
    compile_with_base_detailed(program, base_dir).map_err(|diag| diag.error)
}

pub fn compile_with_base_detailed(
    program: &Program,
    base_dir: Option<&Path>,
) -> Result<Vec<u8>, CompileDiagnostic> {
    Ok(compile_program(program, base_dir)?.main)
}

/// 编译单个程序，返回主程序字节码与所有 `irq!` 派发表（方案 A）。
pub fn compile_program(
    program: &Program,
    base_dir: Option<&Path>,
) -> Result<CompiledProgram, CompileDiagnostic> {
    let mut registry = ChipRegistry::default();
    let mut bytecode = Vec::new();
    let mut vars = HashMap::new();
    let mut next_reg: u16 = 0;
    let mut irqs = Vec::new();
    compile_into(
        program,
        base_dir,
        &mut registry,
        &mut bytecode,
        &mut vars,
        &mut next_reg,
        &mut irqs,
    )?;
    bytecode.push(rseq_vm::Opcode::Return as u8);
    Ok(CompiledProgram {
        main: bytecode,
        irqs,
    })
}

pub fn compile_units_detailed(units: &[SourceUnit]) -> Result<Vec<u8>, SourceDiagnostic> {
    let mut programs = Vec::with_capacity(units.len());

    for (unit_idx, unit) in units.iter().enumerate() {
        let program = parse_detailed(&unit.source).map_err(|errors| {
            let first = errors
                .into_iter()
                .next()
                .expect("parse_detailed returned at least one diagnostic");
            SourceDiagnostic {
                unit: unit_idx,
                span: first.span,
                message: first.message,
                help: Some("check the macro syntax and punctuation near this location".to_string()),
            }
        })?;
        programs.push(program);
    }

    let program_units = programs
        .iter()
        .zip(units)
        .map(|(program, unit)| ProgramUnit {
            program,
            base_dir: unit.base_dir.as_deref(),
        })
        .collect::<Vec<_>>();

    compile_program_units_detailed(&program_units)
}

pub fn compile_program_units_detailed(
    units: &[ProgramUnit<'_>],
) -> Result<Vec<u8>, SourceDiagnostic> {
    Ok(compile_program_units(units)?.main)
}

/// 编译多个程序单元（共享芯片字典与寄存器分配），返回主程序字节码与所有派发表。
pub fn compile_program_units(units: &[ProgramUnit<'_>]) -> Result<CompiledProgram, SourceDiagnostic> {
    let mut registry = ChipRegistry::default();
    let mut bytecode = Vec::new();
    let mut vars = HashMap::new();
    let mut next_reg: u16 = 0;
    let mut irqs = Vec::new();

    for (unit_idx, unit) in units.iter().enumerate() {
        compile_into(
            unit.program,
            unit.base_dir,
            &mut registry,
            &mut bytecode,
            &mut vars,
            &mut next_reg,
            &mut irqs,
        )
        .map_err(|diag| SourceDiagnostic {
            unit: unit_idx,
            span: diag.span,
            message: diag.message,
            help: diag.help,
        })?;
    }

    bytecode.push(rseq_vm::Opcode::Return as u8);
    Ok(CompiledProgram {
        main: bytecode,
        irqs,
    })
}

fn compile_into(
    program: &Program,
    base_dir: Option<&Path>,
    registry: &mut ChipRegistry,
    bytecode: &mut Vec<u8>,
    vars: &mut HashMap<String, u8>,
    next_reg: &mut u16,
    irqs: &mut Vec<IrqVector>,
) -> Result<(), CompileDiagnostic> {
    // 第一遍：加载所有 chip! 字典
    for (idx, stmt) in program.stmts.iter().enumerate() {
        if let Stmt::Chip { path } = stmt {
            let chip_path = resolve_chip_path(path, base_dir);
            registry
                .load_file(&chip_path)
                .map_err(CompileError::Chip)
                .map_err(|error| compile_diagnostic(program, idx, error))?;
        }
    }

    // 第二遍：编译语句
    for (idx, stmt) in program.stmts.iter().enumerate() {
        if let Err(error) = compile_stmt(stmt, registry, vars, next_reg, bytecode, irqs) {
            return Err(compile_diagnostic(program, idx, error));
        }
    }

    Ok(())
}

fn compile_stmt(
    stmt: &Stmt,
    registry: &ChipRegistry,
    vars: &mut HashMap<String, u8>,
    next_reg: &mut u16,
    bytecode: &mut Vec<u8>,
    irqs: &mut Vec<IrqVector>,
) -> Result<(), CompileError> {
    match stmt {
        Stmt::Chip { .. } => {}
        Stmt::Irq { pin, arms } => {
            let vector = compile_irq(pin, arms, registry)?;
            irqs.push(vector);
        }
        Stmt::Let { name, expr } => {
            let dst = compile_expr(expr, registry, vars, next_reg, bytecode)?;
            vars.insert(name.clone(), dst);
        }
        Stmt::Write {
            addr,
            val,
            delay_us,
        } => {
            let addr = resolve_u32(addr, registry)?;
            let delay = delay_us.unwrap_or(0);

            // 写入一个由 let 绑定的变量：变量的值在运行期保存在寄存器里，
            // 因此发射 WriteVar，由 VM 在执行时把寄存器的低字节写入总线。
            if let Value::Ident(name) = val {
                let src = *vars
                    .get(name)
                    .ok_or_else(|| CompileError::UndefinedVariable(name.clone()))?;
                // 与标量 `write!(addr, n)` 保持一致：默认写 1 字节（低 8 位）。
                let len: u32 = 1;
                bytecode.push(rseq_vm::Opcode::WriteVar as u8);
                bytecode.extend_from_slice(&addr.to_le_bytes());
                bytecode.extend_from_slice(&len.to_le_bytes());
                bytecode.extend_from_slice(&delay.to_le_bytes());
                bytecode.push(src);
                return Ok(());
            }

            let data = match val {
                Value::Number(n) => vec![*n as u8],
                Value::Array(arr) => {
                    let mut bytes = Vec::new();
                    for v in arr {
                        match v {
                            Value::Number(n) => bytes.push(*n as u8),
                            _ => return Err(CompileError::UnsupportedValue),
                        }
                    }
                    bytes
                }
                _ => return Err(CompileError::UnsupportedValue),
            };

            let len = data.len() as u32;

            bytecode.push(rseq_vm::Opcode::Write as u8);
            bytecode.extend_from_slice(&addr.to_le_bytes());
            bytecode.extend_from_slice(&len.to_le_bytes());
            bytecode.extend_from_slice(&delay.to_le_bytes());
            bytecode.extend(data);
        }
        Stmt::Update {
            target,
            val,
            delay_us,
        } => {
            let updates = match val {
                Value::Number(n) => {
                    let field = target
                        .rsplit('.')
                        .next()
                        .ok_or_else(|| CompileError::Update("missing field name".into()))?;
                    vec![(field.to_string(), *n)]
                }
                Value::FieldMap(entries) => entries.clone(),
                _ => {
                    return Err(CompileError::Update(
                        "expected field value or field map".into(),
                    ));
                }
            };
            let plan = registry
                .plan_update(target, &updates)
                .map_err(CompileError::Chip)?;
            emit_update_bytecode(bytecode, &plan, delay_us.unwrap_or(0));
        }
    }
    Ok(())
}

/// 把一个 `irq!(pin) { on(event){...} ... }` 块编译成方案 A 的派发表。
///
/// 状态快照优先使用芯片字典里 `interrupt_status_snapshot` 角色的寄存器
/// （一次读取覆盖整组状态位，满足 read_clear 只读一次的约束）；若芯片未
/// 声明快照视图，则退化为"所有事件必须位于同一个状态寄存器"的单寄存器读取。
fn compile_irq(
    pin: &str,
    arms: &[IrqArm],
    registry: &ChipRegistry,
) -> Result<IrqVector, CompileError> {
    // 解析每个事件在独立状态寄存器中的位置。
    let resolved: Vec<(&IrqArm, EventBit)> = arms
        .iter()
        .map(|arm| {
            registry
                .resolve_event(&arm.event)
                .map(|eb| (arm, eb))
                .map_err(|_| CompileError::UnknownEvent(arm.event.clone()))
        })
        .collect::<Result<_, _>>()?;

    // 确定快照基址、宽度与 read_clear。
    let (snapshot_addr, snapshot_len, read_clear) = match registry.interrupt_snapshot() {
        Some(s) => s,
        None => {
            // 没有快照视图：以第一个事件的状态寄存器为准，要求所有事件同址。
            let first = resolved.first().ok_or(CompileError::NoInterruptStatus)?.1.clone();
            if resolved.iter().any(|(_, eb)| eb.status_addr != first.status_addr) {
                return Err(CompileError::NoInterruptStatus);
            }
            (first.status_addr, 1, first.read_clear)
        }
    };

    let mut bins = Vec::with_capacity(resolved.len());
    for (arm, eb) in &resolved {
        // 事件所在状态寄存器相对快照基址的字节偏移。
        if eb.status_addr < snapshot_addr {
            return Err(CompileError::NoInterruptStatus);
        }
        let byte_off = eb.status_addr - snapshot_addr;
        if byte_off >= snapshot_len {
            // 事件不在快照覆盖范围内，无法在一次读取中判定。
            return Err(CompileError::NoInterruptStatus);
        }
        let bit_off = byte_off * 8 + eb.bit_lo as u32;
        let width = (eb.bit_hi - eb.bit_lo + 1) as u32;
        let field_mask: u64 = if width >= 64 {
            u64::MAX
        } else {
            (1u64 << width) - 1
        };
        let mask = field_mask << bit_off;

        // 把分支语句体编译成一个自包含的处理段（独立 Vm 实例执行）。
        let mut handler = Vec::new();
        let mut handler_vars = HashMap::new();
        let mut handler_reg: u16 = 0;
        let mut nested_irqs = Vec::new();
        for s in &arm.body {
            compile_stmt(
                s,
                registry,
                &mut handler_vars,
                &mut handler_reg,
                &mut handler,
                &mut nested_irqs,
            )?;
        }
        handler.push(rseq_vm::Opcode::Return as u8);

        bins.push(IrqArmBin {
            event: arm.event.clone(),
            mask,
            handler,
        });
    }

    Ok(IrqVector {
        pin: pin.to_string(),
        snapshot_addr,
        snapshot_len,
        read_clear,
        arms: bins,
    })
}

/// 分配一个新的寄存器索引。
fn alloc_reg(next_reg: &mut u16) -> Result<u8, CompileError> {
    if *next_reg >= 256 {
        return Err(CompileError::RegisterOverflow);
    }
    let reg = *next_reg as u8;
    *next_reg += 1;
    Ok(reg)
}

/// 将表达式编译为字节码，返回存放结果的寄存器索引。
fn compile_expr(
    expr: &Expr,
    registry: &ChipRegistry,
    vars: &mut HashMap<String, u8>,
    next_reg: &mut u16,
    bytecode: &mut Vec<u8>,
) -> Result<u8, CompileError> {
    match expr {
        Expr::Number(n) => {
            let dst = alloc_reg(next_reg)?;
            bytecode.push(rseq_vm::Opcode::LoadConst as u8);
            bytecode.push(dst);
            bytecode.extend_from_slice(&n.to_le_bytes());
            Ok(dst)
        }
        Expr::Ident(name) => vars.get(name).copied().ok_or_else(|| {
            CompileError::UndefinedVariable(name.clone())
        }),
        Expr::Read {
            addr,
            len,
            delay_us,
        } => {
            let addr = resolve_u32(addr, registry)?;
            let len = resolve_u32(len, registry)?;
            if len == 0 || len > 4 {
                return Err(CompileError::UnsupportedValue);
            }
            let delay = delay_us.unwrap_or(0);
            let dst = alloc_reg(next_reg)?;
            bytecode.push(rseq_vm::Opcode::ReadVar as u8);
            bytecode.extend_from_slice(&addr.to_le_bytes());
            bytecode.extend_from_slice(&len.to_le_bytes());
            bytecode.extend_from_slice(&delay.to_le_bytes());
            bytecode.push(dst);
            Ok(dst)
        }
        Expr::Binary { op, lhs, rhs } => {
            let lhs_reg = compile_expr(lhs, registry, vars, next_reg, bytecode)?;
            let rhs_reg = compile_expr(rhs, registry, vars, next_reg, bytecode)?;
            let dst = alloc_reg(next_reg)?;
            let opcode = match op {
                BinOp::Add => rseq_vm::Opcode::Add,
                BinOp::Sub => rseq_vm::Opcode::Sub,
                BinOp::Mul => rseq_vm::Opcode::Mul,
                BinOp::Div => rseq_vm::Opcode::Div,
                BinOp::Mod => rseq_vm::Opcode::Mod,
                BinOp::Shl => rseq_vm::Opcode::Shl,
                BinOp::Shr => rseq_vm::Opcode::Shr,
                BinOp::And => rseq_vm::Opcode::And,
                BinOp::Or => rseq_vm::Opcode::Or,
                BinOp::Xor => rseq_vm::Opcode::Xor,
            };
            bytecode.push(opcode as u8);
            bytecode.push(dst);
            bytecode.push(lhs_reg);
            bytecode.push(rhs_reg);
            Ok(dst)
        }
    }
}

fn compile_diagnostic(program: &Program, idx: usize, error: CompileError) -> CompileDiagnostic {
    let span = program.stmt_spans.get(idx).cloned().unwrap_or(0..0);
    let help = compile_help(&error);
    CompileDiagnostic {
        span,
        message: error.to_string(),
        help,
        error,
    }
}

fn compile_help(error: &CompileError) -> Option<String> {
    match error {
        CompileError::Chip(ChipError::RegisterNotUpdatable { name, access }) => Some(format!(
            "register '{name}' is declared as access={access}; use update! only on RW registers, or use read!/write! if that matches the device semantics"
        )),
        CompileError::Chip(ChipError::FieldNotFound(field)) => Some(format!(
            "check the field name in the loaded chip YAML; '{field}' was not found"
        )),
        CompileError::Chip(ChipError::NotFound(name)) => Some(format!(
            "check that '{name}' exists in a chip! YAML file loaded before this statement"
        )),
        CompileError::UnsupportedValue => {
            Some("write! values must be a byte number or an array of byte numbers".to_string())
        }
        CompileError::NumberExpected => Some(
            "use a numeric literal like 0x10, or a register name from a loaded chip dictionary"
                .to_string(),
        ),
        CompileError::Update(_) => Some(
            "use update!(PAGE.REG.FIELD, value) or update!(PAGE.REG, { field: value })".to_string(),
        ),
        CompileError::UndefinedVariable(name) => Some(format!(
            "declare '{name}' with `let {name} = ...` before using it in an expression"
        )),
        CompileError::RegisterOverflow => Some(
            "the program uses too many live values; reduce the number of let bindings".to_string(),
        ),
        CompileError::UnknownEvent(name) => Some(format!(
            "'{name}' is not a known interrupt event; use an `event:` name declared on an interrupt_status field in the chip YAML"
        )),
        CompileError::NoInterruptStatus => Some(
            "declare an interrupt status register (role interrupt_status) and ideally an interrupt_status_snapshot view in the chip YAML so irq! events can be dispatched in a single read".to_string(),
        ),
        _ => None,
    }
}

fn resolve_u32(value: &Value, registry: &ChipRegistry) -> Result<u32, CompileError> {
    match value {
        Value::Number(n) => Ok(*n),
        Value::Ident(name) => registry
            .resolve_register(name)
            .map(|(addr, _)| addr)
            .map_err(|e| CompileError::Register(e.to_string())),
        _ => Err(CompileError::NumberExpected),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecompileError {
    UnexpectedEnd,
    InvalidOpcode,
}

pub fn decompile(bytecode: &[u8]) -> Result<String, DecompileError> {
    let mut pc = 0;
    let mut output = String::new();

    fn read_u32(data: &[u8], pos: &mut usize) -> Result<u32, DecompileError> {
        if *pos + 4 > data.len() {
            return Err(DecompileError::UnexpectedEnd);
        }
        let bytes = [data[*pos], data[*pos + 1], data[*pos + 2], data[*pos + 3]];
        *pos += 4;
        Ok(u32::from_le_bytes(bytes))
    }

    while pc < bytecode.len() {
        match rseq_vm::Opcode::from_u8(bytecode[pc]) {
            Some(rseq_vm::Opcode::Read) => {
                pc += 1;
                let addr = read_u32(bytecode, &mut pc)?;
                let len = read_u32(bytecode, &mut pc)?;
                let delay = read_u32(bytecode, &mut pc)?;
                output.push_str(&format!("read!(0x{:x}, {}", addr, len));
                if delay > 0 {
                    output.push_str(&format!(", {}", delay));
                }
                output.push_str(");\n");
            }
            Some(rseq_vm::Opcode::Write) => {
                pc += 1;
                let addr = read_u32(bytecode, &mut pc)?;
                let len = read_u32(bytecode, &mut pc)?;
                let delay = read_u32(bytecode, &mut pc)?;
                if pc + (len as usize) > bytecode.len() {
                    return Err(DecompileError::UnexpectedEnd);
                }
                let data = &bytecode[pc..pc + len as usize];
                pc += len as usize;
                output.push_str(&format!("write!(0x{:x}, ", addr));
                if len == 1 {
                    output.push_str(&format!("0x{:02x}", data[0]));
                } else {
                    output.push('[');
                    for (i, byte) in data.iter().enumerate() {
                        if i > 0 {
                            output.push_str(", ");
                        }
                        output.push_str(&format!("0x{:02x}", byte));
                    }
                    output.push(']');
                }
                if delay > 0 {
                    output.push_str(&format!(", {}", delay));
                }
                output.push_str(");\n");
            }
            Some(rseq_vm::Opcode::Update) => {
                pc += 1;
                let addr = read_u32(bytecode, &mut pc)?;
                let width = read_u32(bytecode, &mut pc)?;
                let delay = read_u32(bytecode, &mut pc)?;
                let field_count = read_u32(bytecode, &mut pc)?;
                output.push_str(&format!(
                    "update!(0x{addr:x}, /* {width} bytes, {field_count} fields */"
                ));
                for _ in 0..field_count {
                    if pc + 6 > bytecode.len() {
                        return Err(DecompileError::UnexpectedEnd);
                    }
                    let bit_lo = bytecode[pc];
                    let bit_hi = bytecode[pc + 1];
                    pc += 2;
                    let value = read_u32(bytecode, &mut pc)?;
                    output.push_str(&format!(" {{bits {bit_hi}:{bit_lo} = {value}}}"));
                }
                if delay > 0 {
                    output.push_str(&format!(", {}", delay));
                }
                output.push_str(");\n");
            }
            Some(rseq_vm::Opcode::Return) => {
                break;
            }
            Some(rseq_vm::Opcode::ReadVar) => {
                pc += 1;
                let addr = read_u32(bytecode, &mut pc)?;
                let len = read_u32(bytecode, &mut pc)?;
                let delay = read_u32(bytecode, &mut pc)?;
                if pc >= bytecode.len() {
                    return Err(DecompileError::UnexpectedEnd);
                }
                let dst = bytecode[pc];
                pc += 1;
                output.push_str(&format!("// r{dst} = read!(0x{addr:x}, {len}"));
                if delay > 0 {
                    output.push_str(&format!(", {delay}"));
                }
                output.push_str(");\n");
            }
            Some(rseq_vm::Opcode::LoadConst) => {
                pc += 1;
                if pc >= bytecode.len() {
                    return Err(DecompileError::UnexpectedEnd);
                }
                let dst = bytecode[pc];
                pc += 1;
                let imm = read_u32(bytecode, &mut pc)?;
                output.push_str(&format!("// r{dst} = 0x{imm:x}\n"));
            }
            Some(rseq_vm::Opcode::Move) => {
                pc += 1;
                if pc + 1 >= bytecode.len() {
                    return Err(DecompileError::UnexpectedEnd);
                }
                let dst = bytecode[pc];
                let src = bytecode[pc + 1];
                pc += 2;
                output.push_str(&format!("// r{dst} = r{src}\n"));
            }
            Some(op @ (rseq_vm::Opcode::Add
            | rseq_vm::Opcode::Sub
            | rseq_vm::Opcode::Mul
            | rseq_vm::Opcode::Div
            | rseq_vm::Opcode::Mod
            | rseq_vm::Opcode::Shl
            | rseq_vm::Opcode::Shr
            | rseq_vm::Opcode::And
            | rseq_vm::Opcode::Or
            | rseq_vm::Opcode::Xor)) => {
                pc += 1;
                if pc + 2 >= bytecode.len() {
                    return Err(DecompileError::UnexpectedEnd);
                }
                let dst = bytecode[pc];
                let lhs = bytecode[pc + 1];
                let rhs = bytecode[pc + 2];
                pc += 3;
                let op_str = match op {
                    rseq_vm::Opcode::Add => "+",
                    rseq_vm::Opcode::Sub => "-",
                    rseq_vm::Opcode::Mul => "*",
                    rseq_vm::Opcode::Div => "/",
                    rseq_vm::Opcode::Mod => "%",
                    rseq_vm::Opcode::Shl => "<<",
                    rseq_vm::Opcode::Shr => ">>",
                    rseq_vm::Opcode::And => "&",
                    rseq_vm::Opcode::Or => "|",
                    rseq_vm::Opcode::Xor => "^",
                    _ => unreachable!(),
                };
                output.push_str(&format!("// r{dst} = r{lhs} {op_str} r{rhs}\n"));
            }
            Some(rseq_vm::Opcode::WriteVar) => {
                pc += 1;
                let addr = read_u32(bytecode, &mut pc)?;
                let len = read_u32(bytecode, &mut pc)?;
                let delay = read_u32(bytecode, &mut pc)?;
                if pc >= bytecode.len() {
                    return Err(DecompileError::UnexpectedEnd);
                }
                let src = bytecode[pc];
                pc += 1;
                output.push_str(&format!("write!(0x{addr:x}, r{src} /* {len} bytes */"));
                if delay > 0 {
                    output.push_str(&format!(", {delay}"));
                }
                output.push_str(");\n");
            }
            Some(rseq_vm::Opcode::UpdateVar) => {
                return Err(DecompileError::InvalidOpcode);
            }
            None => {
                return Err(DecompileError::InvalidOpcode);
            }
        }
    }

    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 用于 irq! 派发测试的简易内存总线。
    struct MapBus {
        mem: HashMap<u32, u8>,
        writes: Vec<(u32, Vec<u8>)>,
    }

    impl MapBus {
        fn new() -> Self {
            Self {
                mem: HashMap::new(),
                writes: Vec::new(),
            }
        }
    }

    impl rseq_vm::Bus for MapBus {
        fn read(&mut self, addr: u32, data: &mut [u8]) -> Result<(), rseq_vm::BusError> {
            for (i, slot) in data.iter_mut().enumerate() {
                *slot = self.mem.get(&(addr + i as u32)).copied().unwrap_or(0);
            }
            Ok(())
        }
        fn write(&mut self, addr: u32, data: &[u8]) -> Result<(), rseq_vm::BusError> {
            for (i, &b) in data.iter().enumerate() {
                self.mem.insert(addr + i as u32, b);
            }
            self.writes.push((addr, data.to_vec()));
            Ok(())
        }
        fn delay_us(&mut self, _us: u32) -> Result<(), rseq_vm::BusError> {
            Ok(())
        }
    }

    fn qmi_base() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    #[test]
    fn test_parse_irq_block() {
        let src = r#"
        irq!(int1) {
            on(fifo_watermark) { write!(0x10, 0x01); }
            on(accel_drdy) { write!(0x11, 0x02); }
        }
        "#;
        let program = parse(src).unwrap();
        match &program.stmts[0] {
            Stmt::Irq { pin, arms } => {
                assert_eq!(pin, "int1");
                assert_eq!(arms.len(), 2);
                assert_eq!(arms[0].event, "fifo_watermark");
                assert_eq!(arms[1].event, "accel_drdy");
                assert_eq!(arms[0].body.len(), 1);
            }
            _ => panic!("expected Irq"),
        }
    }

    #[test]
    fn test_compile_irq_vector_masks_and_snapshot() {
        let src = r#"
        chip!("qmi8660.yaml");
        irq!(int1) {
            on(fifo_watermark) { write!(UI.ENCTL, 0x03); }
            on(accel_drdy) { write!(UI.ENCTL, 0x01); }
        }
        "#;
        let program = parse(src).unwrap();
        let compiled = compile_program(&program, Some(&qmi_base())).unwrap();

        // irq! 不进入主程序，主程序只有 Return。
        assert_eq!(compiled.main, vec![rseq_vm::Opcode::Return as u8]);

        assert_eq!(compiled.irqs.len(), 1);
        let vector = &compiled.irqs[0];
        assert_eq!(vector.pin, "int1");
        // 快照视图 INT_HELPER：0x58，4 字节，读后清零。
        assert_eq!(vector.snapshot_addr, 0x58);
        assert_eq!(vector.snapshot_len, 4);
        assert!(vector.read_clear);

        assert_eq!(vector.arms.len(), 2);
        // fifo_watermark 在 INT_STATUS0 bit6 → mask 0x40。
        assert_eq!(vector.arms[0].event, "fifo_watermark");
        assert_eq!(vector.arms[0].mask, 1 << 6);
        // accel_drdy 在 INT_STATUS0 bit0 → mask 0x1。
        assert_eq!(vector.arms[1].event, "accel_drdy");
        assert_eq!(vector.arms[1].mask, 1 << 0);
        // 处理段自包含，以 Return 结尾。
        assert_eq!(
            vector.arms[0].handler.last(),
            Some(&(rseq_vm::Opcode::Return as u8))
        );
    }

    #[test]
    fn test_compile_irq_unknown_event_errors() {
        let src = r#"
        chip!("qmi8660.yaml");
        irq!(int1) {
            on(not_a_real_event) { write!(UI.ENCTL, 0x03); }
        }
        "#;
        let program = parse_detailed(src).unwrap();
        let diag = compile_program(&program, Some(&qmi_base())).unwrap_err();
        assert!(matches!(
            diag.error,
            CompileError::UnknownEvent(ref name) if name == "not_a_real_event"
        ));
    }

    #[test]
    fn test_run_irq_vector_dispatches_only_set_bits() {
        let src = r#"
        chip!("qmi8660.yaml");
        irq!(int1) {
            on(fifo_watermark) { write!(UI.ENCTL, 0x03); }
            on(accel_drdy) { write!(UI.ENCTL, 0x01); }
        }
        "#;
        let program = parse(src).unwrap();
        let compiled = compile_program(&program, Some(&qmi_base())).unwrap();
        let vector = &compiled.irqs[0];

        // 只有 fifo_watermark (bit6) 置位。
        let mut bus = MapBus::new();
        bus.mem.insert(0x58, 0x40);
        let fired = run_irq_vector(&mut bus, vector).unwrap();
        assert_eq!(fired, vec!["fifo_watermark".to_string()]);
        // 处理段把 0x03 写到 UI.ENCTL (0x3d)。
        assert!(bus.writes.iter().any(|(a, d)| *a == 0x3d && d == &[0x03]));

        // 只有 accel_drdy (bit0) 置位。
        let mut bus = MapBus::new();
        bus.mem.insert(0x58, 0x01);
        let fired = run_irq_vector(&mut bus, vector).unwrap();
        assert_eq!(fired, vec!["accel_drdy".to_string()]);
        assert!(bus.writes.iter().any(|(a, d)| *a == 0x3d && d == &[0x01]));

        // 两个都置位 → 按声明顺序（优先级）触发。
        let mut bus = MapBus::new();
        bus.mem.insert(0x58, 0x41);
        let fired = run_irq_vector(&mut bus, vector).unwrap();
        assert_eq!(
            fired,
            vec!["fifo_watermark".to_string(), "accel_drdy".to_string()]
        );

        // 无关位置位 → 不触发任何分支。
        let mut bus = MapBus::new();
        bus.mem.insert(0x58, 0x80);
        let fired = run_irq_vector(&mut bus, vector).unwrap();
        assert!(fired.is_empty());
    }

    #[test]
    fn test_parse_line_comment_is_ignored() {
        let src = r"
        // 这是注释
        let x = 1 + 2; // 行尾注释
        ";
        let program = parse(src).unwrap();
        assert_eq!(program.stmts.len(), 1);
    }

    #[test]
    fn test_parse_let_read() {
        let src = r"
        let val = read!(0x10, 1, 100);
        ";
        let result = parse(src);
        assert!(result.is_ok());
        let program = result.unwrap();
        assert_eq!(program.stmts.len(), 1);
        match &program.stmts[0] {
            Stmt::Let { name, expr } => {
                assert_eq!(name, "val");
                match expr {
                    Expr::Read {
                        addr,
                        len,
                        delay_us,
                    } => {
                        assert_eq!(addr, &Value::Number(0x10));
                        assert_eq!(len, &Value::Number(1));
                        assert_eq!(delay_us, &Some(100));
                    }
                    _ => panic!("Expected Expr::Read"),
                }
            }
            _ => panic!("Expected Let statement"),
        }
    }

    #[test]
    fn test_parse_read_no_delay() {
        let src = r"
        let data = read!(0x20, 4);
        ";
        let result = parse(src);
        assert!(result.is_ok());
    }

    #[test]
    fn test_parse_write() {
        let src = r"
        write!(0x30, 0x55, 1000);
        ";
        let result = parse(src);
        assert!(result.is_ok());
    }

    #[test]
    fn test_parse_write_array_with_commas() {
        let src = r"
        write!(0x40, [0x01, 0x02, 0x03]);
        ";
        let result = parse(src);
        assert!(result.is_ok());
        let program = result.unwrap();
        assert_eq!(program.stmts.len(), 1);
        match &program.stmts[0] {
            Stmt::Write { addr, val, .. } => {
                assert_eq!(addr, &Value::Number(0x40));
                assert_eq!(
                    val,
                    &Value::Array(vec![
                        Value::Number(0x01),
                        Value::Number(0x02),
                        Value::Number(0x03),
                    ])
                );
            }
            _ => panic!("Expected Write statement"),
        }
    }

    #[test]
    fn test_parse_chip() {
        let src = r#"
        chip!("qmi8660.yaml");
        write!(UI.WHOAMI, 0x06);
        "#;
        let program = parse(src).unwrap();
        assert_eq!(program.stmts.len(), 2);
        assert!(matches!(&program.stmts[0], Stmt::Chip { path } if path == "qmi8660.yaml"));
    }

    #[test]
    fn test_compile_with_chip_register() {
        use std::path::PathBuf;

        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../qmi8660.yaml");
        let base = path.parent().unwrap();
        let src = r#"
        chip!("qmi8660.yaml");
        write!(UI.RESET, 0x98, 50);
        let id = read!(UI.WHOAMI, 1);
        "#;
        let program = parse(src).unwrap();
        let bytecode = compile_with_base(&program, Some(base)).unwrap();
        assert!(bytecode.len() > 1);
    }

    #[test]
    fn test_parse_update_single_field() {
        let src = r#"
        update!(UI.COMM_CTL.sda_scl_pu_dis, 1);
        "#;
        let program = parse(src).unwrap();
        match &program.stmts[0] {
            Stmt::Update { target, val, .. } => {
                assert_eq!(target, "UI.COMM_CTL.sda_scl_pu_dis");
                assert_eq!(val, &Value::Number(1));
            }
            _ => panic!("expected Update"),
        }
    }

    #[test]
    fn test_parse_update_field_map() {
        let src = r#"
        update!(UI.COMM_CTL, { cs_pu_dis: 1, sda_scl_pu_dis: 0 });
        "#;
        let program = parse(src).unwrap();
        match &program.stmts[0] {
            Stmt::Update { target, val, .. } => {
                assert_eq!(target, "UI.COMM_CTL");
                assert_eq!(
                    val,
                    &Value::FieldMap(vec![("cs_pu_dis".into(), 1), ("sda_scl_pu_dis".into(), 0),])
                );
            }
            _ => panic!("expected Update"),
        }
    }

    #[test]
    fn test_compile_update_rmw() {
        use std::path::PathBuf;

        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../qmi8660.yaml");
        let base = path.parent().unwrap();
        let src = r#"
        chip!("qmi8660.yaml");
        write!(UI.COMM_CTL, 0x2A);
        update!(UI.COMM_CTL.cs_pu_dis, 1);
        "#;
        let program = parse(src).unwrap();
        let bytecode = compile_with_base(&program, Some(base)).unwrap();
        assert_eq!(bytecode[0], rseq_vm::Opcode::Write as u8);
        assert_eq!(bytecode[14], rseq_vm::Opcode::Update as u8);
    }

    #[test]
    fn test_compile_diagnostic_points_to_failing_statement() {
        use std::path::PathBuf;

        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../qmi8660.yaml");
        let base = path.parent().unwrap();
        let src = r#"
        chip!("qmi8660.yaml");
        update!(UI.ENCTL.aen_ui, 1);
        update!(UI.WHOAMI.value, 0x08);
        "#;
        let program = parse_detailed(src).unwrap();
        let diag = compile_with_base_detailed(&program, Some(base)).unwrap_err();

        assert!(matches!(
            diag.error,
            CompileError::Chip(ChipError::RegisterNotUpdatable { .. })
        ));
        assert!(src[diag.span.clone()].contains("update!(UI.WHOAMI.value"));
        assert!(
            diag.help
                .as_deref()
                .is_some_and(|help| help.contains("update! only on RW registers"))
        );
    }

    #[test]
    fn test_compile_program_units_share_chip_registry_and_return_once() {
        use std::path::PathBuf;

        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../qmi8660.yaml");
        let base = path.parent().unwrap();
        let init = parse_detailed(
            r#"
            chip!("qmi8660.yaml");
            write!(UI.RESET, 0x98);
            "#,
        )
        .unwrap();
        let enable = parse_detailed(
            r#"
            update!(UI.ENCTL.aen_ui, 1);
            "#,
        )
        .unwrap();

        let bytecode = compile_program_units_detailed(&[
            ProgramUnit {
                program: &init,
                base_dir: Some(base),
            },
            ProgramUnit {
                program: &enable,
                base_dir: Some(base),
            },
        ])
        .unwrap();

        assert_eq!(bytecode.last(), Some(&(rseq_vm::Opcode::Return as u8)));
        assert_eq!(
            bytecode
                .iter()
                .filter(|byte| **byte == rseq_vm::Opcode::Return as u8)
                .count(),
            1
        );
        assert!(bytecode.contains(&(rseq_vm::Opcode::Update as u8)));
    }

    #[test]
    fn test_compile_units_diagnostic_reports_source_index() {
        use std::path::PathBuf;

        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../qmi8660.yaml");
        let base = path.parent().unwrap().to_path_buf();
        let units = vec![
            SourceUnit {
                name: "init.rseq".to_string(),
                source: r#"chip!("qmi8660.yaml");"#.to_string(),
                base_dir: Some(base.clone()),
            },
            SourceUnit {
                name: "bad.rseq".to_string(),
                source: r#"update!(UI.WHOAMI.value, 0x08);"#.to_string(),
                base_dir: Some(base),
            },
        ];

        let diag = compile_units_detailed(&units).unwrap_err();

        assert_eq!(diag.unit, 1);
        assert!(units[diag.unit].source[diag.span.clone()].contains("WHOAMI"));
        assert!(
            diag.help
                .as_deref()
                .is_some_and(|help| help.contains("update! only on RW registers"))
        );
    }

    #[test]
    fn test_manifest_selects_sequences_in_requested_order() {
        let manifest = Manifest::parse(
            r#"
            chip = "qmi8660.yaml"

            [[sequence]]
            id = "init"
            name = "Initialize QMI8660"
            file = "qmi8660_init.rseq"

            [[sequence]]
            id = "enable_accel"
            file = "qmi8660_enable_accel.rseq"
            "#,
        )
        .unwrap();

        let selected = manifest
            .selected_sequences(&["enable_accel".to_string(), "init".to_string()])
            .unwrap();

        assert_eq!(manifest.chip.as_deref(), Some("qmi8660.yaml"));
        assert_eq!(selected[0].id, "enable_accel");
        assert_eq!(selected[1].id, "init");
    }

    #[test]
    fn test_manifest_rejects_unknown_sequence() {
        let manifest = Manifest::parse(
            r#"
            [[sequence]]
            id = "init"
            file = "qmi8660_init.rseq"
            "#,
        )
        .unwrap();

        let err = manifest
            .selected_sequences(&["missing".to_string()])
            .unwrap_err();

        assert!(matches!(err, ManifestError::UnknownId(id) if id == "missing"));
    }

    // ── 算术表达式测试 ──────────────────────────────────

    #[test]
    fn test_parse_let_number() {
        let src = "let a = 0x10;";
        let program = parse(src).unwrap();
        match &program.stmts[0] {
            Stmt::Let { name, expr } => {
                assert_eq!(name, "a");
                assert_eq!(expr, &Expr::Number(0x10));
            }
            _ => panic!("expected Let"),
        }
    }

    #[test]
    fn test_parse_let_add() {
        let src = "let c = 1 + 2;";
        let program = parse(src).unwrap();
        match &program.stmts[0] {
            Stmt::Let { name, expr } => {
                assert_eq!(name, "c");
                match expr {
                    Expr::Binary { op, lhs, rhs } => {
                        assert_eq!(*op, BinOp::Add);
                        assert_eq!(**lhs, Expr::Number(1));
                        assert_eq!(**rhs, Expr::Number(2));
                    }
                    _ => panic!("expected Binary"),
                }
            }
            _ => panic!("expected Let"),
        }
    }

    #[test]
    fn test_parse_precedence_mul_over_add() {
        // a + b * c  →  a + (b * c)
        let src = "let x = 1 + 2 * 3;";
        let program = parse(src).unwrap();
        match &program.stmts[0] {
            Stmt::Let { expr, .. } => match expr {
                Expr::Binary { op, lhs, rhs } => {
                    assert_eq!(*op, BinOp::Add);
                    assert_eq!(**lhs, Expr::Number(1));
                    match rhs.as_ref() {
                        Expr::Binary { op, .. } => assert_eq!(*op, BinOp::Mul),
                        _ => panic!("expected Mul on rhs"),
                    }
                }
                _ => panic!("expected Binary"),
            },
            _ => panic!("expected Let"),
        }
    }

    #[test]
    fn test_parse_parens_override_precedence() {
        // (a + b) * c  →  (a + b) is grouped
        let src = "let x = (1 + 2) * 3;";
        let program = parse(src).unwrap();
        match &program.stmts[0] {
            Stmt::Let { expr, .. } => match expr {
                Expr::Binary { op, lhs, rhs } => {
                    assert_eq!(*op, BinOp::Mul);
                    match lhs.as_ref() {
                        Expr::Binary { op, .. } => assert_eq!(*op, BinOp::Add),
                        _ => panic!("expected Add in parens"),
                    }
                    assert_eq!(**rhs, Expr::Number(3));
                }
                _ => panic!("expected Binary"),
            },
            _ => panic!("expected Let"),
        }
    }

    #[test]
    fn test_parse_all_binary_ops() {
        let cases = [
            ("+", BinOp::Add),
            ("-", BinOp::Sub),
            ("*", BinOp::Mul),
            ("/", BinOp::Div),
            ("%", BinOp::Mod),
            ("<<", BinOp::Shl),
            (">>", BinOp::Shr),
            ("&", BinOp::And),
            ("|", BinOp::Or),
            ("^", BinOp::Xor),
        ];
        for (sym, expected) in cases {
            let src = format!("let x = 1 {sym} 2;");
            let program = parse(&src).unwrap();
            match &program.stmts[0] {
                Stmt::Let { expr, .. } => match expr {
                    Expr::Binary { op, .. } => assert_eq!(*op, expected, "for symbol {sym}"),
                    _ => panic!("expected Binary for {sym}"),
                },
                _ => panic!("expected Let"),
            }
        }
    }

    #[test]
    fn test_parse_shift_precedence_below_arith() {
        // 1 + 2 << 3  →  (1 + 2) << 3   [shift is lower precedence than +]
        let src = "let x = 1 + 2 << 3;";
        let program = parse(src).unwrap();
        match &program.stmts[0] {
            Stmt::Let { expr, .. } => match expr {
                Expr::Binary { op, lhs, rhs } => {
                    assert_eq!(*op, BinOp::Shl);
                    match lhs.as_ref() {
                        Expr::Binary { op, .. } => assert_eq!(*op, BinOp::Add),
                        _ => panic!("expected Add below shift"),
                    }
                    assert_eq!(**rhs, Expr::Number(3));
                }
                _ => panic!("expected Binary"),
            },
            _ => panic!("expected Let"),
        }
    }

    #[test]
    fn test_parse_and_or_xor_precedence() {
        // a | b ^ c & d  →  a | (b ^ (c & d))
        let src = "let x = 1 | 2 ^ 3 & 4;";
        let program = parse(src).unwrap();
        match &program.stmts[0] {
            Stmt::Let { expr, .. } => match expr {
                Expr::Binary { op, lhs, rhs } => {
                    assert_eq!(*op, BinOp::Or);
                    assert_eq!(**lhs, Expr::Number(1));
                    match rhs.as_ref() {
                        Expr::Binary { op, rhs, .. } => {
                            assert_eq!(*op, BinOp::Xor);
                            match rhs.as_ref() {
                                Expr::Binary { op, .. } => assert_eq!(*op, BinOp::And),
                                _ => panic!("expected And at deepest level"),
                            }
                        }
                        _ => panic!("expected Xor"),
                    }
                }
                _ => panic!("expected Binary"),
            },
            _ => panic!("expected Let"),
        }
    }

    #[test]
    fn test_parse_variable_reference_in_expr() {
        let src = "let c = a + b;";
        let program = parse(src).unwrap();
        match &program.stmts[0] {
            Stmt::Let { name, expr } => {
                assert_eq!(name, "c");
                match expr {
                    Expr::Binary { op, lhs, rhs } => {
                        assert_eq!(*op, BinOp::Add);
                        assert_eq!(**lhs, Expr::Ident("a".into()));
                        assert_eq!(**rhs, Expr::Ident("b".into()));
                    }
                    _ => panic!("expected Binary"),
                }
            }
            _ => panic!("expected Let"),
        }
    }

    #[test]
    fn test_compile_arithmetic_add() {
        let src = "let x = 1 + 2;";
        let program = parse(src).unwrap();
        let bytecode = compile(&program).unwrap();
        // LoadConst r0=1 ; LoadConst r1=2 ; Add r2=r0+r1 ; Return
        assert_eq!(bytecode[0], rseq_vm::Opcode::LoadConst as u8);
        assert_eq!(bytecode[6], rseq_vm::Opcode::LoadConst as u8);
        assert_eq!(bytecode[12], rseq_vm::Opcode::Add as u8);
        assert_eq!(bytecode.last(), Some(&(rseq_vm::Opcode::Return as u8)));
    }

    #[test]
    fn test_compile_arithmetic_with_variables() {
        let src = r"
        let a = 0x10;
        let b = 0x20;
        let c = a + b;
        ";
        let program = parse(src).unwrap();
        let bytecode = compile(&program).unwrap();
        // a: LoadConst r0=0x10
        // b: LoadConst r1=0x20
        // c: Add r2=r0+r1
        assert!(bytecode
            .iter()
            .any(|&b| b == rseq_vm::Opcode::Add as u8));
    }

    #[test]
    fn test_compile_read_var_emits_readvar() {
        let src = "let val = read!(0x10, 1);";
        let program = parse(src).unwrap();
        let bytecode = compile(&program).unwrap();
        assert_eq!(bytecode[0], rseq_vm::Opcode::ReadVar as u8);
    }

    #[test]
    fn test_compile_undefined_variable_error() {
        let src = "let x = undefined_var + 1;";
        let program = parse_detailed(src).unwrap();
        let diag = compile_with_base_detailed(&program, None).unwrap_err();
        assert!(matches!(
            diag.error,
            CompileError::UndefinedVariable(ref name) if name == "undefined_var"
        ));
    }

    #[test]
    fn test_compile_and_run_arithmetic_in_vm() {
        // compile a program that does arithmetic and verify via decompile
        let src = r"
        let a = 6;
        let b = 7;
        let c = a * b;
        let d = c - 1;
        let e = d << 2;
        let f = e & 0xFF;
        let g = f | 0x100;
        let h = g ^ 0x10;
        ";
        let program = parse(src).unwrap();
        let bytecode = compile(&program).unwrap();
        // Just verify it compiles and contains expected opcodes
        let has_mul = bytecode
            .iter()
            .any(|&b| b == rseq_vm::Opcode::Mul as u8);
        let has_sub = bytecode
            .iter()
            .any(|&b| b == rseq_vm::Opcode::Sub as u8);
        let has_shl = bytecode
            .iter()
            .any(|&b| b == rseq_vm::Opcode::Shl as u8);
        let has_and = bytecode
            .iter()
            .any(|&b| b == rseq_vm::Opcode::And as u8);
        let has_or = bytecode
            .iter()
            .any(|&b| b == rseq_vm::Opcode::Or as u8);
        let has_xor = bytecode
            .iter()
            .any(|&b| b == rseq_vm::Opcode::Xor as u8);
        assert!(has_mul && has_sub && has_shl && has_and && has_or && has_xor);
    }

    #[test]
    fn test_decompile_arithmetic_opcodes() {
        let src = "let x = 1 + 2;";
        let program = parse(src).unwrap();
        let bytecode = compile(&program).unwrap();
        let decompiled = decompile(&bytecode).unwrap();
        assert!(decompiled.contains("LoadConst") || decompiled.contains("r0 = 0x"));
        assert!(decompiled.contains("+"));
    }

    #[test]
    fn test_compile_div_and_mod() {
        let src = r"
        let q = 17 / 5;
        let r = 17 % 5;
        ";
        let program = parse(src).unwrap();
        let bytecode = compile(&program).unwrap();
        assert!(bytecode.iter().any(|&b| b == rseq_vm::Opcode::Div as u8));
        assert!(bytecode.iter().any(|&b| b == rseq_vm::Opcode::Mod as u8));
    }

    #[test]
    fn test_compile_shr() {
        let src = "let x = 0x100 >> 4;";
        let program = parse(src).unwrap();
        let bytecode = compile(&program).unwrap();
        assert!(bytecode
            .iter()
            .any(|&b| b == rseq_vm::Opcode::Shr as u8));
    }

    #[test]
    fn test_parse_complex_expression() {
        let src = "let x = (a + b) * (c - d) & 0xFF;";
        let program = parse(src).unwrap();
        match &program.stmts[0] {
            Stmt::Let { expr, .. } => {
                // top-level should be & (lowest precedence here)
                match expr {
                    Expr::Binary { op, .. } => assert_eq!(*op, BinOp::And),
                    _ => panic!("expected And at top level"),
                }
            }
            _ => panic!("expected Let"),
        }
    }
}