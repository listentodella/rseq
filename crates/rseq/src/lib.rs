//! Register Sequence DSL Parser
//! A DSL for defining register sequences in embedded systems

mod chip;

pub mod link;
pub mod trace;

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
    Chip, ChipError, ChipRegistry, EventBit, Page, Register, UpdatePlan, emit_update_bytecode,
    fields_to_bytes, load_chip, normalize_chip_path, resolve_chip_path,
};

type ParserExtra<'tok, 'src> = extra::Err<Rich<'tok, Token<'src>>>;

pub const REPORT_KIND_FIFO_RAW: u32 = 0x01;
pub const REPORT_KIND_AMD: u32 = 0x02;
pub const REPORT_KIND_SMD: u32 = 0x03;
pub const REPORT_KIND_DRDY: u32 = 0x04;

pub fn report_kind_name(kind: u32) -> Option<&'static str> {
    match kind {
        REPORT_KIND_FIFO_RAW => Some("FIFO_RAW"),
        REPORT_KIND_AMD => Some("AMD"),
        REPORT_KIND_SMD => Some("SMD"),
        REPORT_KIND_DRDY => Some("DRDY"),
        _ => None,
    }
}

pub fn report_kind_value(name: &str) -> Option<u32> {
    match name {
        "FIFO_RAW" => Some(REPORT_KIND_FIFO_RAW),
        "AMD" => Some(REPORT_KIND_AMD),
        "SMD" => Some(REPORT_KIND_SMD),
        "DRDY" => Some(REPORT_KIND_DRDY),
        _ => None,
    }
}

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
    #[token("bus!")]
    BusMacro,
    #[token("bus_probe!")]
    BusProbeMacro,
    #[token("irq!")]
    IrqMacro,
    #[token("repeat!")]
    RepeatMacro,
    #[token("print!")]
    PrintMacro,
    #[token("report_format!")]
    ReportFormatMacro,
    #[token("report!")]
    ReportMacro,
    #[token("wait!")]
    WaitMacro,
    #[token("if")]
    If,
    #[token("else")]
    Else,
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

    // ── 比较 / 逻辑运算符（多字符优先于单字符，logos 最长匹配）──
    #[token("==")]
    Eq,
    #[token("!=")]
    Ne,
    #[token("<=")]
    Le,
    #[token(">=")]
    Ge,
    #[token("<")]
    Lt,
    #[token(">")]
    Gt,
    #[token("&&")]
    AndAnd,
    #[token("||")]
    OrOr,
    #[token("!")]
    Bang,

    #[regex(r#""([^"\\]|\\.)*""#, |lex| {
        // 在引号之间做转义展开：\n \t \r \" \\；未知 \x 保留字面反斜杠。
        // 支持 print!("msg\n") / print!("a\nb") 这类换行。
        let s = lex.slice();
        let body = &s[1..s.len() - 1];
        let mut out = String::with_capacity(body.len());
        let mut chars = body.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                match chars.next() {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some('r') => out.push('\r'),
                    Some('"') => out.push('"'),
                    Some('\\') => out.push('\\'),
                    Some(other) => {
                        out.push('\\');
                        out.push(other);
                    }
                    None => out.push('\\'),
                }
            } else {
                out.push(c);
            }
        }
        out
    })]
    String(String),

    #[regex(r"[ \t\f\r\n]+", logos::skip)]
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
            Self::BusMacro => write!(f, "bus!"),
            Self::BusProbeMacro => write!(f, "bus_probe!"),
            Self::IrqMacro => write!(f, "irq!"),
            Self::RepeatMacro => write!(f, "repeat!"),
            Self::PrintMacro => write!(f, "print!"),
            Self::ReportFormatMacro => write!(f, "report_format!"),
            Self::ReportMacro => write!(f, "report!"),
            Self::WaitMacro => write!(f, "wait!"),
            Self::If => write!(f, "if"),
            Self::Else => write!(f, "else"),
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
            Self::Eq => write!(f, "=="),
            Self::Ne => write!(f, "!="),
            Self::Le => write!(f, "<="),
            Self::Ge => write!(f, ">="),
            Self::Lt => write!(f, "<"),
            Self::Gt => write!(f, ">"),
            Self::AndAnd => write!(f, "&&"),
            Self::OrOr => write!(f, "||"),
            Self::Bang => write!(f, "!"),
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
pub enum ReportOptionValue {
    Number(u32),
    Ident(String),
    IdentArray(Vec<String>),
}

impl fmt::Display for ReportOptionValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Number(value) => write!(f, "{value}"),
            Self::Ident(value) => write!(f, "{value}"),
            Self::IdentArray(values) => {
                write!(f, "[")?;
                for (idx, value) in values.iter().enumerate() {
                    if idx != 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{value}")?;
                }
                write!(f, "]")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    Chip {
        path: String,
    },
    /// `bus!(spi)` / `bus!(i2c[, addr])` / `bus!(i3c)`：选择后续寄存器
    /// 读写使用的物理总线。`arg` 预留给总线特定参数；当前 I2C 使用它
    /// 作为 7-bit slave address。
    Bus {
        kind: String,
        arg: Option<u32>,
    },
    /// `bus_probe!(kind, { addrs: [...], read: REG, expect: VALUE, ... })`：
    /// 通用总线探测。MCU 只尝试候选总线参数并读取寄存器匹配期望值；
    /// 芯片型号、地址候选和 WHOAMI 等知识全部来自 DSL/host。
    BusProbe {
        kind: String,
        options: Vec<(String, Value)>,
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
    /// `repeat!(N) { ... }` 定长循环：把 body 重复执行 N 次。
    /// 编译为 `Loop` 操作码（body 只存一份，VM 计数回跳），字节码不随 N 膨胀。
    Repeat {
        count: u32,
        body: Vec<Stmt>,
    },
    /// `read!(addr, len[, delay])` 独立读语句：发射 `Read` 操作码（≤4096字节）。
    /// 读出数据在 VM 本地丢弃，但 `TracingBus` 会把每次读作为 Trace 回传主机——
    /// 适合多字节采集/轮询。与 `let x = read!(...)`（`ReadVar`，≤4字节，存寄存器）互补。
    Read {
        addr: Value,
        len: Value,
        delay_us: Option<u32>,
    },
    /// `if (cond) { ... } else { ... }`：cond 为任意表达式（非零为真）。
    /// 编译为 `JumpIfZero`/`Jump`；体可嵌套 if/repeat。`else_` 空=无 else。
    If {
        cond: Box<Expr>,
        then: Vec<Stmt>,
        else_: Vec<Stmt>,
    },
    /// `print!("msg")` 或 `print!("fmt", v1, v2)`：vars 空 → `Log`（纯字符串）；
    /// vars 非空 → `LogVar`（变量插值，`{}` 有符号十进制 / `{x}` 十六进制）。
    /// 经 `Bus::log`/`Bus::log_vars` 在 MockBus/stdout、真机 USART3(printk)、
    /// 主机 trace 流三处可见。
    Print {
        msg: String,
        vars: Vec<String>,
    },
    /// `report!(kind, expr...)`：结构化数据上报。kind 为事件类型编号或
    /// 内置事件类型名（如 FIFO_RAW），参数可混合 u32 表达式与 raw data 变量。
    Report {
        kind: Value,
        values: Vec<Expr>,
    },
    /// `report_format!(kind, decoder, { option: value, ... })`：主机侧上报解码元数据。
    /// 该语句不生成 VM 字节码，只供 CLI/watch 解释 `report!` 回传的 raw bytes。
    ReportFormat {
        kind: Value,
        decoder: String,
        options: Vec<(String, ReportOptionValue)>,
    },
    /// `wait!(pin[, timeout_ms])`：阻塞至该中断引脚发生边沿（或超时），
    /// 紧随其后的内联派发序列读中断状态快照、按 `irq!(pin)` 各 arm 的
    /// 掩码分支，命中则执行对应体。timeout 缺省 4000ms。
    Wait {
        pin: String,
        timeout_ms: u32,
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
    // 比较（结果 0/1；Lt/Le/Gt/Ge 按有符号 i32）
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    // 逻辑（急求值非短路）
    AndAnd,
    OrOr,
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
            Self::Eq => "==",
            Self::Ne => "!=",
            Self::Lt => "<",
            Self::Le => "<=",
            Self::Gt => ">",
            Self::Ge => ">=",
            Self::AndAnd => "&&",
            Self::OrOr => "||",
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
    /// 一元前缀运算（当前仅逻辑非 `!`）。
    Unary {
        op: UnaryOp,
        expr: Box<Expr>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    /// `!expr`：逻辑非，结果 0/1。
    Not,
}

impl fmt::Display for UnaryOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Not => "!",
        };
        f.write_str(s)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    pub stmts: Vec<Stmt>,
    pub stmt_spans: Vec<Range<usize>>,
}

/// 完整编译产物：线性主程序字节码 + 若干中断模板。
///
/// `irq!(pin)` 块先整理为模板（掩码 + AST 体）：`wait!(pin)` 可把派发逻辑
/// 内联进 main 字节码；多单元编译/CLI 还会为 MCU 自动响应模式生成独立
/// IRQ 处理段。
#[derive(Debug, Clone, PartialEq)]
pub struct CompiledProgram {
    /// 上电初始化等顺序执行的字节码，以 Return 结尾。`wait!` 的内联派发
    /// 序列也在这里。
    pub main: Vec<u8>,
    /// 每个 `irq!(pin)` 块编译出的一份模板，供 `wait!(pin)` 内联引用，
    /// 也供主机 `--fire` 注入时定位快照地址。
    pub irqs: Vec<IrqTemplate>,
    /// 每个 irq 段的独立字节码（pin → bytecode），供 MCU 侧自动响应中断。
    /// key 为引脚名（如 "int1"），value 为该引脚的完整处理段字节码。
    pub irq_bytecodes: HashMap<String, Vec<u8>>,
}

/// 一个中断引脚的模板：`wait!(pin)` 命中边沿后读一次状态快照，
/// 再按声明顺序对命中的事件内联执行对应的 arm 体。
#[derive(Debug, Clone, PartialEq)]
pub struct IrqTemplate {
    /// 中断引脚名（仅作标注，如 "int1"）。
    pub pin: String,
    /// 引脚编号（= 在 `irqs` 中的下标），`WaitIrq` 操作码携带它；
    /// MCU 侧映射 0 → PB8（当前唯一支持的中断引脚）。
    pub pin_id: u8,
    /// 状态快照寄存器地址（一次性读取整组中断状态）。
    pub snapshot_addr: u32,
    /// 快照读取字节数（内联模型要求 ≤ 4，装进单个 u32 寄存器）。
    pub snapshot_len: u32,
    /// 状态寄存器读取后是否自动清零（决定只能读一次）。
    pub read_clear: bool,
    /// 各事件分支，按源码顺序即优先级排列。
    pub arms: Vec<IrqArmTpl>,
}

/// 模板里的一条事件分支：命中 `mask` 时内联执行 `body`。
#[derive(Debug, Clone, PartialEq)]
pub struct IrqArmTpl {
    pub event: String,
    /// 该事件在状态快照中的位掩码（snapshot_len≤4 时高位为 0）。
    pub mask: u64,
    /// arm 语句体（AST 克隆），由 `wait!` 内联编译进 main。
    pub body: Vec<Stmt>,
}

fn value<'tok, 'src: 'tok, I>()
-> impl Parser<'tok, I, Value, ParserExtra<'tok, 'src>> + Clone + 'tok
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

fn field_map<'tok, 'src: 'tok, I>()
-> impl Parser<'tok, I, Value, ParserExtra<'tok, 'src>> + Clone + 'tok
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

fn bus_probe_option_map<'tok, 'src: 'tok, I>()
-> impl Parser<'tok, I, Vec<(String, Value)>, ParserExtra<'tok, 'src>> + Clone + 'tok
where
    I: ValueInput<'tok, Token = Token<'src>, Span = SimpleSpan>,
{
    select! { Token::Ident(s) => s.to_string() }
        .then_ignore(just(Token::Colon))
        .then(value())
        .separated_by(just(Token::Comma))
        .allow_trailing()
        .collect::<Vec<_>>()
        .delimited_by(just(Token::LBrace), just(Token::RBrace))
}

fn report_option_map<'tok, 'src: 'tok, I>()
-> impl Parser<'tok, I, Vec<(String, ReportOptionValue)>, ParserExtra<'tok, 'src>> + Clone + 'tok
where
    I: ValueInput<'tok, Token = Token<'src>, Span = SimpleSpan>,
{
    let ident_array = select! { Token::Ident(s) => s.to_string() }
        .separated_by(just(Token::Comma))
        .allow_trailing()
        .collect::<Vec<_>>()
        .map(ReportOptionValue::IdentArray)
        .delimited_by(just(Token::LBracket), just(Token::RBracket));

    let ident = select! { Token::Ident(s) => ReportOptionValue::Ident(s.to_string()) };

    let option_value = select! { Token::Number(n) => ReportOptionValue::Number(n) }
        .or(ident_array)
        .or(ident);

    select! { Token::Ident(s) => s.to_string() }
        .then_ignore(just(Token::Colon))
        .then(option_value)
        .separated_by(just(Token::Comma))
        .allow_trailing()
        .collect::<Vec<_>>()
        .delimited_by(just(Token::LBrace), just(Token::RBrace))
}

fn read_expr<'tok, 'src: 'tok, I>()
-> impl Parser<'tok, I, Expr, ParserExtra<'tok, 'src>> + Clone + 'tok
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
///   `||` → `&&` → `|` → `^` → `&` → `==`/`!=` → `<`/`<=`/`>`/`>=`
///   → `<<`/`>>` → `+`/`-` → `*`/`/`/`%` → 一元 `!` → primary
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
        let atom = number.or(ident).or(read_expr()).or(paren);

        // 一元前缀 `!`（可叠加 `!!x`）；零个 `!` 时退化为 atom。
        let primary = just(Token::Bang)
            .repeated()
            .collect::<Vec<_>>()
            .then(atom)
            .map(|(bangs, atom)| {
                bangs.iter().fold(atom, |acc, _| Expr::Unary {
                    op: UnaryOp::Not,
                    expr: Box::new(acc),
                })
            });

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
        let mul_op = just(Token::Star)
            .to(BinOp::Mul)
            .or(just(Token::Slash).to(BinOp::Div))
            .or(just(Token::Percent).to(BinOp::Mod));
        let mul = binop_layer(primary, mul_op);

        // 加减
        let add_op = just(Token::Plus)
            .to(BinOp::Add)
            .or(just(Token::Minus).to(BinOp::Sub));
        let add = binop_layer(mul, add_op);

        // 移位
        let shift_op = just(Token::Shl)
            .to(BinOp::Shl)
            .or(just(Token::Shr).to(BinOp::Shr));
        let shift = binop_layer(add, shift_op);

        // 关系：< <= > >=（按有符号 i32 比较）
        let rel_op = just(Token::Lt)
            .to(BinOp::Lt)
            .or(just(Token::Le).to(BinOp::Le))
            .or(just(Token::Gt).to(BinOp::Gt))
            .or(just(Token::Ge).to(BinOp::Ge));
        let rel = binop_layer(shift, rel_op);

        // 相等：== !=
        let eq_op = just(Token::Eq)
            .to(BinOp::Eq)
            .or(just(Token::Ne).to(BinOp::Ne));
        let eq = binop_layer(rel, eq_op);

        // 按位与
        let and_op = just(Token::Amp).to(BinOp::And);
        let and = binop_layer(eq, and_op);

        // 按位异或
        let xor_op = just(Token::Caret).to(BinOp::Xor);
        let xor = binop_layer(and, xor_op);

        // 按位或
        let or_op = just(Token::Pipe).to(BinOp::Or);
        let or = binop_layer(xor, or_op);

        // 逻辑与 &&（急求值非短路；低于按位或）
        let andand_op = just(Token::AndAnd).to(BinOp::AndAnd);
        let logand = binop_layer(or, andand_op);

        // 逻辑或 ||（最低优先级）
        let oror_op = just(Token::OrOr).to(BinOp::OrOr);
        binop_layer(logand, oror_op)
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

    let bus_stmt = just(Token::BusMacro)
        .ignore_then(
            select! { Token::Ident(s) => s.to_string() }
                .then(
                    just(Token::Comma)
                        .ignore_then(select! { Token::Number(n) => n })
                        .or_not(),
                )
                .delimited_by(just(Token::LParen), just(Token::RParen)),
        )
        .then_ignore(just(Token::Semicolon).or_not())
        .map(|(kind, arg)| Stmt::Bus { kind, arg });

    let bus_probe_stmt = just(Token::BusProbeMacro)
        .ignore_then(
            select! { Token::Ident(s) => s.to_string() }
                .then(just(Token::Comma).ignore_then(bus_probe_option_map()))
                .delimited_by(just(Token::LParen), just(Token::RParen)),
        )
        .then_ignore(just(Token::Semicolon).or_not())
        .map(|(kind, options)| Stmt::BusProbe { kind, options });

    let let_stmt = just(Token::Let)
        .ignore_then(select! { Token::Ident(s) => s.to_string() })
        .then(just(Token::Assign).ignore_then(expr()))
        .then_ignore(just(Token::Semicolon).or_not())
        .map(|(name, expr)| Stmt::Let { name, expr });

    let write_stmt = just(Token::WriteMacro)
        .ignore_then(
            value()
                .then(just(Token::Comma).ignore_then(field_map().or(value())))
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

    let read_stmt = just(Token::ReadMacro)
        .ignore_then(
            value()
                .then(just(Token::Comma).ignore_then(value()))
                .then(just(Token::Comma).ignore_then(value()).or_not())
                .delimited_by(just(Token::LParen), just(Token::RParen)),
        )
        .then_ignore(just(Token::Semicolon).or_not())
        .map(|((addr, len), delay_us)| {
            let delay_us = delay_us.and_then(|v| match v {
                Value::Number(n) => Some(n),
                _ => None,
            });
            Stmt::Read {
                addr,
                len,
                delay_us,
            }
        });

    let print_stmt = just(Token::PrintMacro)
        .ignore_then(
            select! { Token::String(s) => s }
                .then(
                    just(Token::Comma)
                        .ignore_then(
                            select! { Token::Ident(s) => s.to_string() }
                                .separated_by(just(Token::Comma))
                                .allow_trailing()
                                .collect::<Vec<_>>(),
                        )
                        .or_not()
                        .map(|v| v.unwrap_or_default()),
                )
                .delimited_by(just(Token::LParen), just(Token::RParen)),
        )
        .then_ignore(just(Token::Semicolon).or_not())
        .map(|(msg, vars)| Stmt::Print { msg, vars });

    let report_format_stmt = just(Token::ReportFormatMacro)
        .ignore_then(
            select! {
                Token::Number(n) => Value::Number(n),
                Token::Ident(s) => Value::Ident(s.to_string()),
            }
            .then(just(Token::Comma).ignore_then(select! {
                Token::Ident(s) => s.to_string(),
            }))
            .then(just(Token::Comma).ignore_then(report_option_map()).or_not())
            .delimited_by(just(Token::LParen), just(Token::RParen)),
        )
        .then_ignore(just(Token::Semicolon).or_not())
        .map(|((kind, decoder), options)| Stmt::ReportFormat {
            kind,
            decoder,
            options: options.unwrap_or_default(),
        });

    let report_stmt = just(Token::ReportMacro)
        .ignore_then(
            select! {
                Token::Number(n) => Value::Number(n),
                Token::Ident(s) => Value::Ident(s.to_string()),
            }
            .then(
                just(Token::Comma)
                    .ignore_then(
                        expr()
                            .separated_by(just(Token::Comma))
                            .allow_trailing()
                            .collect::<Vec<_>>(),
                    )
                    .or_not()
                    .map(|v| v.unwrap_or_default()),
            )
            .delimited_by(just(Token::LParen), just(Token::RParen)),
        )
        .then_ignore(just(Token::Semicolon).or_not())
        .map(|(kind, values)| Stmt::Report { kind, values });

    // wait!(pin[, timeout_ms])：阻塞等中断边沿。timeout 缺省 4000ms。
    let wait_stmt = just(Token::WaitMacro)
        .ignore_then(
            select! { Token::Ident(s) => s.to_string() }
                .then(
                    just(Token::Comma)
                        .ignore_then(select! { Token::Number(n) => n })
                        .or_not()
                        .map(|t| t.unwrap_or(4000)),
                )
                .delimited_by(just(Token::LParen), just(Token::RParen)),
        )
        .then_ignore(just(Token::Semicolon).or_not())
        .map(|(pin, timeout_ms)| Stmt::Wait { pin, timeout_ms });

    chip_stmt
        .or(bus_stmt)
        .or(bus_probe_stmt)
        .or(let_stmt)
        .or(update_stmt)
        .or(write_stmt)
        .or(read_stmt)
        .or(print_stmt)
        .or(report_format_stmt)
        .or(report_stmt)
        .or(wait_stmt)
}

/// 构造 `repeat!(<N>) { <body>* }` 解析器，体使用传入的 `body` 解析器。
/// 抽出来是为了让 `block_stmt` 能用 `recursive` 把"自身"作为体传入，
/// 从而支持嵌套 repeat! 而不在构造期形成无限递归。
fn repeat_with<'tok, 'src: 'tok, I, B>(
    body: B,
) -> impl Parser<'tok, I, Stmt, ParserExtra<'tok, 'src>> + Clone + 'tok
where
    I: ValueInput<'tok, Token = Token<'src>, Span = SimpleSpan>,
    B: Parser<'tok, I, Stmt, ParserExtra<'tok, 'src>> + Clone + 'tok,
{
    just(Token::RepeatMacro)
        .ignore_then(
            select! { Token::Number(n) => n }
                .delimited_by(just(Token::LParen), just(Token::RParen)),
        )
        .then(
            body.repeated()
                .collect::<Vec<_>>()
                .delimited_by(just(Token::LBrace), just(Token::RBrace)),
        )
        .then_ignore(just(Token::Semicolon).or_not())
        .map(|(count, body)| Stmt::Repeat { count, body })
}

/// 构造 `if ( cond ) { body* } [ else { body* } | else <stmt> ]` 解析器，
/// 体/else 使用传入的 `body` 解析器。else 子句支持 `else { ... }` 与
/// `else if (...)`（单个 if 语句经 body 解析后包成 vec）。与 repeat_with 同构，
/// 让 block_stmt 能用 `recursive` 把"自身"作为体传入而不在构造期递归。
fn if_with<'tok, 'src: 'tok, I, B>(
    body: B,
) -> impl Parser<'tok, I, Stmt, ParserExtra<'tok, 'src>> + Clone + 'tok
where
    I: ValueInput<'tok, Token = Token<'src>, Span = SimpleSpan>,
    B: Parser<'tok, I, Stmt, ParserExtra<'tok, 'src>> + Clone + 'tok,
{
    let braced = body
        .clone()
        .repeated()
        .collect::<Vec<_>>()
        .delimited_by(just(Token::LBrace), just(Token::RBrace));
    let single = body.map(|s| vec![s]);
    just(Token::If)
        .ignore_then(expr().delimited_by(just(Token::LParen), just(Token::RParen)))
        .then(braced.clone())
        .then(just(Token::Else).ignore_then(braced.or(single)).or_not())
        .then_ignore(just(Token::Semicolon).or_not())
        .map(|((cond, then), else_)| Stmt::If {
            cond: Box::new(cond),
            then,
            else_: else_.unwrap_or_default(),
        })
}

/// 块内允许的语句集：普通语句 + 嵌套 repeat! + 嵌套 if（不含 irq!，与 irq! arm
/// 体只允许普通语句的约束对称）。用 `recursive` 提供自引用句柄 `r`，把它作为
/// repeat/if 体传入——构造期不调用自身，解析期按嵌套层数递归（有界）。
fn block_stmt<'tok, 'src: 'tok, I>()
-> impl Parser<'tok, I, Stmt, ParserExtra<'tok, 'src>> + Clone + 'tok
where
    I: ValueInput<'tok, Token = Token<'src>, Span = SimpleSpan>,
{
    recursive(|r| simple_stmt().or(repeat_with(r.clone())).or(if_with(r)))
}

/// 顶层 `repeat!(N) { <stmt>* }`：定长循环。count 仅接受数字字量；体可嵌套 repeat!/if。
fn repeat_stmt<'tok, 'src: 'tok, I>()
-> impl Parser<'tok, I, Stmt, ParserExtra<'tok, 'src>> + Clone + 'tok
where
    I: ValueInput<'tok, Token = Token<'src>, Span = SimpleSpan>,
{
    repeat_with(block_stmt())
}

/// 顶层 `if (cond) { ... } else { ... }`。体可嵌套 repeat!/if。
fn if_stmt<'tok, 'src: 'tok, I>()
-> impl Parser<'tok, I, Stmt, ParserExtra<'tok, 'src>> + Clone + 'tok
where
    I: ValueInput<'tok, Token = Token<'src>, Span = SimpleSpan>,
{
    if_with(block_stmt())
}

/// 解析 `irq!(pin) { on(event) { ... } ... }` 中断处理块。
/// 块内每个 `on(event)` 分支允许普通语句、`repeat!` 和 `if`，但不可再嵌套 `irq!`。
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
            block_stmt()
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
    simple_stmt()
        .or(irq_stmt())
        .or(repeat_stmt())
        .or(if_stmt())
        .map_with(
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
    /// `wait!(pin)` 引用的 pin 未由前置的 `irq!(pin)` 声明。
    WaitWithoutIrq(String),
    /// 中断状态快照宽于 4 字节，内联派发装不进单个 u32 寄存器。
    SnapshotTooWide(u32),
    /// `report!` 单条事件携带过多参数。
    ReportTooManyValues(usize),
    /// 这里需要 u32 标量变量，但拿到了 raw buffer 变量。
    ExpectedScalar(String),
    /// VM 第一版仅提供一个 raw data buffer slot。
    BufferOverflow,
    /// 未知的 `report!` 类型名。
    UnknownReportKind(String),
    /// I2C 总线选择缺少 7-bit 从机地址。
    MissingI2cAddress,
    /// I2C 从机地址不在 7-bit 有效范围内。
    InvalidI2cAddress(u32),
    /// `bus_probe!` 缺少必需选项。
    MissingProbeOption(&'static str),
    /// `bus_probe!` 选项值不合法。
    InvalidProbeOption(String),
    /// 未知的 `bus!` 总线类型。
    UnknownBusKind(String),
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
            Self::WaitWithoutIrq(pin) => {
                write!(
                    f,
                    "wait!({pin}) has no matching irq!({pin}) declaration before it"
                )
            }
            Self::SnapshotTooWide(w) => {
                write!(
                    f,
                    "interrupt status snapshot is {w} bytes wide, >4 not supported by inline dispatch"
                )
            }
            Self::ReportTooManyValues(n) => {
                write!(f, "report! supports at most 8 args, got {n}")
            }
            Self::ExpectedScalar(name) => {
                write!(f, "'{name}' is raw data; use it only as a report! argument")
            }
            Self::BufferOverflow => {
                write!(
                    f,
                    "only one raw data buffer variable is supported in this VM profile"
                )
            }
            Self::UnknownReportKind(name) => {
                write!(f, "unknown report kind '{name}'")
            }
            Self::MissingI2cAddress => {
                write!(f, "bus!(i2c) requires an explicit 7-bit address")
            }
            Self::InvalidI2cAddress(addr) => {
                write!(f, "invalid I2C address 0x{addr:x}; expected 0x01..0x7f")
            }
            Self::MissingProbeOption(name) => {
                write!(f, "bus_probe! requires option '{name}'")
            }
            Self::InvalidProbeOption(msg) => {
                write!(f, "invalid bus_probe! option: {msg}")
            }
            Self::UnknownBusKind(name) => {
                write!(f, "unknown bus kind '{name}', expected spi, i2c, or i3c")
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VarBinding {
    Reg(u8),
    Buf(u8),
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
    let mut next_buf: u8 = 0;
    let mut irqs = Vec::new();
    compile_into(
        program,
        base_dir,
        &mut registry,
        &mut bytecode,
        &mut vars,
        &mut next_reg,
        &mut next_buf,
        &mut irqs,
        &mut HashMap::new(),
    )?;
    bytecode.push(rseq_vm::Opcode::Return as u8);
    Ok(CompiledProgram {
        main: bytecode,
        irqs,
        irq_bytecodes: HashMap::new(),
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
pub fn compile_program_units(
    units: &[ProgramUnit<'_>],
) -> Result<CompiledProgram, SourceDiagnostic> {
    let mut registry = ChipRegistry::default();
    let mut main_bytecode = Vec::new();
    let mut vars = HashMap::new();
    let mut next_reg: u16 = 0;
    let mut next_buf: u8 = 0;
    let mut irqs = Vec::new();
    let mut irq_bytecodes: HashMap<String, Vec<u8>> = HashMap::new();

    for (unit_idx, unit) in units.iter().enumerate() {
        compile_into(
            unit.program,
            unit.base_dir,
            &mut registry,
            &mut main_bytecode,
            &mut vars,
            &mut next_reg,
            &mut next_buf,
            &mut irqs,
            &mut irq_bytecodes,
        )
        .map_err(|diag| SourceDiagnostic {
            unit: unit_idx,
            span: diag.span,
            message: diag.message,
            help: diag.help,
        })?;
    }

    main_bytecode.push(rseq_vm::Opcode::Return as u8);

    // 为每个 irq 段编译字节码
    for irq_tmpl in &irqs {
        if !irq_bytecodes.contains_key(&irq_tmpl.pin) {
            // 编译 irq 段：ReadVar 快照 + 派发逻辑
            let mut irq_bc = Vec::new();
            compile_irq_segment(&irq_tmpl, &registry, &mut irq_bc)?;
            irq_bytecodes.insert(irq_tmpl.pin.clone(), irq_bc);
        }
    }

    Ok(CompiledProgram {
        main: main_bytecode,
        irqs,
        irq_bytecodes,
    })
}

fn compile_into(
    program: &Program,
    base_dir: Option<&Path>,
    registry: &mut ChipRegistry,
    bytecode: &mut Vec<u8>,
    vars: &mut HashMap<String, VarBinding>,
    next_reg: &mut u16,
    next_buf: &mut u8,
    irqs: &mut Vec<IrqTemplate>,
    _irq_bytecodes: &mut HashMap<String, Vec<u8>>,
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
        if let Err(error) = compile_stmt(stmt, registry, vars, next_reg, next_buf, bytecode, irqs) {
            return Err(compile_diagnostic(program, idx, error));
        }
    }

    Ok(())
}

fn compile_stmt(
    stmt: &Stmt,
    registry: &ChipRegistry,
    vars: &mut HashMap<String, VarBinding>,
    next_reg: &mut u16,
    next_buf: &mut u8,
    bytecode: &mut Vec<u8>,
    irqs: &mut Vec<IrqTemplate>,
) -> Result<(), CompileError> {
    match stmt {
        Stmt::Chip { .. } => {}
        Stmt::Bus { kind, arg } => {
            let kind = resolve_bus_kind(kind)?;
            let arg = resolve_bus_arg(kind, *arg)?;
            bytecode.push(rseq_vm::Opcode::SetBus as u8);
            bytecode.push(kind as u8);
            bytecode.extend_from_slice(&arg.to_le_bytes());
        }
        Stmt::BusProbe { kind, options } => {
            let probe = resolve_bus_probe(kind, options, registry)?;
            bytecode.push(rseq_vm::Opcode::ProbeBus as u8);
            bytecode.push(probe.kind as u8);
            bytecode.push(probe.candidates.len() as u8);
            for candidate in &probe.candidates {
                bytecode.extend_from_slice(&candidate.to_le_bytes());
            }
            bytecode.extend_from_slice(&probe.read_addr.to_le_bytes());
            bytecode.extend_from_slice(&probe.len.to_le_bytes());
            bytecode.extend_from_slice(&probe.expect.to_le_bytes());
            bytecode.extend_from_slice(&probe.mask.to_le_bytes());
            bytecode.extend_from_slice(&probe.delay_us.to_le_bytes());
        }
        Stmt::Irq { pin, arms } => {
            let pin_id = irqs.len() as u8;
            let tmpl = compile_irq(pin, pin_id, arms, registry)?;
            irqs.push(tmpl);
        }
        Stmt::Wait { pin, timeout_ms } => {
            compile_wait(
                pin,
                *timeout_ms,
                irqs,
                registry,
                vars,
                next_reg,
                next_buf,
                bytecode,
            )?;
        }
        Stmt::Let { name, expr } => {
            if let Some(buf) = compile_read_buffer_binding(
                name, expr, registry, vars, next_reg, next_buf, bytecode,
            )? {
                vars.insert(name.clone(), VarBinding::Buf(buf));
            } else {
                let dst = compile_expr(expr, registry, vars, next_reg, bytecode)?;
                vars.insert(name.clone(), VarBinding::Reg(dst));
            }
        }
        Stmt::Write {
            addr,
            val,
            delay_us,
        } => {
            let delay = delay_us.unwrap_or(0);

            // write!(PAGE.REG, { field: value, ... }): build the register bytes
            // from the field values at compile time (no read — a deterministic
            // whole-byte set), then emit a plain Write. Bits outside the listed
            // fields are 0.
            if let Value::FieldMap(entries) = val {
                let target =
                    match addr {
                        Value::Ident(s) => s.as_str(),
                        _ => return Err(CompileError::Update(
                            "write!(REG, { field: value }) requires a named register (PAGE.REG)"
                                .into(),
                        )),
                    };
                let plan = registry
                    .plan_update(target, entries)
                    .map_err(CompileError::Chip)?;
                let data = fields_to_bytes(plan.width, &plan.fields);
                bytecode.push(rseq_vm::Opcode::Write as u8);
                bytecode.extend_from_slice(&plan.addr.to_le_bytes());
                bytecode.extend_from_slice(&(data.len() as u32).to_le_bytes());
                bytecode.extend_from_slice(&delay.to_le_bytes());
                bytecode.extend(data);
                return Ok(());
            }

            let addr = resolve_u32(addr, registry)?;

            // 写入一个由 let 绑定的变量：变量的值在运行期保存在寄存器里，
            // 因此发射 WriteVar，由 VM 在执行时把寄存器的低字节写入总线。
            if let Value::Ident(name) = val {
                let src = require_reg(vars, name)?;
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
        Stmt::Repeat { count, body } => {
            // count==0 或空体：no-op，直接不发射 Loop（VM 也会把 count==0 当 no-op，
            // 但跳过发射更省字节，且避免 body_len==0 的空 Loop 帧）。
            if *count == 0 || body.is_empty() {
                return Ok(());
            }
            // body 各语句编译进临时缓冲，共享外层 vars/next_reg/registry/irqs：
            // `let` 在每轮都重算到同一寄存器（寄存器在编译期只分配一次），无作用域，
            // 与既有 `let` 语义一致。body 只编译一份，VM 靠 Loop 计数回跳复用。
            let mut body_buf: Vec<u8> = Vec::new();
            for s in body {
                compile_stmt(s, registry, vars, next_reg, next_buf, &mut body_buf, irqs)?;
            }
            if body_buf.is_empty() {
                return Ok(());
            }
            bytecode.push(rseq_vm::Opcode::Loop as u8);
            bytecode.extend_from_slice(&count.to_le_bytes());
            bytecode.extend_from_slice(&(body_buf.len() as u32).to_le_bytes());
            bytecode.extend(&body_buf);
        }
        Stmt::Read {
            addr,
            len,
            delay_us,
        } => {
            let addr = resolve_u32(addr, registry)?;
            let delay = delay_us.unwrap_or(0);

            // len 可以是常量或变量
            match len {
                Value::Number(n) => {
                    // 编译期常量：发射 Read 操作码
                    bytecode.push(rseq_vm::Opcode::Read as u8);
                    bytecode.extend_from_slice(&addr.to_le_bytes());
                    bytecode.extend_from_slice(&n.to_le_bytes());
                    bytecode.extend_from_slice(&delay.to_le_bytes());
                }
                Value::Ident(name) => {
                    // 运行时变量：发射 ReadDyn，按寄存器中的长度读取并丢弃结果。
                    // 这用于 FIFO drain：先 `let fifo_len = read!(STATUS...)`，
                    // 再 `read!(FIFO_DATA, fifo_len)` 精确读取当前 FIFO 字节数。
                    let len_reg = require_reg(vars, name)?;
                    bytecode.push(rseq_vm::Opcode::ReadDyn as u8);
                    bytecode.extend_from_slice(&addr.to_le_bytes());
                    bytecode.extend_from_slice(&delay.to_le_bytes());
                    bytecode.push(len_reg);
                }
                _ => return Err(CompileError::UnsupportedValue),
            }
        }
        Stmt::If { cond, then, else_ } => {
            // cond 编译进主字节码流，得到 cond_reg。
            let cond_reg = compile_expr(cond, registry, vars, next_reg, bytecode)?;
            // 体先编译进临时缓冲（共享外层 vars/next_reg/registry），长度已知后再发射跳转。
            let mut then_buf: Vec<u8> = Vec::new();
            for s in then {
                compile_stmt(s, registry, vars, next_reg, next_buf, &mut then_buf, irqs)?;
            }
            let mut else_buf: Vec<u8> = Vec::new();
            for s in else_ {
                compile_stmt(s, registry, vars, next_reg, next_buf, &mut else_buf, irqs)?;
            }

            // Jump 指令长度 = 操作码(1) + i32 偏移(4)。
            const JUMP_INSTR_LEN: usize = 1 + 4;

            if else_buf.is_empty() {
                // if (cond) { then }：cond==0 跳过 then 体。
                //   JumpIfZero cond_reg, +then_len
                //   <then>
                bytecode.push(rseq_vm::Opcode::JumpIfZero as u8);
                bytecode.push(cond_reg);
                let off = then_buf.len() as i32;
                bytecode.extend_from_slice(&off.to_le_bytes());
                bytecode.extend(&then_buf);
            } else {
                // if (cond) { then } else { else }：
                //   JumpIfZero cond_reg, +(then_len + JUMP_INSTR_LEN)  → 跳到 else
                //   <then>
                //   Jump +else_len                                            → 跳过 else
                //   <else>
                bytecode.push(rseq_vm::Opcode::JumpIfZero as u8);
                bytecode.push(cond_reg);
                let off = (then_buf.len() + JUMP_INSTR_LEN) as i32;
                bytecode.extend_from_slice(&off.to_le_bytes());
                bytecode.extend(&then_buf);
                bytecode.push(rseq_vm::Opcode::Jump as u8);
                let off2 = else_buf.len() as i32;
                bytecode.extend_from_slice(&off2.to_le_bytes());
                bytecode.extend(&else_buf);
            }
        }
        Stmt::Print { msg, vars: pvars } => {
            let bytes = msg.as_bytes();
            if pvars.is_empty() {
                // print!("msg") → 纯 Log。
                bytecode.push(rseq_vm::Opcode::Log as u8);
                bytecode.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
                bytecode.extend(bytes);
            } else {
                // print!("fmt", v1, v2, ...) → LogVar | n_vars | regs... | fmt_len | fmt
                if pvars.len() > 8 {
                    return Err(CompileError::Update(
                        "print! supports at most 8 variables".into(),
                    ));
                }
                bytecode.push(rseq_vm::Opcode::LogVar as u8);
                bytecode.push(pvars.len() as u8);
                for name in pvars {
                    let reg = require_reg(vars, name)?;
                    bytecode.push(reg);
                }
                bytecode.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
                bytecode.extend(bytes);
            }
        }
        Stmt::ReportFormat { .. } => {
            // Host-side decode metadata only. It intentionally emits no VM bytecode.
        }
        Stmt::Report { kind, values } => {
            if values.len() > 8 {
                return Err(CompileError::ReportTooManyValues(values.len()));
            }
            let kind = resolve_report_kind(kind)?;
            let mut args = Vec::with_capacity(values.len());
            for value in values {
                if let Expr::Ident(name) = value {
                    if let Some(VarBinding::Buf(buf)) = vars.get(name) {
                        args.push((rseq_vm::REPORT_ARG_BYTES, *buf));
                        continue;
                    }
                }
                let reg = compile_expr(value, registry, vars, next_reg, bytecode)?;
                args.push((rseq_vm::REPORT_ARG_U32, reg));
            }

            bytecode.push(rseq_vm::Opcode::Report as u8);
            bytecode.extend_from_slice(&kind.to_le_bytes());
            bytecode.push(args.len() as u8);
            for (tag, idx) in args {
                bytecode.push(tag);
                bytecode.push(idx);
            }
        }
    }
    Ok(())
}

/// 把一个 `irq!(pin) { on(event){...} ... }` 块编译成方案 A 的派发表。
///
/// 状态快照优先使用芯片字典里 `interrupt_status_snapshot` 角色的寄存器
/// （一次读取覆盖整组状态位，满足 read_clear 只读一次的约束）；若芯片未
/// 声明快照视图，则退化为"所有事件必须位于同一个状态寄存器"的单寄存器读取。
/// 把一个 `irq!(pin) { on(event){...} ... }` 块整理成 [`IrqTemplate`]。
///
/// 复用芯片字典的 `interrupt_status_snapshot` 角色确定快照基址/宽度/
/// read_clear，再用 `resolve_event` 算各事件在快照里的位掩码。**不再预编译
/// 独立处理段**：arm 体以 AST 克隆存入模板，由 `wait!(pin)` 在编译期内联进
/// main 字节码。`pin_id` 为该模板在 `irqs` 中的下标，`WaitIrq` 操作码携带它。
fn compile_irq(
    pin: &str,
    pin_id: u8,
    arms: &[IrqArm],
    registry: &ChipRegistry,
) -> Result<IrqTemplate, CompileError> {
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
            let first = resolved
                .first()
                .ok_or(CompileError::NoInterruptStatus)?
                .1
                .clone();
            if resolved
                .iter()
                .any(|(_, eb)| eb.status_addr != first.status_addr)
            {
                return Err(CompileError::NoInterruptStatus);
            }
            (first.status_addr, 1, first.read_clear)
        }
    };

    // 内联派发把快照读进单个 u32 寄存器，故快照宽于 4 字节则不支持。
    if snapshot_len == 0 || snapshot_len > 4 {
        return Err(CompileError::SnapshotTooWide(snapshot_len));
    }

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

        bins.push(IrqArmTpl {
            event: arm.event.clone(),
            mask,
            body: arm.body.clone(),
        });
    }

    Ok(IrqTemplate {
        pin: pin.to_string(),
        pin_id,
        snapshot_addr,
        snapshot_len,
        read_clear,
        arms: bins,
    })
}

/// 把 `wait!(pin[, timeout_ms])` 编译成内联派发序列。
///
/// 字节码布局（紧跟在调用点的 main 流中）：
/// ```text
/// WaitIrq   pin_id, timeout_ms
/// ReadVar   snapshot_addr, snapshot_len, delay=0  -> r_snap
/// LoadConst r_mask, 0           // 占位，每 arm 前用 mask 覆写立即数
/// And       r_test, r_snap, r_mask
/// 对每个 arm：
///   LoadConst r_mask, (mask as u32)   // 覆写掩码
///   And       r_test, r_snap, r_mask
///   JumpIfZero r_test, +(body_len)   // 跳过本 arm 体
///   <arm.body 各 stmt 内联编译>
/// ```
/// `r_snap`/`r_mask`/`r_test` 各分配一次、跨 arm 复用；arm 体共享 main 的
/// 变量/寄存器空间。`mask` 截到低 32 位（snapshot_len≤4 保证高位为 0）。
fn compile_wait(
    pin: &str,
    timeout_ms: u32,
    irqs: &[IrqTemplate],
    registry: &ChipRegistry,
    vars: &mut HashMap<String, VarBinding>,
    next_reg: &mut u16,
    next_buf: &mut u8,
    bytecode: &mut Vec<u8>,
) -> Result<(), CompileError> {
    let tmpl = irqs
        .iter()
        .find(|t| t.pin == pin)
        .ok_or_else(|| CompileError::WaitWithoutIrq(pin.to_string()))?;
    let pin_id = tmpl.pin_id;
    let snapshot_addr = tmpl.snapshot_addr;
    let snapshot_len = tmpl.snapshot_len;

    // WaitIrq pin_id, timeout_ms
    bytecode.push(rseq_vm::Opcode::WaitIrq as u8);
    bytecode.push(pin_id);
    bytecode.extend_from_slice(&timeout_ms.to_le_bytes());

    // ReadVar snapshot_addr, snapshot_len, delay=0 -> r_snap
    let r_snap = alloc_reg(next_reg)?;
    bytecode.push(rseq_vm::Opcode::ReadVar as u8);
    bytecode.extend_from_slice(&snapshot_addr.to_le_bytes());
    bytecode.extend_from_slice(&snapshot_len.to_le_bytes());
    bytecode.extend_from_slice(&0u32.to_le_bytes()); // delay
    bytecode.push(r_snap);

    // r_mask / r_test 各分配一次，跨 arm 复用。
    let r_mask = alloc_reg(next_reg)?;
    let r_test = alloc_reg(next_reg)?;

    for arm in &tmpl.arms {
        let mask32 = arm.mask as u32;
        // LoadConst r_mask, mask32
        bytecode.push(rseq_vm::Opcode::LoadConst as u8);
        bytecode.push(r_mask);
        bytecode.extend_from_slice(&mask32.to_le_bytes());
        // And r_test, r_snap, r_mask
        bytecode.push(rseq_vm::Opcode::And as u8);
        bytecode.push(r_test);
        bytecode.push(r_snap);
        bytecode.push(r_mask);

        // 先把 arm 体编译进临时缓冲（共享外层 vars/next_reg，与 `if` 体同构），
        // 量出长度后再发 JumpIfZero。
        let mut body_buf: Vec<u8> = Vec::new();
        for s in &arm.body {
            compile_stmt(
                s,
                registry,
                vars,
                next_reg,
                next_buf,
                &mut body_buf,
                &mut Vec::new(),
            )?;
        }

        // JumpIfZero r_test, +body_len
        bytecode.push(rseq_vm::Opcode::JumpIfZero as u8);
        bytecode.push(r_test);
        bytecode.extend_from_slice(&(body_buf.len() as i32).to_le_bytes());
        bytecode.extend_from_slice(&body_buf);
    }

    Ok(())
}

/// 把 `IrqTemplate` 编译成独立的中断处理段字节码（供 MCU 自动响应）。
///
/// 字节码布局：
/// ```text
/// ReadVar   snapshot_addr, snapshot_len, delay=0  -> r_snap
/// 对每个 arm：
///   LoadConst r_mask, (mask as u32)
///   And       r_test, r_snap, r_mask
///   JumpIfZero r_test, +(body_len)
///   <arm.body 各 stmt 编译>
/// Return
/// ```
fn compile_irq_segment(
    tmpl: &IrqTemplate,
    registry: &ChipRegistry,
    bytecode: &mut Vec<u8>,
) -> Result<(), SourceDiagnostic> {
    let mut vars = HashMap::new();
    let mut next_reg: u16 = 0;
    let mut next_buf: u8 = 0;

    // ReadVar snapshot_addr, snapshot_len, delay=0 -> r_snap
    let r_snap = alloc_reg(&mut next_reg).map_err(|e| SourceDiagnostic {
        unit: 0,
        span: 0..0,
        message: format!("IRQ segment register allocation failed: {}", e),
        help: None,
    })?;
    bytecode.push(rseq_vm::Opcode::ReadVar as u8);
    bytecode.extend_from_slice(&tmpl.snapshot_addr.to_le_bytes());
    bytecode.extend_from_slice(&tmpl.snapshot_len.to_le_bytes());
    bytecode.extend_from_slice(&0u32.to_le_bytes()); // delay
    bytecode.push(r_snap);

    // r_mask / r_test 各分配一次，跨 arm 复用
    let r_mask = alloc_reg(&mut next_reg).map_err(|e| SourceDiagnostic {
        unit: 0,
        span: 0..0,
        message: format!("IRQ segment register allocation failed: {}", e),
        help: None,
    })?;
    let r_test = alloc_reg(&mut next_reg).map_err(|e| SourceDiagnostic {
        unit: 0,
        span: 0..0,
        message: format!("IRQ segment register allocation failed: {}", e),
        help: None,
    })?;

    for arm in &tmpl.arms {
        let mask32 = arm.mask as u32;
        // LoadConst r_mask, mask32
        bytecode.push(rseq_vm::Opcode::LoadConst as u8);
        bytecode.push(r_mask);
        bytecode.extend_from_slice(&mask32.to_le_bytes());
        // And r_test, r_snap, r_mask
        bytecode.push(rseq_vm::Opcode::And as u8);
        bytecode.push(r_test);
        bytecode.push(r_snap);
        bytecode.push(r_mask);

        // 编译 arm 体
        let mut body_buf: Vec<u8> = Vec::new();
        for s in &arm.body {
            compile_stmt(
                s,
                registry,
                &mut vars,
                &mut next_reg,
                &mut next_buf,
                &mut body_buf,
                &mut Vec::new(),
            )
            .map_err(|e| SourceDiagnostic {
                unit: 0,
                span: 0..0,
                message: format!("IRQ arm body compilation failed: {}", e),
                help: None,
            })?;
        }

        // JumpIfZero r_test, +body_len
        bytecode.push(rseq_vm::Opcode::JumpIfZero as u8);
        bytecode.push(r_test);
        bytecode.extend_from_slice(&(body_buf.len() as i32).to_le_bytes());
        bytecode.extend_from_slice(&body_buf);
    }

    // Return
    bytecode.push(rseq_vm::Opcode::Return as u8);

    Ok(())
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

fn alloc_buf(next_buf: &mut u8) -> Result<u8, CompileError> {
    if *next_buf >= rseq_vm::DATA_BUF_COUNT as u8 {
        return Err(CompileError::BufferOverflow);
    }
    let buf = *next_buf;
    *next_buf += 1;
    Ok(buf)
}

fn require_reg(vars: &HashMap<String, VarBinding>, name: &str) -> Result<u8, CompileError> {
    match vars.get(name) {
        Some(VarBinding::Reg(reg)) => Ok(*reg),
        Some(VarBinding::Buf(_)) => Err(CompileError::ExpectedScalar(name.to_string())),
        None => Err(CompileError::UndefinedVariable(name.to_string())),
    }
}

fn resolve_report_kind(kind: &Value) -> Result<u32, CompileError> {
    match kind {
        Value::Number(n) => Ok(*n),
        Value::Ident(name) => {
            report_kind_value(name).ok_or_else(|| CompileError::UnknownReportKind(name.clone()))
        }
        _ => Err(CompileError::UnsupportedValue),
    }
}

fn compile_len_to_reg(
    len: &Value,
    vars: &HashMap<String, VarBinding>,
    next_reg: &mut u16,
    bytecode: &mut Vec<u8>,
) -> Result<Option<u8>, CompileError> {
    match len {
        Value::Number(n) if *n > 4 => {
            if *n > rseq_vm::DATA_BUF_LEN as u32 {
                return Err(CompileError::UnsupportedValue);
            }
            let reg = alloc_reg(next_reg)?;
            bytecode.push(rseq_vm::Opcode::LoadConst as u8);
            bytecode.push(reg);
            bytecode.extend_from_slice(&n.to_le_bytes());
            Ok(Some(reg))
        }
        Value::Number(_) => Ok(None),
        Value::Ident(name) => Ok(Some(require_reg(vars, name)?)),
        _ => Err(CompileError::UnsupportedValue),
    }
}

fn compile_read_buffer_binding(
    name: &str,
    expr: &Expr,
    registry: &ChipRegistry,
    vars: &mut HashMap<String, VarBinding>,
    next_reg: &mut u16,
    next_buf: &mut u8,
    bytecode: &mut Vec<u8>,
) -> Result<Option<u8>, CompileError> {
    let Expr::Read {
        addr,
        len,
        delay_us,
    } = expr
    else {
        return Ok(None);
    };

    let Some(len_reg) = compile_len_to_reg(len, vars, next_reg, bytecode)? else {
        return Ok(None);
    };

    let addr = resolve_u32(addr, registry)?;
    let delay = delay_us.unwrap_or(0);
    let buf = match vars.get(name) {
        Some(VarBinding::Buf(existing)) => *existing,
        _ => alloc_buf(next_buf)?,
    };

    bytecode.push(rseq_vm::Opcode::ReadBuf as u8);
    bytecode.extend_from_slice(&addr.to_le_bytes());
    bytecode.extend_from_slice(&delay.to_le_bytes());
    bytecode.push(len_reg);
    bytecode.push(buf);

    Ok(Some(buf))
}

/// 将表达式编译为字节码，返回存放结果的寄存器索引。
fn compile_expr(
    expr: &Expr,
    registry: &ChipRegistry,
    vars: &mut HashMap<String, VarBinding>,
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
        Expr::Ident(name) => require_reg(vars, name),
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
                BinOp::Eq => rseq_vm::Opcode::CmpEq,
                BinOp::Ne => rseq_vm::Opcode::CmpNe,
                BinOp::Lt => rseq_vm::Opcode::CmpLt,
                BinOp::Le => rseq_vm::Opcode::CmpLe,
                BinOp::Gt => rseq_vm::Opcode::CmpGt,
                BinOp::Ge => rseq_vm::Opcode::CmpGe,
                BinOp::AndAnd => rseq_vm::Opcode::LogAnd,
                BinOp::OrOr => rseq_vm::Opcode::LogOr,
            };
            bytecode.push(opcode as u8);
            bytecode.push(dst);
            bytecode.push(lhs_reg);
            bytecode.push(rhs_reg);
            Ok(dst)
        }
        Expr::Unary { op, expr } => {
            let src = compile_expr(expr, registry, vars, next_reg, bytecode)?;
            let dst = alloc_reg(next_reg)?;
            let opcode = match op {
                UnaryOp::Not => rseq_vm::Opcode::LogNot,
            };
            bytecode.push(opcode as u8);
            bytecode.push(dst);
            bytecode.push(src);
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
        CompileError::ExpectedScalar(name) => Some(format!(
            "`{name}` holds raw bytes; pass it to report! directly instead of using it in arithmetic, print!, write!, or length expressions"
        )),
        CompileError::BufferOverflow => Some(
            "reuse the same raw data variable name, or report the current buffer before reading another raw payload".to_string(),
        ),
        CompileError::UnknownReportKind(name) => Some(format!(
            "use a numeric kind like 0x01, or one of the built-in kind names FIFO_RAW, AMD, SMD, or DRDY; '{name}' is not defined"
        )),
        CompileError::UnknownEvent(name) => Some(format!(
            "'{name}' is not a known interrupt event; use an `event:` name declared on an interrupt_status field in the chip YAML"
        )),
        CompileError::NoInterruptStatus => Some(
            "declare an interrupt status register (role interrupt_status) and ideally an interrupt_status_snapshot view in the chip YAML so irq! events can be dispatched in a single read".to_string(),
        ),
        CompileError::UnknownBusKind(_) => Some(
            "use bus!(spi), bus!(i2c, 0x6a), or bus!(i3c) before the reads/writes that should use that physical bus".to_string(),
        ),
        CompileError::MissingI2cAddress => Some(
            "the MCU firmware is a generic bridge and does not know chip-specific candidate addresses; put the address in the DSL, for example bus!(i2c, 0x6a)".to_string(),
        ),
        CompileError::InvalidI2cAddress(_) => Some(
            "use a 7-bit I2C slave address; 0 is reserved as 'unset' in the VM bus selector".to_string(),
        ),
        CompileError::MissingProbeOption(_) | CompileError::InvalidProbeOption(_) => Some(
            "use bus_probe!(spi, { read: REG, expect: 0xNN }) or bus_probe!(i2c, { addrs: [0x6a, 0x6b], read: REG, expect: 0xNN })".to_string(),
        ),
        _ => None,
    }
}

struct BusProbeConfig {
    kind: rseq_vm::BusKind,
    candidates: Vec<u32>,
    read_addr: u32,
    len: u32,
    expect: u32,
    mask: u32,
    delay_us: u32,
}

fn resolve_bus_probe(
    kind: &str,
    options: &[(String, Value)],
    registry: &ChipRegistry,
) -> Result<BusProbeConfig, CompileError> {
    let kind = resolve_bus_kind(kind)?;
    let mut addrs: Option<Vec<u32>> = None;
    let mut read: Option<Value> = None;
    let mut expect: Option<Value> = None;
    let mut len: u32 = 1;
    let mut mask: Option<u32> = None;
    let mut delay_us: u32 = 0;

    for (name, value) in options {
        match name.as_str() {
            "addrs" | "args" => addrs = Some(resolve_probe_candidates(value)?),
            "read" | "reg" => read = Some(value.clone()),
            "expect" => expect = Some(value.clone()),
            "len" => {
                len = resolve_probe_number(value, registry)?;
            }
            "mask" => {
                mask = Some(resolve_probe_number(value, registry)?);
            }
            "delay_us" | "delay" => {
                delay_us = resolve_probe_number(value, registry)?;
            }
            _ => {
                return Err(CompileError::InvalidProbeOption(format!(
                    "unknown option '{name}'"
                )));
            }
        }
    }

    if len == 0 || len > 4 {
        return Err(CompileError::InvalidProbeOption(format!(
            "len must be 1..4, got {len}"
        )));
    }

    let candidates = match kind {
        rseq_vm::BusKind::I2c => {
            let addrs = addrs.ok_or(CompileError::MissingProbeOption("addrs"))?;
            if addrs.is_empty() {
                return Err(CompileError::InvalidProbeOption(
                    "addrs must not be empty".to_string(),
                ));
            }
            addrs
                .into_iter()
                .map(|addr| resolve_bus_arg(kind, Some(addr)))
                .collect::<Result<Vec<_>, _>>()?
        }
        rseq_vm::BusKind::Spi | rseq_vm::BusKind::I3c => addrs.unwrap_or_else(|| vec![0]),
    };
    if candidates.len() > rseq_vm::PROBE_MAX_CANDIDATES {
        return Err(CompileError::InvalidProbeOption(format!(
            "candidate count {} exceeds {}",
            candidates.len(),
            rseq_vm::PROBE_MAX_CANDIDATES
        )));
    }

    let read = read.ok_or(CompileError::MissingProbeOption("read"))?;
    let expect = expect.ok_or(CompileError::MissingProbeOption("expect"))?;
    let read_addr = resolve_u32(&read, registry)?;
    let expect = resolve_probe_number(&expect, registry)?;
    let default_mask = if len >= 4 {
        u32::MAX
    } else {
        (1u32 << (len * 8)) - 1
    };

    Ok(BusProbeConfig {
        kind,
        candidates,
        read_addr,
        len,
        expect,
        mask: mask.unwrap_or(default_mask),
        delay_us,
    })
}

fn resolve_probe_number(value: &Value, registry: &ChipRegistry) -> Result<u32, CompileError> {
    match value {
        Value::Number(n) => Ok(*n),
        Value::Ident(_) => resolve_u32(value, registry),
        _ => Err(CompileError::InvalidProbeOption(
            "expected scalar number or register name".to_string(),
        )),
    }
}

fn resolve_probe_candidates(value: &Value) -> Result<Vec<u32>, CompileError> {
    let Value::Array(values) = value else {
        return Err(CompileError::InvalidProbeOption(
            "addrs must be an array of numbers".to_string(),
        ));
    };
    values
        .iter()
        .map(|value| match value {
            Value::Number(n) => Ok(*n),
            _ => Err(CompileError::InvalidProbeOption(
                "addrs must contain only numbers".to_string(),
            )),
        })
        .collect()
}

fn resolve_bus_kind(kind: &str) -> Result<rseq_vm::BusKind, CompileError> {
    match kind {
        "spi" | "SPI" => Ok(rseq_vm::BusKind::Spi),
        "i2c" | "I2C" => Ok(rseq_vm::BusKind::I2c),
        "i3c" | "I3C" => Ok(rseq_vm::BusKind::I3c),
        _ => Err(CompileError::UnknownBusKind(kind.to_string())),
    }
}

fn resolve_bus_arg(kind: rseq_vm::BusKind, arg: Option<u32>) -> Result<u32, CompileError> {
    match kind {
        rseq_vm::BusKind::I2c => {
            let addr = arg.ok_or(CompileError::MissingI2cAddress)?;
            if addr == 0 || addr > 0x7f {
                return Err(CompileError::InvalidI2cAddress(addr));
            }
            Ok(addr)
        }
        rseq_vm::BusKind::Spi | rseq_vm::BusKind::I3c => Ok(arg.unwrap_or(0)),
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
    decompile_block(bytecode)
}

/// 反编译一个指令块（顶层或 `repeat!` 体）。递归处理 `Loop`：对 body 子切片
/// 再调用本函数，按 2 空格缩进拼回 `repeat!(N) { ... }`。
fn decompile_block(bytecode: &[u8]) -> Result<String, DecompileError> {
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
            Some(rseq_vm::Opcode::SetBus) => {
                pc += 1;
                if pc + 5 > bytecode.len() {
                    return Err(DecompileError::UnexpectedEnd);
                }
                let kind =
                    rseq_vm::BusKind::from_u8(bytecode[pc]).ok_or(DecompileError::InvalidOpcode)?;
                pc += 1;
                let arg = read_u32(bytecode, &mut pc)?;
                if arg == 0 {
                    output.push_str(&format!("bus!({});\n", kind.as_str()));
                } else {
                    output.push_str(&format!("bus!({}, 0x{arg:x});\n", kind.as_str()));
                }
            }
            Some(rseq_vm::Opcode::ProbeBus) => {
                pc += 1;
                if pc + 2 > bytecode.len() {
                    return Err(DecompileError::UnexpectedEnd);
                }
                let kind =
                    rseq_vm::BusKind::from_u8(bytecode[pc]).ok_or(DecompileError::InvalidOpcode)?;
                pc += 1;
                let count = bytecode[pc] as usize;
                pc += 1;
                if count == 0 || count > rseq_vm::PROBE_MAX_CANDIDATES {
                    return Err(DecompileError::InvalidOpcode);
                }
                if pc + count * 4 + 20 > bytecode.len() {
                    return Err(DecompileError::UnexpectedEnd);
                }
                let mut candidates = Vec::with_capacity(count);
                for _ in 0..count {
                    candidates.push(read_u32(bytecode, &mut pc)?);
                }
                let addr = read_u32(bytecode, &mut pc)?;
                let len = read_u32(bytecode, &mut pc)?;
                let expect = read_u32(bytecode, &mut pc)?;
                let mask = read_u32(bytecode, &mut pc)?;
                let delay = read_u32(bytecode, &mut pc)?;
                output.push_str(&format!("bus_probe!({}, {{ ", kind.as_str()));
                if kind == rseq_vm::BusKind::I2c {
                    output.push_str("addrs: [");
                    for (idx, candidate) in candidates.iter().enumerate() {
                        if idx != 0 {
                            output.push_str(", ");
                        }
                        output.push_str(&format!("0x{candidate:x}"));
                    }
                    output.push_str("], ");
                }
                output.push_str(&format!("read: 0x{addr:x}, expect: 0x{expect:x}"));
                if len != 1 {
                    output.push_str(&format!(", len: {len}"));
                }
                let default_mask = if len >= 4 {
                    u32::MAX
                } else {
                    (1u32 << (len * 8)) - 1
                };
                if mask != default_mask {
                    output.push_str(&format!(", mask: 0x{mask:x}"));
                }
                if delay > 0 {
                    output.push_str(&format!(", delay_us: {delay}"));
                }
                output.push_str(" });\n");
            }
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
            Some(rseq_vm::Opcode::Loop) => {
                pc += 1;
                let count = read_u32(bytecode, &mut pc)?;
                let body_len = read_u32(bytecode, &mut pc)? as usize;
                if pc + body_len > bytecode.len() {
                    return Err(DecompileError::UnexpectedEnd);
                }
                let inner = decompile_block(&bytecode[pc..pc + body_len])?;
                pc += body_len;
                output.push_str(&format!("repeat!({count}) {{\n"));
                for line in inner.lines() {
                    output.push_str("  ");
                    output.push_str(line);
                    output.push('\n');
                }
                output.push_str("}\n");
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
            Some(
                op @ (rseq_vm::Opcode::Add
                | rseq_vm::Opcode::Sub
                | rseq_vm::Opcode::Mul
                | rseq_vm::Opcode::Div
                | rseq_vm::Opcode::Mod
                | rseq_vm::Opcode::Shl
                | rseq_vm::Opcode::Shr
                | rseq_vm::Opcode::And
                | rseq_vm::Opcode::Or
                | rseq_vm::Opcode::Xor),
            ) => {
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
            Some(
                op @ (rseq_vm::Opcode::CmpEq
                | rseq_vm::Opcode::CmpNe
                | rseq_vm::Opcode::CmpLt
                | rseq_vm::Opcode::CmpLe
                | rseq_vm::Opcode::CmpGt
                | rseq_vm::Opcode::CmpGe),
            ) => {
                pc += 1;
                if pc + 2 >= bytecode.len() {
                    return Err(DecompileError::UnexpectedEnd);
                }
                let dst = bytecode[pc];
                let lhs = bytecode[pc + 1];
                let rhs = bytecode[pc + 2];
                pc += 3;
                let op_str = match op {
                    rseq_vm::Opcode::CmpEq => "==",
                    rseq_vm::Opcode::CmpNe => "!=",
                    rseq_vm::Opcode::CmpLt => "<",
                    rseq_vm::Opcode::CmpLe => "<=",
                    rseq_vm::Opcode::CmpGt => ">",
                    rseq_vm::Opcode::CmpGe => ">=",
                    _ => unreachable!(),
                };
                output.push_str(&format!("// r{dst} = r{lhs} {op_str} r{rhs}\n"));
            }
            Some(op @ (rseq_vm::Opcode::LogAnd | rseq_vm::Opcode::LogOr)) => {
                pc += 1;
                if pc + 2 >= bytecode.len() {
                    return Err(DecompileError::UnexpectedEnd);
                }
                let dst = bytecode[pc];
                let lhs = bytecode[pc + 1];
                let rhs = bytecode[pc + 2];
                pc += 3;
                let op_str = match op {
                    rseq_vm::Opcode::LogAnd => "&&",
                    rseq_vm::Opcode::LogOr => "||",
                    _ => unreachable!(),
                };
                output.push_str(&format!("// r{dst} = r{lhs} {op_str} r{rhs}\n"));
            }
            Some(rseq_vm::Opcode::LogNot) => {
                pc += 1;
                if pc + 1 >= bytecode.len() {
                    return Err(DecompileError::UnexpectedEnd);
                }
                let dst = bytecode[pc];
                let src = bytecode[pc + 1];
                pc += 2;
                output.push_str(&format!("// r{dst} = !r{src}\n"));
            }
            Some(rseq_vm::Opcode::JumpIfZero) => {
                pc += 1;
                if pc >= bytecode.len() {
                    return Err(DecompileError::UnexpectedEnd);
                }
                let cond = bytecode[pc];
                pc += 1;
                let off = read_u32(bytecode, &mut pc)? as i32;
                output.push_str(&format!("// if r{cond} == 0 goto +{off}\n"));
            }
            Some(rseq_vm::Opcode::Jump) => {
                pc += 1;
                let off = read_u32(bytecode, &mut pc)? as i32;
                output.push_str(&format!("// goto +{off}\n"));
            }
            Some(rseq_vm::Opcode::Log) => {
                pc += 1;
                let len = read_u32(bytecode, &mut pc)? as usize;
                if pc + len > bytecode.len() {
                    return Err(DecompileError::UnexpectedEnd);
                }
                let msg = core::str::from_utf8(&bytecode[pc..pc + len])
                    .map_err(|_| DecompileError::InvalidOpcode)?;
                pc += len;
                output.push_str(&format!("print!({msg:?});\n"));
            }
            Some(rseq_vm::Opcode::LogVar) => {
                pc += 1;
                if pc >= bytecode.len() {
                    return Err(DecompileError::UnexpectedEnd);
                }
                let n = bytecode[pc] as usize;
                pc += 1;
                if pc + n > bytecode.len() {
                    return Err(DecompileError::UnexpectedEnd);
                }
                let regs: Vec<u8> = bytecode[pc..pc + n].to_vec();
                pc += n;
                let fmt_len = read_u32(bytecode, &mut pc)? as usize;
                if pc + fmt_len > bytecode.len() {
                    return Err(DecompileError::UnexpectedEnd);
                }
                let fmt = core::str::from_utf8(&bytecode[pc..pc + fmt_len])
                    .map_err(|_| DecompileError::InvalidOpcode)?;
                pc += fmt_len;
                let args: Vec<String> = regs.iter().map(|r| format!("r{r}")).collect();
                output.push_str(&format!(
                    "print!({fmt:?}{});\n",
                    if args.is_empty() {
                        String::new()
                    } else {
                        format!(", {}", args.join(", "))
                    }
                ));
            }
            Some(rseq_vm::Opcode::Report) => {
                pc += 1;
                let kind = read_u32(bytecode, &mut pc)?;
                if pc >= bytecode.len() {
                    return Err(DecompileError::UnexpectedEnd);
                }
                let n = bytecode[pc] as usize;
                pc += 1;
                if n > 8 || pc + n * 2 > bytecode.len() {
                    return Err(DecompileError::UnexpectedEnd);
                }
                let mut args = Vec::with_capacity(n);
                for _ in 0..n {
                    let tag = bytecode[pc];
                    let idx = bytecode[pc + 1];
                    pc += 2;
                    match tag {
                        rseq_vm::REPORT_ARG_U32 => args.push(format!("r{idx}")),
                        rseq_vm::REPORT_ARG_BYTES => args.push(format!("buf{idx}")),
                        _ => return Err(DecompileError::InvalidOpcode),
                    }
                }
                let kind_expr = report_kind_name(kind)
                    .map_or_else(|| format!("0x{kind:x}"), |name| name.to_string());
                output.push_str(&format!("report!({kind_expr}"));
                if !args.is_empty() {
                    output.push_str(", ");
                    output.push_str(&args.join(", "));
                }
                output.push_str(");\n");
            }
            Some(rseq_vm::Opcode::WaitIrq) => {
                pc += 1;
                if pc + 4 >= bytecode.len() {
                    return Err(DecompileError::UnexpectedEnd);
                }
                let pin = bytecode[pc];
                pc += 1;
                let timeout_ms = read_u32(bytecode, &mut pc)?;
                output.push_str(&format!("// wait!(pin={pin}, timeout={timeout_ms}ms)\n"));
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
            Some(rseq_vm::Opcode::ReadDyn) => {
                pc += 1;
                let addr = read_u32(bytecode, &mut pc)?;
                let delay = read_u32(bytecode, &mut pc)?;
                if pc >= bytecode.len() {
                    return Err(DecompileError::UnexpectedEnd);
                }
                let src = bytecode[pc];
                pc += 1;
                output.push_str(&format!("read!(0x{addr:x}, r{src}"));
                if delay > 0 {
                    output.push_str(&format!(", {delay}"));
                }
                output.push_str(");\n");
            }
            Some(rseq_vm::Opcode::ReadBuf) => {
                pc += 1;
                let addr = read_u32(bytecode, &mut pc)?;
                let delay = read_u32(bytecode, &mut pc)?;
                if pc + 1 >= bytecode.len() {
                    return Err(DecompileError::UnexpectedEnd);
                }
                let len_src = bytecode[pc];
                let dst_buf = bytecode[pc + 1];
                pc += 2;
                output.push_str(&format!("// buf{dst_buf} = read!(0x{addr:x}, r{len_src}"));
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
        logs: Vec<String>,
        reports: Vec<(u32, Vec<ReportArgOwned>)>,
    }

    impl MapBus {
        fn new() -> Self {
            Self {
                mem: HashMap::new(),
                writes: Vec::new(),
                logs: Vec::new(),
                reports: Vec::new(),
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
        fn log(&mut self, msg: &str) -> Result<(), rseq_vm::BusError> {
            self.logs.push(msg.to_string());
            Ok(())
        }
        fn report(
            &mut self,
            kind: u32,
            args: &[rseq_vm::ReportArg<'_>],
        ) -> Result<(), rseq_vm::BusError> {
            self.reports.push((
                kind,
                args.iter()
                    .map(|arg| match arg {
                        rseq_vm::ReportArg::U32(v) => ReportArgOwned::U32(*v),
                        rseq_vm::ReportArg::Bytes(bytes) => ReportArgOwned::Bytes(bytes.to_vec()),
                    })
                    .collect(),
            ));
            Ok(())
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum ReportArgOwned {
        U32(u32),
        Bytes(Vec<u8>),
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
    fn test_parse_irq_arm_allows_repeat() {
        let src = r#"
        irq!(int1) {
            on(fifo_watermark) {
                repeat!(4) {
                    read!(UI.FIFO_DATA, 32);
                }
            }
        }
        "#;
        let program = parse(src).unwrap();
        match &program.stmts[0] {
            Stmt::Irq { arms, .. } => {
                assert_eq!(arms.len(), 1);
                assert!(matches!(arms[0].body[0], Stmt::Repeat { count: 4, .. }));
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
        assert_eq!(vector.pin_id, 0);
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
        // arm 体以 AST 形式保存（由 wait! 内联编译），不再是独立字节码段。
        assert_eq!(vector.arms[0].body.len(), 1);
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
    fn test_compile_wait_inlines_dispatch() {
        // irq! 声明 + wait!(int1)：主程序应含 WaitIrq + ReadVar(0x58,4) +
        // 两条 And/JumpIfZero + 两个 Write 体（内联），不再依赖独立处理段。
        let src = r#"
        chip!("qmi8660.yaml");
        irq!(int1) {
            on(fifo_watermark) { write!(UI.ENCTL, 0x03); }
            on(accel_drdy) { write!(UI.ENCTL, 0x01); }
        }
        wait!(int1);
        "#;
        let program = parse(src).unwrap();
        let compiled = compile_program(&program, Some(&qmi_base())).unwrap();
        let main = &compiled.main;

        // WaitIrq 操作码出现且 pin=0。
        let wait_pos = main
            .iter()
            .position(|&b| b == rseq_vm::Opcode::WaitIrq as u8)
            .expect("main contains WaitIrq");
        assert_eq!(main[wait_pos + 1], 0); // pin_id

        // 紧跟一条 ReadVar 读快照 0x58/4 字节。
        assert_eq!(main[wait_pos + 6], rseq_vm::Opcode::ReadVar as u8);
        let addr = u32::from_le_bytes([
            main[wait_pos + 7],
            main[wait_pos + 8],
            main[wait_pos + 9],
            main[wait_pos + 10],
        ]);
        assert_eq!(addr, 0x58);

        // 两条 And（每 arm 一条）+ 两条 JumpIfZero。
        let ands = main
            .iter()
            .filter(|&&b| b == rseq_vm::Opcode::And as u8)
            .count();
        let jumps = main
            .iter()
            .filter(|&&b| b == rseq_vm::Opcode::JumpIfZero as u8)
            .count();
        assert_eq!(ands, 2);
        assert_eq!(jumps, 2);
        // 两个内联 Write 体。不要直接在裸字节里数 0x02：地址/寄存器/立即数
        // 也可能碰巧等于 Write opcode。
        let text = decompile(main).unwrap();
        assert_eq!(text.matches("write!(").count(), 2, "decompiled: {text}");
    }

    #[test]
    fn test_compile_wait_inlines_irq_repeat_body() {
        let src = r#"
        chip!("qmi8660.yaml");
        irq!(int1) {
            on(fifo_watermark) {
                repeat!(4) {
                    read!(UI.FIFO_DATA, 32);
                }
            }
        }
        wait!(int1);
        "#;
        let program = parse(src).unwrap();
        let compiled = compile_program(&program, Some(&qmi_base())).unwrap();
        assert!(
            compiled
                .main
                .iter()
                .any(|&b| b == rseq_vm::Opcode::Loop as u8)
        );
    }

    #[test]
    fn test_wait_without_irq_errors() {
        let src = r#"
        chip!("qmi8660.yaml");
        wait!(int1);
        "#;
        let program = parse_detailed(src).unwrap();
        let diag = compile_program(&program, Some(&qmi_base())).unwrap_err();
        assert!(matches!(
            diag.error,
            CompileError::WaitWithoutIrq(ref pin) if pin == "int1"
        ));
    }

    #[test]
    fn test_wait_dispatches_matching_arms() {
        // MapBus.wait_irq 用默认 no-op，wait! 立即放行；ReadVar 读 0x58 快照，
        // 按掩码分支命中对应 arm。一回调入口、读快照判多状态。
        let src = r#"
        chip!("qmi8660.yaml");
        irq!(int1) {
            on(fifo_watermark) { write!(UI.ENCTL, 0x03); }
            on(accel_drdy) { write!(UI.ENCTL, 0x01); }
        }
        wait!(int1);
        "#;
        let program = parse(src).unwrap();
        let compiled = compile_program(&program, Some(&qmi_base())).unwrap();

        // 只有 fifo_watermark (bit6) 置位。
        let mut bus = MapBus::new();
        bus.mem.insert(0x58, 0x40);
        rseq_vm::Vm::new(&mut bus, &compiled.main).run().unwrap();
        assert!(bus.writes.iter().any(|(a, d)| *a == 0x3d && d == &[0x03]));
        assert!(!bus.writes.iter().any(|(a, d)| *a == 0x3d && d == &[0x01]));

        // 两个都置位 → 两个 arm 都触发（按声明顺序）。
        let mut bus = MapBus::new();
        bus.mem.insert(0x58, 0x41);
        rseq_vm::Vm::new(&mut bus, &compiled.main).run().unwrap();
        assert!(bus.writes.iter().any(|(a, d)| *a == 0x3d && d == &[0x03]));
        assert!(bus.writes.iter().any(|(a, d)| *a == 0x3d && d == &[0x01]));

        // 无关位置位 → 不触发任何分支。
        let mut bus = MapBus::new();
        bus.mem.insert(0x58, 0x80);
        rseq_vm::Vm::new(&mut bus, &compiled.main).run().unwrap();
        assert!(!bus.writes.iter().any(|(a, _d)| *a == 0x3d));
    }

    #[test]
    fn test_decompile_wait() {
        let src = r#"
        chip!("qmi8660.yaml");
        irq!(int1) {
            on(accel_drdy) { write!(UI.ENCTL, 0x01); }
        }
        wait!(int1, 5000);
        "#;
        let program = parse(src).unwrap();
        let compiled = compile_program(&program, Some(&qmi_base())).unwrap();
        let text = decompile(&compiled.main).unwrap();
        assert!(text.contains("wait!"), "decompiled: {text}");
        assert!(text.contains("5000"), "decompiled: {text}");
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
    fn test_parse_windows_crlf_newlines() {
        let src = "chip!(\"qmi8660.yaml\");\r\n// comment\r\nwrite!(UI.WHOAMI, 0x06);\r\n";
        let program = parse(src).unwrap();
        assert_eq!(program.stmts.len(), 2);
        assert!(matches!(
            &program.stmts[0],
            Stmt::Chip { path } if path == "qmi8660.yaml"
        ));
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
    fn test_parse_bus_select() {
        let program = parse("bus!(i2c, 0x6a); bus!(spi); bus!(i3c);").unwrap();
        assert_eq!(program.stmts.len(), 3);
        assert!(matches!(
            &program.stmts[0],
            Stmt::Bus { kind, arg } if kind == "i2c" && *arg == Some(0x6a)
        ));
        assert!(matches!(
            &program.stmts[1],
            Stmt::Bus { kind, arg } if kind == "spi" && arg.is_none()
        ));
        assert!(matches!(
            &program.stmts[2],
            Stmt::Bus { kind, arg } if kind == "i3c" && arg.is_none()
        ));
    }

    #[test]
    fn test_parse_bus_probe() {
        let program =
            parse("bus_probe!(i2c, { addrs: [0x6a, 0x6b], read: 0x00, expect: 0x06 });").unwrap();
        match &program.stmts[0] {
            Stmt::BusProbe { kind, options } => {
                assert_eq!(kind, "i2c");
                assert_eq!(options.len(), 3);
                assert!(matches!(
                    &options[0],
                    (name, Value::Array(values))
                        if name == "addrs"
                            && values == &vec![Value::Number(0x6a), Value::Number(0x6b)]
                ));
            }
            _ => panic!("expected BusProbe"),
        }
    }

    #[test]
    fn test_compile_bus_select_emits_set_bus() {
        let bytecode = compile(&parse("bus!(i2c, 0x6a);").unwrap()).unwrap();
        assert_eq!(bytecode[0], rseq_vm::Opcode::SetBus as u8);
        assert_eq!(bytecode[1], rseq_vm::BusKind::I2c as u8);
        assert_eq!(u32::from_le_bytes(bytecode[2..6].try_into().unwrap()), 0x6a);
        assert_eq!(bytecode.last(), Some(&(rseq_vm::Opcode::Return as u8)));
    }

    #[test]
    fn test_compile_bus_probe_emits_probe_bus() {
        let bytecode = compile(
            &parse("bus_probe!(i2c, { addrs: [0x6a, 0x6b], read: 0x00, expect: 0x06 });").unwrap(),
        )
        .unwrap();
        assert_eq!(bytecode[0], rseq_vm::Opcode::ProbeBus as u8);
        assert_eq!(bytecode[1], rseq_vm::BusKind::I2c as u8);
        assert_eq!(bytecode[2], 2);
        assert_eq!(u32::from_le_bytes(bytecode[3..7].try_into().unwrap()), 0x6a);
        assert_eq!(
            u32::from_le_bytes(bytecode[7..11].try_into().unwrap()),
            0x6b
        );
        assert_eq!(bytecode.last(), Some(&(rseq_vm::Opcode::Return as u8)));
    }

    #[test]
    fn test_bus_probe_vm_tries_candidates_until_match() {
        struct ProbeBus {
            active_arg: u32,
            selects: Vec<(rseq_vm::BusKind, u32)>,
        }

        impl rseq_vm::Bus for ProbeBus {
            fn set_bus_kind(
                &mut self,
                kind: rseq_vm::BusKind,
                arg: u32,
            ) -> Result<(), rseq_vm::BusError> {
                self.active_arg = arg;
                self.selects.push((kind, arg));
                Ok(())
            }

            fn read(&mut self, _addr: u32, data: &mut [u8]) -> Result<(), rseq_vm::BusError> {
                if self.active_arg != 0x6b {
                    return Err(rseq_vm::BusError::HardwareFailure);
                }
                data[0] = 0x06;
                Ok(())
            }

            fn write(&mut self, _addr: u32, _data: &[u8]) -> Result<(), rseq_vm::BusError> {
                Ok(())
            }

            fn delay_us(&mut self, _us: u32) -> Result<(), rseq_vm::BusError> {
                Ok(())
            }
        }

        let bytecode = compile(
            &parse("bus_probe!(i2c, { addrs: [0x6a, 0x6b], read: 0x00, expect: 0x06 });").unwrap(),
        )
        .unwrap();
        let mut bus = ProbeBus {
            active_arg: 0,
            selects: Vec::new(),
        };
        rseq_vm::Vm::new(&mut bus, &bytecode).run().unwrap();
        assert_eq!(
            bus.selects,
            vec![(rseq_vm::BusKind::I2c, 0x6a), (rseq_vm::BusKind::I2c, 0x6b),]
        );
        assert_eq!(bus.active_arg, 0x6b);
    }

    #[test]
    fn test_compile_i2c_bus_requires_address() {
        let err = compile(&parse("bus!(i2c);").unwrap()).unwrap_err();
        assert!(matches!(err, CompileError::MissingI2cAddress));
    }

    #[test]
    fn test_compile_i2c_bus_rejects_zero_address() {
        let err = compile(&parse("bus!(i2c, 0);").unwrap()).unwrap_err();
        assert!(matches!(err, CompileError::InvalidI2cAddress(0)));
    }

    #[test]
    fn test_decompile_bus_select() {
        let bytecode = compile(&parse("bus!(i2c, 0x6a); bus!(spi); bus!(i3c);").unwrap()).unwrap();
        let decompiled = decompile(&bytecode).unwrap();
        assert!(decompiled.contains("bus!(i2c, 0x6a);"));
        assert!(decompiled.contains("bus!(spi);"));
        assert!(decompiled.contains("bus!(i3c);"));
    }

    #[test]
    fn test_unknown_bus_kind_errors() {
        let program = parse("bus!(can);").unwrap();
        let err = compile(&program).unwrap_err();
        assert!(matches!(err, CompileError::UnknownBusKind(ref kind) if kind == "can"));
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
    fn test_compile_write_field_map_equals_raw_byte() {
        use std::path::PathBuf;

        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../qmi8660.yaml");
        let base = path.parent().unwrap();

        // write! with a field map must build the byte from the field value at
        // compile time and emit a plain Write (no read), so it is byte-identical
        // to writing the same raw byte.
        let src_fields = r#"
        chip!("qmi8660.yaml");
        write!(UI.ACTL1, { afs_ui: 2 }, 50);
        "#;
        let src_raw = r#"
        chip!("qmi8660.yaml");
        write!(UI.ACTL1, 0x02, 50);
        "#;

        let bc_fields = compile_with_base(&parse(src_fields).unwrap(), Some(base)).unwrap();
        let bc_raw = compile_with_base(&parse(src_raw).unwrap(), Some(base)).unwrap();

        assert_eq!(bc_fields.first(), Some(&(rseq_vm::Opcode::Write as u8)));
        assert_eq!(bc_fields, bc_raw);
    }

    #[test]
    fn test_parse_write_field_map() {
        let src = r#"
        write!(UI.ACTL1, { afs_ui: 2, ast: 0 });
        "#;
        let program = parse(src).unwrap();
        match &program.stmts[0] {
            Stmt::Write { addr, val, .. } => {
                assert_eq!(addr, &Value::Ident("UI.ACTL1".to_string()));
                assert_eq!(
                    val,
                    &Value::FieldMap(vec![("afs_ui".into(), 2), ("ast".into(), 0)])
                );
            }
            _ => panic!("expected Write"),
        }
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
        assert!(bytecode.iter().any(|&b| b == rseq_vm::Opcode::Add as u8));
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
        let has_mul = bytecode.iter().any(|&b| b == rseq_vm::Opcode::Mul as u8);
        let has_sub = bytecode.iter().any(|&b| b == rseq_vm::Opcode::Sub as u8);
        let has_shl = bytecode.iter().any(|&b| b == rseq_vm::Opcode::Shl as u8);
        let has_and = bytecode.iter().any(|&b| b == rseq_vm::Opcode::And as u8);
        let has_or = bytecode.iter().any(|&b| b == rseq_vm::Opcode::Or as u8);
        let has_xor = bytecode.iter().any(|&b| b == rseq_vm::Opcode::Xor as u8);
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
        assert!(bytecode.iter().any(|&b| b == rseq_vm::Opcode::Shr as u8));
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

    // ── repeat! / Loop ──────────────────────────────────────────────

    #[test]
    fn test_parse_repeat() {
        let src = "repeat!(3) { write!(0x10, 0xaa); }";
        let program = parse(src).unwrap();
        match &program.stmts[0] {
            Stmt::Repeat { count, body } => {
                assert_eq!(*count, 3);
                assert_eq!(body.len(), 1);
            }
            _ => panic!("expected Repeat"),
        }
    }

    #[test]
    fn test_compile_repeat_emits_loop() {
        let src = "repeat!(3) { write!(0x10, 0xaa); }";
        let program = parse(src).unwrap();
        let bytecode = compile(&program).unwrap();
        // 首字节为 Loop，count=3，body 含一次 Write(0xAA)，整段以 Return 收尾。
        assert_eq!(bytecode[0], rseq_vm::Opcode::Loop as u8);
        let count = u32::from_le_bytes(bytecode[1..5].try_into().unwrap());
        assert_eq!(count, 3);
        let body_len = u32::from_le_bytes(bytecode[5..9].try_into().unwrap()) as usize;
        let body = &bytecode[9..9 + body_len];
        assert_eq!(body[0], rseq_vm::Opcode::Write as u8);
        assert!(body.iter().any(|&b| b == 0xAA));
        assert_eq!(bytecode.last(), Some(&(rseq_vm::Opcode::Return as u8)));
    }

    #[test]
    fn test_repeat_bus_equivalence() {
        // repeat!(3){...} 与手写 3 次 write! 的总线效果应当一致。
        let repeat_src = "repeat!(3) { write!(0x10, 0xaa); }";
        let manual_src = "write!(0x10, 0xaa); write!(0x10, 0xaa); write!(0x10, 0xaa);";
        let repeat_bc = compile(&parse(repeat_src).unwrap()).unwrap();
        let manual_bc = compile(&parse(manual_src).unwrap()).unwrap();

        let mut bus_r = MapBus::new();
        rseq_vm::Vm::new(&mut bus_r, &repeat_bc).run().unwrap();
        let mut bus_m = MapBus::new();
        rseq_vm::Vm::new(&mut bus_m, &manual_bc).run().unwrap();

        assert_eq!(bus_r.writes.len(), 3);
        assert_eq!(bus_r.writes, bus_m.writes);
    }

    #[test]
    fn test_decompile_repeat() {
        let src = "repeat!(3) { write!(0x10, 0xaa); }";
        let bytecode = compile(&parse(src).unwrap()).unwrap();
        let decompiled = decompile(&bytecode).unwrap();
        assert!(decompiled.contains("repeat!(3) {"));
        assert!(decompiled.contains("write!(0x10, 0xaa);"));
        assert!(decompiled.contains("}\n"));
    }

    #[test]
    fn test_repeat_nested() {
        let src = "repeat!(2) { repeat!(3) { write!(0x10, 0xaa); } }";
        let program = parse(src).unwrap();
        match &program.stmts[0] {
            Stmt::Repeat { count, body } => {
                assert_eq!(*count, 2);
                assert!(matches!(body[0], Stmt::Repeat { count: 3, .. }));
            }
            _ => panic!("expected outer Repeat"),
        }

        let bytecode = compile(&program).unwrap();
        // 运行后应产生 2*3=6 次写。
        let mut bus = MapBus::new();
        rseq_vm::Vm::new(&mut bus, &bytecode).run().unwrap();
        assert_eq!(bus.writes.len(), 6);
        // 反编译能还原两层 repeat!。
        let decompiled = decompile(&bytecode).unwrap();
        assert_eq!(decompiled.matches("repeat!(").count(), 2);
    }

    #[test]
    fn test_repeat_count_zero_emits_nothing() {
        let src = "repeat!(0) { write!(0x10, 0xaa); }";
        let bytecode = compile(&parse(src).unwrap()).unwrap();
        // count==0 不发射 Loop，只剩结尾的 Return。
        assert_eq!(bytecode, vec![rseq_vm::Opcode::Return as u8]);
    }

    // ── read! 独立读语句 ───────────────────────────────────────────

    #[test]
    fn test_parse_read_stmt() {
        let src = "read!(0x10, 6, 100);";
        let program = parse(src).unwrap();
        match &program.stmts[0] {
            Stmt::Read {
                addr,
                len,
                delay_us,
            } => {
                assert_eq!(addr, &Value::Number(0x10));
                assert_eq!(len, &Value::Number(6));
                assert_eq!(*delay_us, Some(100));
            }
            _ => panic!("expected Read"),
        }
    }

    #[test]
    fn test_compile_read_stmt_emits_read() {
        let src = "read!(0x10, 6, 100);";
        let bytecode = compile(&parse(src).unwrap()).unwrap();
        // Read | addr=0x10 | len=6 | delay=100 | Return
        assert_eq!(bytecode[0], rseq_vm::Opcode::Read as u8);
        assert_eq!(u32::from_le_bytes(bytecode[1..5].try_into().unwrap()), 0x10);
        assert_eq!(u32::from_le_bytes(bytecode[5..9].try_into().unwrap()), 6);
        assert_eq!(u32::from_le_bytes(bytecode[9..13].try_into().unwrap()), 100);
        assert_eq!(bytecode.last(), Some(&(rseq_vm::Opcode::Return as u8)));
    }

    #[test]
    fn test_compile_read_stmt_allows_variable_len() {
        let src = "let n = 6; read!(0x10, n, 100);";
        let bytecode = compile(&parse(src).unwrap()).unwrap();
        let pos = bytecode
            .iter()
            .position(|&b| b == rseq_vm::Opcode::ReadDyn as u8)
            .expect("contains ReadDyn");
        assert_eq!(
            u32::from_le_bytes(bytecode[pos + 1..pos + 5].try_into().unwrap()),
            0x10
        );
        assert_eq!(
            u32::from_le_bytes(bytecode[pos + 5..pos + 9].try_into().unwrap()),
            100
        );
        assert_eq!(bytecode[pos + 9], 0);
    }

    #[test]
    fn test_decompile_read_stmt() {
        let src = "read!(0x10, 6, 100);";
        let bytecode = compile(&parse(src).unwrap()).unwrap();
        let decompiled = decompile(&bytecode).unwrap();
        assert!(decompiled.contains("read!(0x10, 6, 100);"));
    }

    #[test]
    fn test_repeat_with_read_runs() {
        // 轮询场景：repeat! 包住独立 read!。Read 在 VM 本地丢弃数据，
        // 但 TracingBus 会回传——这里只验证 VM 跑通且反编译还原结构。
        let src = "repeat!(3) { read!(0x10, 6, 0); }";
        let bytecode = compile(&parse(src).unwrap()).unwrap();
        let mut bus = MapBus::new();
        assert!(rseq_vm::Vm::new(&mut bus, &bytecode).run().is_ok());
        let decompiled = decompile(&bytecode).unwrap();
        assert!(decompiled.contains("repeat!(3) {"));
        assert!(decompiled.contains("read!(0x10, 6);"));
    }

    #[test]
    fn test_compile_irq_segment_allows_dynamic_fifo_read() {
        let src = r#"
        chip!("qmi8660.yaml");
        irq!(int1) {
            on(fifo_watermark) {
                let fifo_l = read!(UI.FIFO_STATUSL, 1);
                read!(UI.FIFO_DATA, fifo_l);
            }
        }
        "#;
        let program = parse(src).unwrap();
        let compiled = compile_program_units(&[ProgramUnit {
            program: &program,
            base_dir: Some(&qmi_base()),
        }])
        .unwrap();
        let int1 = compiled.irq_bytecodes.get("int1").expect("int1 handler");
        assert!(int1.iter().any(|&b| b == rseq_vm::Opcode::ReadDyn as u8));
    }

    // ── if-else / 逻辑运算符 ────────────────────────────────────────

    #[test]
    fn test_parse_if_else() {
        let src = "if (a > 5) { write!(0x10, 0x01); } else { write!(0x11, 0x02); }";
        let program = parse(src).unwrap();
        match &program.stmts[0] {
            Stmt::If { cond, then, else_ } => {
                assert_eq!(then.len(), 1);
                assert_eq!(else_.len(), 1);
                assert!(matches!(&**cond, Expr::Binary { op: BinOp::Gt, .. }));
            }
            _ => panic!("expected If"),
        }
    }

    #[test]
    fn test_if_else_runs_correct_branch() {
        // cond 假 (3 > 5) → else 写 0x11=2
        let src = "if (3 > 5) { write!(0x10, 0x01); } else { write!(0x11, 0x02); }";
        let bc = compile(&parse(src).unwrap()).unwrap();
        let mut bus = MapBus::new();
        rseq_vm::Vm::new(&mut bus, &bc).run().unwrap();
        assert!(bus.writes.iter().any(|(a, d)| *a == 0x11 && d == &[0x02]));
        assert!(!bus.writes.iter().any(|(a, _)| *a == 0x10));

        // cond 真 (5 > 3) → then 写 0x10=1
        let src = "if (5 > 3) { write!(0x10, 0x01); } else { write!(0x11, 0x02); }";
        let bc = compile(&parse(src).unwrap()).unwrap();
        let mut bus = MapBus::new();
        rseq_vm::Vm::new(&mut bus, &bc).run().unwrap();
        assert!(bus.writes.iter().any(|(a, d)| *a == 0x10 && d == &[0x01]));
        assert!(!bus.writes.iter().any(|(a, _)| *a == 0x11));
    }

    #[test]
    fn test_if_without_else() {
        let false_src = "if (1 > 2) { write!(0x10, 0x01); }";
        let bc = compile(&parse(false_src).unwrap()).unwrap();
        let mut bus = MapBus::new();
        rseq_vm::Vm::new(&mut bus, &bc).run().unwrap();
        assert!(bus.writes.is_empty());

        let true_src = "if (2 > 1) { write!(0x10, 0x01); }";
        let bc = compile(&parse(true_src).unwrap()).unwrap();
        let mut bus = MapBus::new();
        rseq_vm::Vm::new(&mut bus, &bc).run().unwrap();
        assert!(bus.writes.iter().any(|(a, d)| *a == 0x10 && d == &[0x01]));
    }

    #[test]
    fn test_if_with_logical_and() {
        // (5>3) && (2>1) → 真
        let src = "if ((5 > 3) && (2 > 1)) { write!(0x10, 0x01); }";
        let bc = compile(&parse(src).unwrap()).unwrap();
        let mut bus = MapBus::new();
        rseq_vm::Vm::new(&mut bus, &bc).run().unwrap();
        assert!(bus.writes.iter().any(|(a, d)| *a == 0x10 && d == &[0x01]));

        // (5>3) && (2>5) → 假
        let src = "if ((5 > 3) && (2 > 5)) { write!(0x10, 0x01); }";
        let bc = compile(&parse(src).unwrap()).unwrap();
        let mut bus = MapBus::new();
        rseq_vm::Vm::new(&mut bus, &bc).run().unwrap();
        assert!(bus.writes.is_empty());
    }

    #[test]
    fn test_if_with_logical_or() {
        // (1>2) || (2>1) → 真
        let src = "if ((1 > 2) || (2 > 1)) { write!(0x10, 0x01); }";
        let bc = compile(&parse(src).unwrap()).unwrap();
        let mut bus = MapBus::new();
        rseq_vm::Vm::new(&mut bus, &bc).run().unwrap();
        assert!(bus.writes.iter().any(|(a, d)| *a == 0x10 && d == &[0x01]));

        // (1>2) || (2>5) → 假
        let src = "if ((1 > 2) || (2 > 5)) { write!(0x10, 0x01); }";
        let bc = compile(&parse(src).unwrap()).unwrap();
        let mut bus = MapBus::new();
        rseq_vm::Vm::new(&mut bus, &bc).run().unwrap();
        assert!(bus.writes.is_empty());
    }

    #[test]
    fn test_if_with_logical_not() {
        // !(1==1) → !1 → 0 → 假
        let src = "if (!(1 == 1)) { write!(0x10, 0x01); }";
        let bc = compile(&parse(src).unwrap()).unwrap();
        let mut bus = MapBus::new();
        rseq_vm::Vm::new(&mut bus, &bc).run().unwrap();
        assert!(bus.writes.is_empty());

        // !(1==2) → !0 → 1 → 真
        let src = "if (!(1 == 2)) { write!(0x10, 0x01); }";
        let bc = compile(&parse(src).unwrap()).unwrap();
        let mut bus = MapBus::new();
        rseq_vm::Vm::new(&mut bus, &bc).run().unwrap();
        assert!(bus.writes.iter().any(|(a, d)| *a == 0x10 && d == &[0x01]));
    }

    #[test]
    fn test_else_if_chain() {
        // 1>2 假, 2>1 真 → 写 0x11=2
        let src = "if (1 > 2) { write!(0x10, 0x01); } else if (2 > 1) { write!(0x11, 0x02); } else { write!(0x12, 0x03); }";
        let bc = compile(&parse(src).unwrap()).unwrap();
        let mut bus = MapBus::new();
        rseq_vm::Vm::new(&mut bus, &bc).run().unwrap();
        assert!(bus.writes.iter().any(|(a, d)| *a == 0x11 && d == &[0x02]));
        assert!(!bus.writes.iter().any(|(a, _)| *a == 0x10 || *a == 0x12));
    }

    #[test]
    fn test_if_nested_in_else() {
        // 1>2 假 → else → 内层 if 3>2 真 → 写 0x11=2
        let src = "if (1 > 2) { write!(0x10, 0x01); } else { if (3 > 2) { write!(0x11, 0x02); } }";
        let bc = compile(&parse(src).unwrap()).unwrap();
        let mut bus = MapBus::new();
        rseq_vm::Vm::new(&mut bus, &bc).run().unwrap();
        assert!(bus.writes.iter().any(|(a, d)| *a == 0x11 && d == &[0x02]));
        assert!(!bus.writes.iter().any(|(a, _)| *a == 0x10));
    }

    #[test]
    fn test_if_nested_in_repeat() {
        // repeat!(3) { if (1 > 0) { write!(0x10, 0x01); } } → 3 次写
        let src = "repeat!(3) { if (1 > 0) { write!(0x10, 0x01); } }";
        let bc = compile(&parse(src).unwrap()).unwrap();
        let mut bus = MapBus::new();
        rseq_vm::Vm::new(&mut bus, &bc).run().unwrap();
        assert_eq!(bus.writes.len(), 3);
        assert!(bus.writes.iter().all(|(a, d)| *a == 0x10 && d == &[0x01]));
    }

    #[test]
    fn test_decompile_if_else() {
        let src = "if (3 > 5) { write!(0x10, 0x01); } else { write!(0x11, 0x02); }";
        let bc = compile(&parse(src).unwrap()).unwrap();
        let decompiled = decompile(&bc).unwrap();
        // 线性伪指令：条件跳转 + 无条件跳转 + 比较。
        assert!(decompiled.contains("// if r"));
        assert!(decompiled.contains("goto"));
        assert!(decompiled.contains(">")); // CmpGt（3 > 5）
        assert!(decompiled.contains("write!(0x10, 0x01);"));
        assert!(decompiled.contains("write!(0x11, 0x02);"));
    }

    // ── print! / Log ────────────────────────────────────────────────

    #[test]
    fn test_parse_print() {
        let src = r#"print!("hello");"#;
        let program = parse(src).unwrap();
        match &program.stmts[0] {
            Stmt::Print { msg, vars } => {
                assert_eq!(msg, "hello");
                assert!(vars.is_empty());
            }
            _ => panic!("expected Print"),
        }
    }

    #[test]
    fn test_parse_print_with_vars() {
        let src = r#"print!("a={} b={x}", a, b);"#;
        let program = parse(src).unwrap();
        match &program.stmts[0] {
            Stmt::Print { msg, vars } => {
                assert_eq!(msg, "a={} b={x}");
                assert_eq!(vars, &vec!["a".to_string(), "b".to_string()]);
            }
            _ => panic!("expected Print"),
        }
    }

    #[test]
    fn test_compile_print_emits_log() {
        let src = r#"print!("hello");"#;
        let bc = compile(&parse(src).unwrap()).unwrap();
        // Log | len=5 | "hello" | Return
        assert_eq!(bc[0], rseq_vm::Opcode::Log as u8);
        let len = u32::from_le_bytes(bc[1..5].try_into().unwrap());
        assert_eq!(len, 5);
        assert_eq!(&bc[5..10], b"hello");
        assert_eq!(bc.last(), Some(&(rseq_vm::Opcode::Return as u8)));
    }

    #[test]
    fn test_compile_print_with_vars_emits_logvar() {
        // let a = 7; print!("a={}", a);
        let src = r#"let a = 7; print!("a={}", a);"#;
        let bc = compile(&parse(src).unwrap()).unwrap();
        // 找到 LogVar 操作码。
        let idx = bc
            .iter()
            .position(|&b| b == rseq_vm::Opcode::LogVar as u8)
            .expect("LogVar emitted");
        // LogVar | n_vars=1 | reg | fmt_len=4 | "a={}"
        assert_eq!(bc[idx + 1], 1); // n_vars
        // bc[idx+2] = reg index of `a` (应非零，因 let a 先分配)
        let fmt_len = u32::from_le_bytes(bc[idx + 3..idx + 7].try_into().unwrap());
        assert_eq!(fmt_len, 4);
        assert_eq!(&bc[idx + 7..idx + 11], b"a={}");
    }

    #[test]
    fn test_decompile_print_round_trip() {
        // 含转义引号，验证 {:?} 还原成合法字符串字面量。
        let src = r#"print!("hello \"x\"");"#;
        let bc = compile(&parse(src).unwrap()).unwrap();
        let decompiled = decompile(&bc).unwrap();
        assert!(decompiled.contains(r#"print!("hello \"x\"");"#));
    }

    #[test]
    fn test_print_runs_and_records_log() {
        let src = r#"print!("starting");"#;
        let bc = compile(&parse(src).unwrap()).unwrap();
        let mut bus = MapBus::new();
        rseq_vm::Vm::new(&mut bus, &bc).run().unwrap();
        assert_eq!(bus.logs, vec!["starting".to_string()]);
        assert!(bus.writes.is_empty());
    }

    #[test]
    fn test_print_with_vars_runs_and_formats() {
        // let a = 42; let b = 0xaa; print!("a={} b={x}", a, b);
        let src = r#"let a = 42; let b = 0xaa; print!("a={} b={x}", a, b);"#;
        let bc = compile(&parse(src).unwrap()).unwrap();
        let mut bus = MapBus::new();
        rseq_vm::Vm::new(&mut bus, &bc).run().unwrap();
        assert_eq!(bus.logs, vec!["a=42 b=0xaa".to_string()]);
    }

    // ── report! / raw buffer ───────────────────────────────────────

    #[test]
    fn test_parse_report_fifo_raw() {
        let src = "let fifo_len = 3; let data = read!(0x20, fifo_len); report!(FIFO_RAW, fifo_len, data);";
        let program = parse(src).unwrap();
        match &program.stmts[2] {
            Stmt::Report { kind, values } => {
                assert_eq!(kind, &Value::Ident("FIFO_RAW".to_string()));
                assert_eq!(values.len(), 2);
            }
            _ => panic!("expected Report"),
        }
    }

    #[test]
    fn test_parse_report_format_fifo_raw() {
        let src = "report_format!(FIFO_RAW, i16_le, { fields: [gx, gy, gz, ax, ay, az], gyro_fields: [gx, gy, gz], accel_fields: [ax, ay, az], accel_fs_g: 16, gyro_fs_dps: 4096, output: physical_f32 });";
        let program = parse(src).unwrap();
        match &program.stmts[0] {
            Stmt::ReportFormat {
                kind,
                decoder,
                options,
            } => {
                assert_eq!(kind, &Value::Ident("FIFO_RAW".to_string()));
                assert_eq!(decoder, "i16_le");
                assert_eq!(
                    options,
                    &vec![
                        (
                            "fields".to_string(),
                            ReportOptionValue::IdentArray(vec![
                                "gx".to_string(),
                                "gy".to_string(),
                                "gz".to_string(),
                                "ax".to_string(),
                                "ay".to_string(),
                                "az".to_string(),
                            ]),
                        ),
                        (
                            "gyro_fields".to_string(),
                            ReportOptionValue::IdentArray(vec![
                                "gx".to_string(),
                                "gy".to_string(),
                                "gz".to_string(),
                            ]),
                        ),
                        (
                            "accel_fields".to_string(),
                            ReportOptionValue::IdentArray(vec![
                                "ax".to_string(),
                                "ay".to_string(),
                                "az".to_string(),
                            ]),
                        ),
                        ("accel_fs_g".to_string(), ReportOptionValue::Number(16)),
                        ("gyro_fs_dps".to_string(), ReportOptionValue::Number(4096)),
                        (
                            "output".to_string(),
                            ReportOptionValue::Ident("physical_f32".to_string()),
                        ),
                    ]
                );
            }
            _ => panic!("expected ReportFormat"),
        }
    }

    #[test]
    fn test_report_format_is_compile_time_metadata_only() {
        let empty = compile(&parse("").unwrap()).unwrap();
        let with_format = compile(
            &parse(
                "report_format!(FIFO_RAW, i16_le, { fields: [gx, gy, gz, ax, ay, az], gyro_fields: [gx, gy, gz], accel_fields: [ax, ay, az], accel_fs_g: 16, gyro_fs_dps: 4096, output: physical_f32 });",
            )
            .unwrap(),
        )
        .unwrap();

        assert_eq!(with_format, empty);
    }

    #[test]
    fn test_report_fifo_raw_compiles_read_buf() {
        let src = "let fifo_len = 3; let data = read!(0x20, fifo_len); report!(FIFO_RAW, fifo_len, data);";
        let bc = compile(&parse(src).unwrap()).unwrap();
        assert!(
            bc.iter().any(|&b| b == rseq_vm::Opcode::ReadBuf as u8),
            "bytecode: {bc:02x?}"
        );
        assert!(
            bc.iter().any(|&b| b == rseq_vm::Opcode::Report as u8),
            "bytecode: {bc:02x?}"
        );
    }

    #[test]
    fn test_report_fifo_raw_runs_with_len_and_bytes() {
        let src = "let fifo_len = 3; let data = read!(0x20, fifo_len); report!(FIFO_RAW, fifo_len, data);";
        let bc = compile(&parse(src).unwrap()).unwrap();
        let mut bus = MapBus::new();
        bus.mem.insert(0x20, 0xaa);
        bus.mem.insert(0x21, 0xbb);
        bus.mem.insert(0x22, 0xcc);

        rseq_vm::Vm::new(&mut bus, &bc).run().unwrap();
        assert_eq!(
            bus.reports,
            vec![(
                0x01,
                vec![
                    ReportArgOwned::U32(3),
                    ReportArgOwned::Bytes(vec![0xaa, 0xbb, 0xcc]),
                ],
            )]
        );
    }

    #[test]
    fn test_report_reserved_kinds_compile() {
        let src = "report!(AMD, 1); report!(SMD, 2); report!(DRDY, 3);";
        let bc = compile(&parse(src).unwrap()).unwrap();
        let mut bus = MapBus::new();

        rseq_vm::Vm::new(&mut bus, &bc).run().unwrap();
        assert_eq!(
            bus.reports,
            vec![
                (REPORT_KIND_AMD, vec![ReportArgOwned::U32(1)]),
                (REPORT_KIND_SMD, vec![ReportArgOwned::U32(2)]),
                (REPORT_KIND_DRDY, vec![ReportArgOwned::U32(3)]),
            ]
        );
    }

    #[test]
    fn test_print_signed_and_hex() {
        // 负数：0xFFFFFFFF = -1。print!("d={} h={x}", n, n) —— 同一值两种视图。
        let src = r#"let n = 0xffffffff; print!("d={} h={x}", n, n);"#;
        let bc = compile(&parse(src).unwrap()).unwrap();
        let mut bus = MapBus::new();
        rseq_vm::Vm::new(&mut bus, &bc).run().unwrap();
        assert_eq!(bus.logs, vec!["d=-1 h=0xffffffff".to_string()]);
    }

    #[test]
    fn test_print_newline_escape() {
        // \n 展开成真实换行：尾换行 + 多行。
        let src = r#"print!("line1\nline2\n");"#;
        let bc = compile(&parse(src).unwrap()).unwrap();
        let mut bus = MapBus::new();
        rseq_vm::Vm::new(&mut bus, &bc).run().unwrap();
        assert_eq!(bus.logs, vec!["line1\nline2\n".to_string()]);
    }

    #[test]
    fn test_decompile_preserves_newline_escape() {
        // 反编译应把真实换行还原成 \n 转义（{:?} 自动处理）。
        let src = r#"print!("a\nb");"#;
        let bc = compile(&parse(src).unwrap()).unwrap();
        let decompiled = decompile(&bc).unwrap();
        assert!(decompiled.contains(r#"print!("a\nb");"#));
    }
}
