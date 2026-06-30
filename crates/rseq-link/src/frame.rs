//! 帧格式与流式解码器。
//!
//! 帧布局(小端):
//! ```text
//! [sync 0x55 0xAA][type: u8][len: u16 LE][payload: len 字节][crc32: u32 LE]
//! ```
//! CRC32(IEEE)覆盖 `type || len || payload`,不含 sync 与 crc 本身。

use crate::crc32::crc32;

/// 帧头长度:sync(2) + type(1) + len(2)。
pub const HEADER_LEN: usize = 5;
/// CRC 长度。
pub const CRC_LEN: usize = 4;
/// 单帧开销(帧头 + CRC)。
pub const OVERHEAD: usize = HEADER_LEN + CRC_LEN;

pub const SYNC0: u8 = 0x55;
pub const SYNC1: u8 = 0xAA;

/// 一条 Trace 帧(R/W)的最大载荷:op(1)+addr(4)+dlen(2)+data(≤4096) = 4103。
/// 对应整帧最大 4103 + OVERHEAD = 4112。VM 的 read/write 已被限到 4096 字节。
pub const MAX_TRACE_PAYLOAD: usize = 1 + 4 + 2 + 4096;
pub const MAX_TRACE_FRAME: usize = MAX_TRACE_PAYLOAD + OVERHEAD;

/// 主机侧解码缓冲容量:需容纳最大可能帧(LOAD 可携带最大 65535 字节载荷)。
pub const HOST_FRAME_BUF: usize = u16::MAX as usize + 1 + OVERHEAD;

/// 帧类型。bit7 置位表示 MCU→Host 方向。
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameType {
    // Host → MCU
    Load = 0x01,
    Exec = 0x02,
    Reset = 0x03,
    Ping = 0x04,
    // MCU → Host
    Ack = 0x81,
    Trace = 0x82,
    Result = 0x83,
    Pong = 0x84,
}

impl FrameType {
    pub const fn from_u8(n: u8) -> Option<Self> {
        match n {
            0x01 => Some(Self::Load),
            0x02 => Some(Self::Exec),
            0x03 => Some(Self::Reset),
            0x04 => Some(Self::Ping),
            0x81 => Some(Self::Ack),
            0x82 => Some(Self::Trace),
            0x83 => Some(Self::Result),
            0x84 => Some(Self::Pong),
            _ => None,
        }
    }

    pub const fn is_host_to_mcu(self) -> bool {
        matches!(self, Self::Load | Self::Exec | Self::Reset | Self::Ping)
    }
}

/// 解析出的一帧:类型 + 借用载荷。
#[derive(Debug, Clone, Copy)]
pub struct Frame<'a> {
    pub ty: FrameType,
    pub payload: &'a [u8],
}

/// 把一条帧编码进 `out`,返回写入字节数。`out` 需 ≥ `payload.len() + OVERHEAD`。
///
/// 注意:这是同步阻塞写入,逐字节组装;MCU 侧也可用。
pub fn encode_into(ty: FrameType, payload: &[u8], out: &mut [u8]) -> usize {
    let need = payload.len() + OVERHEAD;
    assert!(
        out.len() >= need,
        "encode buffer too small: need {need}, have {}",
        out.len()
    );
    out[0] = SYNC0;
    out[1] = SYNC1;
    out[2] = ty as u8;
    let len = payload.len() as u16;
    out[3] = (len & 0xFF) as u8;
    out[4] = (len >> 8) as u8;
    out[HEADER_LEN..HEADER_LEN + payload.len()].copy_from_slice(payload);
    let crc = crc32(&out[2..HEADER_LEN + payload.len()]);
    let c = HEADER_LEN + payload.len();
    out[c..c + CRC_LEN].copy_from_slice(&crc.to_le_bytes());
    need
}

/// 流式帧解码器,const-generic 缓冲,逐字节/分块喂入,sync 失配自动重同步。
///
/// `N` 为整帧最大字节数(含 sync/type/len/payload/crc)。MCU 按可用 RAM 选取;
/// 主机侧用 [`HOST_FRAME_BUF`]。
pub struct FrameDecoder<const N: usize> {
    buf: [u8; N],
    /// 下一写入位置(语义随 `state` 变化)。
    pos: usize,
    state: State,
    ty: u8,
    len: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// 等待 sync 第一字节 0x55。
    Sync0,
    /// 已见 0x55,等待 0xAA。
    Sync1,
    /// 读 type(1 字节)。
    Type,
    /// 读 len(2 字节)。
    Len,
    /// 读 payload(`len` 字节)。
    Payload,
    /// 读 crc(4 字节)。
    Crc,
}

impl<const N: usize> FrameDecoder<N> {
    pub const fn new() -> Self {
        Self {
            buf: [0; N],
            pos: 0,
            state: State::Sync0,
            ty: 0,
            len: 0,
        }
    }

    /// 喂入一段字节流,每解码出一条完整且 CRC 正确的帧就回调 `on_frame`。
    ///
    /// CRC 错或未知 type 的帧被静默丢弃并从下一字节重新寻找 sync。
    pub fn feed<F: FnMut(FrameType, &[u8])>(&mut self, chunk: &[u8], mut on_frame: F) {
        let mut i = 0;
        while i < chunk.len() {
            let b = chunk[i];
            match self.state {
                State::Sync0 => {
                    if b == SYNC0 {
                        self.state = State::Sync1;
                    }
                    i += 1;
                }
                State::Sync1 => {
                    match b {
                        SYNC1 => {
                            self.pos = 0;
                            self.state = State::Type;
                        }
                        SYNC0 => { /* 连续 0x55,保持在 Sync1 等待 0xAA */ }
                        _ => self.state = State::Sync0,
                    }
                    i += 1;
                }
                State::Type => {
                    if FrameType::from_u8(b).is_none() {
                        // 未知 type:重同步,并重处理本字节(可能是真实帧的 0x55)。
                        self.reset_sync();
                        continue;
                    }
                    self.ty = b;
                    self.buf[0] = b;
                    self.pos = 1;
                    self.state = State::Len;
                    i += 1;
                }
                State::Len => {
                    self.buf[self.pos] = b;
                    self.pos += 1;
                    if self.pos == 3 {
                        self.len = u16::from_le_bytes([self.buf[1], self.buf[2]]);
                        let total = 3 + self.len as usize + CRC_LEN;
                        if total > N {
                            self.reset_sync();
                            continue;
                        }
                        self.pos = 3;
                        self.state = if self.len == 0 { State::Crc } else { State::Payload };
                    }
                    i += 1;
                }
                State::Payload => {
                    self.buf[self.pos] = b;
                    self.pos += 1;
                    if self.pos == 3 + self.len as usize {
                        self.state = State::Crc;
                    }
                    i += 1;
                }
                State::Crc => {
                    self.buf[self.pos] = b;
                    self.pos += 1;
                    if self.pos == 3 + self.len as usize + CRC_LEN {
                        let payload_len = self.len as usize;
                        let computed = crc32(&self.buf[0..3 + payload_len]);
                        let recv = u32::from_le_bytes([
                            self.buf[3 + payload_len],
                            self.buf[3 + payload_len + 1],
                            self.buf[3 + payload_len + 2],
                            self.buf[3 + payload_len + 3],
                        ]);
                        if computed == recv {
                            // from_u8 已在 Type 阶段校验过,这里必为 Some。
                            let ty = FrameType::from_u8(self.ty).unwrap();
                            on_frame(ty, &self.buf[3..3 + payload_len]);
                        }
                        self.reset_sync();
                    }
                    i += 1;
                }
            }
        }
    }

    fn reset_sync(&mut self) {
        self.state = State::Sync0;
        self.pos = 0;
    }
}

impl<const N: usize> Default for FrameDecoder<N> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::prelude::v1::*;

    fn encode(ty: FrameType, payload: &[u8]) -> Vec<u8> {
        let mut buf = vec![0u8; payload.len() + OVERHEAD];
        let n = encode_into(ty, payload, &mut buf);
        buf.truncate(n);
        buf
    }

    #[test]
    fn round_trip_single_frame() {
        let frame = encode(FrameType::Exec, &[]);
        let mut dec = FrameDecoder::<{ HOST_FRAME_BUF }>::new();
        let mut got = None;
        dec.feed(&frame, |ty, p| {
            got = Some((ty, p.to_vec()));
        });
        assert_eq!(got, Some((FrameType::Exec, Vec::new())));
    }

    #[test]
    fn round_trip_with_payload() {
        let payload = [0x01, 0x02, 0x03, 0xaa, 0x55];
        let frame = encode(FrameType::Load, &payload);
        let mut dec = FrameDecoder::<{ HOST_FRAME_BUF }>::new();
        let mut got = None;
        dec.feed(&frame, |ty, p| {
            got = Some((ty, p.to_vec()));
        });
        assert_eq!(got, Some((FrameType::Load, payload.to_vec())));
    }

    #[test]
    fn resync_after_garbage() {
        let payload = [0x10, 0x20];
        let frame = encode(FrameType::Ping, &payload);
        let mut stream = vec![0xff, 0x00, 0x55]; // 垃圾,其中 0x55 不跟 0xAA
        stream.extend_from_slice(&frame);
        let mut dec = FrameDecoder::<{ HOST_FRAME_BUF }>::new();
        let mut got = None;
        dec.feed(&stream, |ty, p| {
            got = Some((ty, p.to_vec()));
        });
        assert_eq!(got, Some((FrameType::Ping, payload.to_vec())));
    }

    #[test]
    fn crc_mismatch_is_dropped() {
        let payload = [0x01, 0x02];
        let mut frame = encode(FrameType::Exec, &payload);
        // 翻转最后一个 CRC 字节制造错误。
        let last = frame.len() - 1;
        frame[last] ^= 0xFF;
        let mut dec = FrameDecoder::<{ HOST_FRAME_BUF }>::new();
        let mut got = 0;
        dec.feed(&frame, |_ty, _p| {
            got += 1;
        });
        assert_eq!(got, 0, "corrupt frame must be dropped");
    }

    #[test]
    fn chunked_feed_decodes() {
        let payload = [0xaa, 0xbb, 0xcc, 0xdd];
        let frame = encode(FrameType::Trace, &payload);
        let mut dec = FrameDecoder::<{ HOST_FRAME_BUF }>::new();
        let mut got = None;
        // 每次喂 1 字节。
        for &b in &frame {
            dec.feed(std::slice::from_ref(&b), |ty, p| {
                got = Some((ty, p.to_vec()));
            });
        }
        assert_eq!(got, Some((FrameType::Trace, payload.to_vec())));
    }

    #[test]
    fn multiple_frames_in_one_chunk() {
        let f1 = encode(FrameType::Ping, &[]);
        let f2 = encode(FrameType::Exec, &[0x01]);
        let mut stream = f1;
        stream.extend_from_slice(&f2);
        let mut dec = FrameDecoder::<{ HOST_FRAME_BUF }>::new();
        let mut got = Vec::new();
        dec.feed(&stream, |ty, p| {
            got.push((ty, p.to_vec()));
        });
        assert_eq!(got.len(), 2);
        assert_eq!(got[0], (FrameType::Ping, Vec::new()));
        assert_eq!(got[1], (FrameType::Exec, vec![0x01]));
    }
}
