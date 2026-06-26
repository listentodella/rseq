//! Register Sequence DSL Parser
//! A DSL for defining register sequences in embedded systems

mod chip;

use chumsky::{
    input::{Stream, ValueInput},
    prelude::*,
};
use logos::Logos;
use std::fmt;
use std::path::Path;
use std::vec::Vec;

pub use chip::{
    emit_update_bytecode, load_chip, normalize_chip_path, resolve_chip_path, Chip, ChipError,
    ChipRegistry, UpdatePlan,
};

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

    #[regex(r#""([^"\\]|\\.)*""#, |lex| {
        let s = lex.slice();
        s[1..s.len() - 1].replace("\\\"", "\"").replace("\\\\", "\\")
    })]
    String(String),

    #[regex(r"[ \t\f\n]+", logos::skip)]
    Whitespace,
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
            Self::LParen => write!(f, "("),
            Self::RParen => write!(f, ")"),
            Self::LBracket => write!(f, "["),
            Self::RBracket => write!(f, "]"),
            Self::LBrace => write!(f, "{{"),
            Self::RBrace => write!(f, "}}"),
            Self::Colon => write!(f, ":"),
            Self::Comma => write!(f, ","),
            Self::Semicolon => write!(f, ";"),
            Self::String(s) => write!(f, "\"{s}\""),
            Self::Whitespace => write!(f, "<whitespace>"),
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
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Read {
        addr: Value,
        len: Value,
        delay_us: Option<u32>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    pub stmts: Vec<Stmt>,
}

fn value<'tok, 'src: 'tok, I>() -> impl Parser<'tok, I, Value, extra::Err<Rich<'tok, Token<'src>>>>
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
-> impl Parser<'tok, I, Value, extra::Err<Rich<'tok, Token<'src>>>>
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

fn read_expr<'tok, 'src: 'tok, I>()
-> impl Parser<'tok, I, Expr, extra::Err<Rich<'tok, Token<'src>>>>
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

fn stmt<'tok, 'src: 'tok, I>() -> impl Parser<'tok, I, Stmt, extra::Err<Rich<'tok, Token<'src>>>>
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
        .then(just(Token::Assign).ignore_then(read_expr()))
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

    let update_stmt = just(Token::UpdateMacro)
        .ignore_then(
            select! { Token::Ident(s) => s.to_string() }
                .then(
                    just(Token::Comma).ignore_then(
                        field_map().or(select! { Token::Number(n) => Value::Number(n) }),
                    ),
                )
                .then(just(Token::Comma).ignore_then(select! { Token::Number(n) => n }).or_not())
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

fn parser<'tok, 'src: 'tok, I>()
-> impl Parser<'tok, I, Program, extra::Err<Rich<'tok, Token<'src>>>>
where
    I: ValueInput<'tok, Token = Token<'src>, Span = SimpleSpan>,
{
    stmt().repeated().collect().map(|stmts| Program { stmts })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    GenericError,
}

pub fn parse(source: &str) -> Result<Program, ParseError> {
    let token_iter = Token::lexer(source).spanned().map(|(tok, span)| match tok {
        Ok(tok) => (tok, span.into()),
        Err(()) => (Token::Error, span.into()),
    });

    let token_stream =
        Stream::from_iter(token_iter).map((0..source.len()).into(), |(t, s): (_, _)| (t, s));

    parser()
        .parse(token_stream)
        .into_result()
        .map_err(|_| ParseError::GenericError)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompileError {
    UnsupportedValue,
    NumberExpected,
    Chip(ChipError),
    Register(String),
    Update(String),
}

pub fn compile(program: &Program) -> Result<Vec<u8>, CompileError> {
    compile_with_base(program, None)
}

pub fn compile_with_base(
    program: &Program,
    base_dir: Option<&Path>,
) -> Result<Vec<u8>, CompileError> {
    let mut registry = ChipRegistry::default();
    for stmt in &program.stmts {
        if let Stmt::Chip { path } = stmt {
            let chip_path = resolve_chip_path(path, base_dir);
            registry
                .load_file(&chip_path)
                .map_err(CompileError::Chip)?;
        }
    }

    let mut bytecode = Vec::new();

    for stmt in &program.stmts {
        match stmt {
            Stmt::Chip { .. } => {}
            Stmt::Let { expr, .. } => match expr {
                Expr::Read {
                    addr,
                    len,
                    delay_us,
                } => {
                    let addr = resolve_u32(addr, &registry)?;
                    let len = resolve_u32(len, &registry)?;
                    let delay = delay_us.unwrap_or(0);

                    bytecode.push(rseq_vm::Opcode::Read as u8);
                    bytecode.extend_from_slice(&addr.to_le_bytes());
                    bytecode.extend_from_slice(&len.to_le_bytes());
                    bytecode.extend_from_slice(&delay.to_le_bytes());
                }
            },
            Stmt::Write {
                addr,
                val,
                delay_us,
            } => {
                let addr = resolve_u32(addr, &registry)?;
                let delay = delay_us.unwrap_or(0);

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
                    _ => return Err(CompileError::Update("expected field value or field map".into())),
                };
                let plan = registry
                    .plan_update(target, &updates)
                    .map_err(|e| CompileError::Chip(e))?;
                emit_update_bytecode(&mut bytecode, &plan, delay_us.unwrap_or(0));
            }
        }
    }

    bytecode.push(rseq_vm::Opcode::Return as u8);
    Ok(bytecode)
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
                    &Value::FieldMap(vec![
                        ("cs_pu_dis".into(), 1),
                        ("sda_scl_pu_dis".into(), 0),
                    ])
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
}
