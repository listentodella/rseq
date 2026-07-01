//! 载荷级线缆约定:Trace 记录、执行状态、LOAD 段格式。
//!
//! 这些类型在 [`crate::tracing_bus`](TracingBus,编码端)与主机驱动(解码端)之间共享,
//! 保证两端对载荷布局的理解一致。

use crate::frame::{FrameType, OVERHEAD};

// ── Trace 载荷 ───────────────────────────────────────────────
/// Trace op:读。
pub const TRACE_OP_READ: u8 = 0x01;
/// Trace op:写。
pub const TRACE_OP_WRITE: u8 = 0x02;
/// Trace op:延时。
pub const TRACE_OP_DELAY: u8 = 0x03;
/// Trace op:日志（`print!`）。
pub const TRACE_OP_LOG: u8 = 0x04;

/// Trace 载荷最大长度(op+addr+dlen+data≤4096)。
pub const MAX_TRACE_PAYLOAD: usize = 1 + 4 + 2 + 4096;

/// 解码后的 Trace 记录,载荷切片借用自帧缓冲。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraceRef<'a> {
    Read { addr: u32, data: &'a [u8] },
    Write { addr: u32, data: &'a [u8] },
    Delay { us: u32 },
    /// `print!` 日志，载荷为 utf8 字节（解码端按 lossy 转 String）。
    Log { msg: &'a [u8] },
}

/// 在 `out` 中构造一条完整的 Read/Write Trace 帧(含帧头与 CRC),返回总字节数。
///
/// `out` 需 ≥ `data.len() + 1 + 4 + 2 + OVERHEAD`。
pub fn encode_trace_rw(out: &mut [u8], op: u8, addr: u32, data: &[u8]) -> usize {
    let payload_len = 1 + 4 + 2 + data.len();
    let total = OVERHEAD + payload_len;
    assert!(
        out.len() >= total,
        "trace frame buffer too small: need {total}, have {}",
        out.len()
    );
    // 帧头
    out[0] = 0x55;
    out[1] = 0xAA;
    out[2] = FrameType::Trace as u8;
    out[3] = (payload_len & 0xFF) as u8;
    out[4] = (payload_len >> 8) as u8;
    // 载荷:op + addr(LE) + dlen(LE) + data
    out[5] = op;
    out[6..10].copy_from_slice(&addr.to_le_bytes());
    let dlen = data.len() as u16;
    out[10] = (dlen & 0xFF) as u8;
    out[11] = (dlen >> 8) as u8;
    out[12..12 + data.len()].copy_from_slice(data);
    // CRC 覆盖 type+len+payload
    let crc = crate::crc32::crc32(&out[2..5 + payload_len]);
    out[5 + payload_len..5 + payload_len + 4].copy_from_slice(&crc.to_le_bytes());
    total
}

/// 在 `out` 中构造一条 Delay Trace 帧,返回总字节数。`out` 需 ≥ `OVERHEAD + 5`。
pub fn encode_trace_delay(out: &mut [u8], us: u32) -> usize {
    const PAYLOAD_LEN: usize = 1 + 4;
    let total = OVERHEAD + PAYLOAD_LEN;
    assert!(out.len() >= total, "trace delay buffer too small");
    out[0] = 0x55;
    out[1] = 0xAA;
    out[2] = FrameType::Trace as u8;
    out[3] = (PAYLOAD_LEN & 0xFF) as u8;
    out[4] = (PAYLOAD_LEN >> 8) as u8;
    out[5] = TRACE_OP_DELAY;
    out[6..10].copy_from_slice(&us.to_le_bytes());
    let crc = crate::crc32::crc32(&out[2..5 + PAYLOAD_LEN]);
    out[5 + PAYLOAD_LEN..5 + PAYLOAD_LEN + 4].copy_from_slice(&crc.to_le_bytes());
    total
}

/// 在 `out` 中构造一条 Log Trace 帧(`print!`),返回总字节数。
/// 载荷 = `op + mlen(u16) + msg`。`out` 需 ≥ `OVERHEAD + 3 + msg.len()`。
pub fn encode_trace_log(out: &mut [u8], msg: &str) -> usize {
    let payload_len = 1 + 2 + msg.len();
    let total = OVERHEAD + payload_len;
    assert!(
        out.len() >= total,
        "trace log buffer too small: need {total}, have {}",
        out.len()
    );
    out[0] = 0x55;
    out[1] = 0xAA;
    out[2] = FrameType::Trace as u8;
    out[3] = (payload_len & 0xFF) as u8;
    out[4] = (payload_len >> 8) as u8;
    out[5] = TRACE_OP_LOG;
    let mlen = msg.len() as u16;
    out[6] = (mlen & 0xFF) as u8;
    out[7] = (mlen >> 8) as u8;
    out[8..8 + msg.len()].copy_from_slice(msg.as_bytes());
    let crc = crate::crc32::crc32(&out[2..5 + payload_len]);
    out[5 + payload_len..5 + payload_len + 4].copy_from_slice(&crc.to_le_bytes());
    total
}

/// 从 Trace 载荷解码为 [`TraceRef`]。布局不合法返回 `None`。
pub fn decode_trace(payload: &[u8]) -> Option<TraceRef<'_>> {
    if payload.is_empty() {
        return None;
    }
    match payload[0] {
        TRACE_OP_READ | TRACE_OP_WRITE => {
            if payload.len() < 1 + 4 + 2 {
                return None;
            }
            let addr = u32::from_le_bytes([payload[1], payload[2], payload[3], payload[4]]);
            let dlen = u16::from_le_bytes([payload[5], payload[6]]) as usize;
            if payload.len() < 1 + 4 + 2 + dlen {
                return None;
            }
            let data = &payload[1 + 4 + 2..1 + 4 + 2 + dlen];
            let op = payload[0];
            if op == TRACE_OP_READ {
                Some(TraceRef::Read { addr, data })
            } else {
                Some(TraceRef::Write { addr, data })
            }
        }
        TRACE_OP_DELAY => {
            if payload.len() < 1 + 4 {
                return None;
            }
            let us = u32::from_le_bytes([payload[1], payload[2], payload[3], payload[4]]);
            Some(TraceRef::Delay { us })
        }
        TRACE_OP_LOG => {
            if payload.len() < 1 + 2 {
                return None;
            }
            let mlen = u16::from_le_bytes([payload[1], payload[2]]) as usize;
            if payload.len() < 1 + 2 + mlen {
                return None;
            }
            let msg = &payload[1 + 2..1 + 2 + mlen];
            Some(TraceRef::Log { msg })
        }
        _ => None,
    }
}

// ── 执行状态 ─────────────────────────────────────────────────
/// `Result` 帧的状态码,与 `rseq_vm::VmError` 一一映射。
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecStatus {
    Ok = 0,
    InvalidOpcode = 1,
    ProgramTooShort = 2,
    InvalidLength = 3,
    DivideByZero = 4,
    BusError = 5,
}

impl ExecStatus {
    pub const fn from_u8(n: u8) -> Option<Self> {
        match n {
            0 => Some(Self::Ok),
            1 => Some(Self::InvalidOpcode),
            2 => Some(Self::ProgramTooShort),
            3 => Some(Self::InvalidLength),
            4 => Some(Self::DivideByZero),
            5 => Some(Self::BusError),
            _ => None,
        }
    }

    /// 把 `VmError` 映射为可上线的状态码。
    pub fn from_vm_error(e: rseq_vm::VmError) -> Self {
        match e {
            rseq_vm::VmError::InvalidOpcode => Self::InvalidOpcode,
            rseq_vm::VmError::ProgramTooShort => Self::ProgramTooShort,
            rseq_vm::VmError::InvalidLength => Self::InvalidLength,
            rseq_vm::VmError::DivideByZero => Self::DivideByZero,
            rseq_vm::VmError::BusError(_) => Self::BusError,
        }
    }
}

// ── LOAD 段格式 ──────────────────────────────────────────────
/// LOAD 载荷版本。当前仅版本 1。
pub const LOAD_VERSION: u8 = 1;

/// 段种类:主程序字节码(以 Return 结尾)。
pub const SEG_KIND_MAIN: u8 = 0x00;
/// 段种类:中断派发表(预留,MCU 当前忽略)。
pub const SEG_KIND_IRQ_TABLE: u8 = 0x01;
/// 段种类:中断处理段(预留)。
pub const SEG_KIND_IRQ_HANDLER: u8 = 0x02;

/// 把一段 main 字节码打包成 LOAD 载荷,写入 `out`,返回字节数。
///
/// 布局:`[version=1][seg_count=1][kind=0x00][seg_len u16 LE][bytecode]`。
pub fn encode_load_main_into(out: &mut [u8], bytecode: &[u8]) -> usize {
    let need = 2 + 3 + bytecode.len();
    assert!(
        out.len() >= need,
        "load payload buffer too small: need {need}, have {}",
        out.len()
    );
    out[0] = LOAD_VERSION;
    out[1] = 1; // seg_count
    out[2] = SEG_KIND_MAIN;
    let len = bytecode.len() as u16;
    out[3] = (len & 0xFF) as u8;
    out[4] = (len >> 8) as u8;
    out[5..5 + bytecode.len()].copy_from_slice(bytecode);
    need
}

/// LOAD 载荷的段迭代器,零分配。
pub struct LoadSegs<'a> {
    rest: &'a [u8],
    remaining: usize,
}

impl<'a> Iterator for LoadSegs<'a> {
    type Item = (u8, &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        if self.rest.len() < 3 {
            return None;
        }
        let kind = self.rest[0];
        let len = u16::from_le_bytes([self.rest[1], self.rest[2]]) as usize;
        if self.rest.len() < 3 + len {
            return None;
        }
        let bytes = &self.rest[3..3 + len];
        self.rest = &self.rest[3 + len..];
        self.remaining -= 1;
        Some((kind, bytes))
    }
}

/// 解析 LOAD 载荷,返回 (版本, 段迭代器)。布局不合法返回 `None`。
pub fn load_segments(payload: &[u8]) -> Option<(u8, LoadSegs<'_>)> {
    if payload.len() < 2 {
        return None;
    }
    let version = payload[0];
    let count = payload[1] as usize;
    Some((version, LoadSegs { rest: &payload[2..], remaining: count }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::prelude::v1::*;


    #[test]
    fn trace_rw_round_trip() {
        let data = [0x11, 0x22, 0x33];
        let mut buf = vec![0u8; MAX_TRACE_PAYLOAD + 32];
        let n = encode_trace_rw(&mut buf, TRACE_OP_WRITE, 0x1234, &data);
        // 用 frame 解码器取出 payload,再 decode_trace。
        let mut dec = crate::frame::FrameDecoder::<{ crate::frame::HOST_FRAME_BUF }>::new();
        let mut payload = None;
        dec.feed(&buf[..n], |ty, p| {
            payload = Some((ty, p.to_vec()));
        });
        let (ty, p) = payload.expect("frame decoded");
        assert_eq!(ty, FrameType::Trace);
        let rec = decode_trace(&p).expect("trace decoded");
        assert_eq!(rec, TraceRef::Write { addr: 0x1234, data: &data });
    }

    #[test]
    fn trace_delay_round_trip() {
        let mut buf = vec![0u8; 32];
        let n = encode_trace_delay(&mut buf, 50000);
        let mut dec = crate::frame::FrameDecoder::<{ crate::frame::HOST_FRAME_BUF }>::new();
        let mut payload = None;
        dec.feed(&buf[..n], |ty, p| {
            payload = Some((ty, p.to_vec()));
        });
        let (_, p) = payload.unwrap();
        assert_eq!(decode_trace(&p), Some(TraceRef::Delay { us: 50000 }));
    }

    #[test]
    fn trace_log_round_trip() {
        let mut buf = vec![0u8; 64];
        let n = encode_trace_log(&mut buf, "hi log");
        let mut dec = crate::frame::FrameDecoder::<{ crate::frame::HOST_FRAME_BUF }>::new();
        let mut payload = None;
        dec.feed(&buf[..n], |ty, p| {
            payload = Some((ty, p.to_vec()));
        });
        let (ty, p) = payload.expect("frame decoded");
        assert_eq!(ty, FrameType::Trace);
        assert_eq!(decode_trace(&p), Some(TraceRef::Log { msg: &b"hi log"[..] }));
    }

    #[test]
    fn load_segments_iterates() {
        let prog = [0x01, 0x02, 0x03];
        let mut buf = vec![0u8; 64];
        let n = encode_load_main_into(&mut buf, &prog);
        let (ver, segs) = load_segments(&buf[..n]).unwrap();
        assert_eq!(ver, LOAD_VERSION);
        let collected: Vec<_> = segs.map(|(k, b)| (k, b.to_vec())).collect();
        assert_eq!(collected, vec![(SEG_KIND_MAIN, prog.to_vec())]);
    }

    #[test]
    fn exec_status_round_trip() {
        for s in [
            ExecStatus::Ok,
            ExecStatus::InvalidOpcode,
            ExecStatus::ProgramTooShort,
            ExecStatus::InvalidLength,
            ExecStatus::DivideByZero,
            ExecStatus::BusError,
        ] {
            assert_eq!(ExecStatus::from_u8(s as u8), Some(s));
        }
    }
}
