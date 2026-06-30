//! 主机侧链路驱动:经 `Transport` 把字节码下发到 MCU,并收集回传的 Trace/Result。
//!
//! [`HostLink`] 封装了帧协议的请求/响应编排:
//! - [`HostLink::load`] 下发主程序字节码,等待 ACK;
//! - [`HostLink::exec`] 触发执行,等待 ACK 后流式收集 Trace 帧并解码为 [`BusOp`],
//!   直到收到 Result 帧得到执行状态码;
//! - [`HostLink::reset`] / [`HostLink::ping`] 复位 / 探活。
//!
//! 协议为锁步(一次只发一个请求),故 ACK 与请求一一对应;Trace 仅在 EXEC 期间出现。

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use rseq_link::frame::{encode_into, FrameDecoder, FrameType, HOST_FRAME_BUF, OVERHEAD};
use rseq_link::wire::{decode_trace, encode_load_main_into, ExecStatus, TraceRef};
use rseq_link::{LinkError, Transport};

use crate::trace::BusOp;

/// 一次 EXEC 的结果:执行状态码 + 期间回传并解码的总线轨迹。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecResult {
    /// MCU 字节码执行的终止状态(Ok 表示正常 Return)。
    pub status: ExecStatus,
    /// EXEC 期间收集到的总线操作,顺序与 MCU 执行顺序一致。
    pub traces: Vec<BusOp>,
}

/// 把解码出的 Trace 记录转成主机侧共享的 [`BusOp`](数据拷贝出帧缓冲)。
impl From<TraceRef<'_>> for BusOp {
    fn from(r: TraceRef<'_>) -> Self {
        match r {
            TraceRef::Read { addr, data } => BusOp::Read { addr, data: data.to_vec() },
            TraceRef::Write { addr, data } => BusOp::Write { addr, data: data.to_vec() },
            TraceRef::Delay { us } => BusOp::Delay { us },
        }
    }
}

/// 主机读缓冲(每次 pump 最多读这么多字节)。
const READ_CHUNK: usize = 256;
/// 默认单次请求等待响应的上限,避免回环/串口异常时永久挂起。
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// 主机侧链路驱动。泛型 `T` 为任意实现 [`Transport`] 的字节流通道
/// (串口、回环管道、或测试用的脚本传输)。
pub struct HostLink<T> {
    transport: T,
    dec: FrameDecoder<HOST_FRAME_BUF>,
    /// 已解码但尚未消费的帧(owned 载荷),按到达顺序排队。
    inbox: VecDeque<(FrameType, Vec<u8>)>,
}

impl<T: Transport> HostLink<T> {
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            dec: FrameDecoder::<HOST_FRAME_BUF>::new(),
            inbox: VecDeque::new(),
        }
    }

    /// 取得内部传输的可变引用(高级用法,如调整串口参数)。
    pub fn transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }

    // ── 帧收发原语 ──────────────────────────────────────────

    fn send_frame(&mut self, ty: FrameType, payload: &[u8]) -> Result<(), LinkError> {
        let mut buf = vec![0u8; payload.len() + OVERHEAD];
        let n = encode_into(ty, payload, &mut buf);
        self.transport.write(&buf[..n])
    }

    /// 从传输读一段字节喂入解码器;读到的完整帧入 inbox。
    fn pump(&mut self) -> Result<(), LinkError> {
        let mut buf = [0u8; READ_CHUNK];
        let n = self.transport.read(&mut buf)?;
        if n == 0 {
            // 无数据(回环管道的瞬时空读):稍退避避免空转烧 CPU。
            // 真实串口的 read 通常会阻塞到超时,不会频繁走这里。
            std::thread::sleep(Duration::from_micros(100));
            return Ok(());
        }
        // 先收集到局部 Vec,避免对 self.dec 与 self.inbox 同时长借用。
        let mut captured: Vec<(FrameType, Vec<u8>)> = Vec::new();
        self.dec.feed(&buf[..n], |ty, p| {
            captured.push((ty, p.to_vec()));
        });
        for f in captured {
            self.inbox.push_back(f);
        }
        Ok(())
    }

    /// 取下一条帧;inbox 空则 pump 到截止时间,超时返回 `None`。
    fn next_frame(&mut self, deadline: Instant) -> Result<Option<(FrameType, Vec<u8>)>, LinkError> {
        loop {
            if let Some(f) = self.inbox.pop_front() {
                return Ok(Some(f));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            self.pump()?;
        }
    }

    /// 等待指定类型的一帧;期间提前到达的其它帧按原顺序塞回 inbox 前部,
    /// 供后续消费(协议锁步下一般不会乱序,此处防御性处理)。
    fn expect(&mut self, want: FrameType, deadline: Instant) -> Result<Vec<u8>, LinkError> {
        let mut deferred: Vec<(FrameType, Vec<u8>)> = Vec::new();
        loop {
            match self.next_frame(deadline)? {
                None => return Err(LinkError::Timeout),
                Some((ty, p)) if ty == want => {
                    for f in deferred.into_iter().rev() {
                        self.inbox.push_front(f);
                    }
                    return Ok(p);
                }
                Some(other) => deferred.push(other),
            }
        }
    }

    // ── 高层协议 ────────────────────────────────────────────

    /// 下发主程序字节码;MCU 收到后回 ACK。bytecode 末尾应以 Return 结尾。
    pub fn load(&mut self, bytecode: &[u8]) -> Result<(), LinkError> {
        let mut payload = vec![0u8; 2 + 3 + bytecode.len()];
        let n = encode_load_main_into(&mut payload, bytecode);
        self.send_frame(FrameType::Load, &payload[..n])?;
        let _ = self.expect(FrameType::Ack, Instant::now() + DEFAULT_TIMEOUT)?;
        Ok(())
    }

    /// 触发执行;返回执行状态与期间回传的总线轨迹。
    pub fn exec(&mut self) -> Result<ExecResult, LinkError> {
        self.send_frame(FrameType::Exec, &[])?;
        let _ = self.expect(FrameType::Ack, Instant::now() + DEFAULT_TIMEOUT)?;
        let deadline = Instant::now() + DEFAULT_TIMEOUT;
        let mut traces: Vec<BusOp> = Vec::new();
        loop {
            let (ty, p) = self
                .next_frame(deadline)?
                .ok_or(LinkError::Timeout)?;
            match ty {
                FrameType::Trace => {
                    if let Some(r) = decode_trace(&p) {
                        traces.push(BusOp::from(r));
                    }
                }
                FrameType::Result => {
                    let status = ExecStatus::from_u8(p.first().copied().unwrap_or(0))
                        .ok_or(LinkError::UnknownType)?;
                    return Ok(ExecResult { status, traces });
                }
                // 锁步协议下不应收到其它帧;防御性忽略。
                _ => {}
            }
        }
    }

    /// 复位 MCU 程序区;收到 ACK 即返回。
    pub fn reset(&mut self) -> Result<(), LinkError> {
        self.send_frame(FrameType::Reset, &[])?;
        let _ = self.expect(FrameType::Ack, Instant::now() + DEFAULT_TIMEOUT)?;
        Ok(())
    }

    /// 探活;收到 Pong 即返回。
    pub fn ping(&mut self) -> Result<(), LinkError> {
        self.send_frame(FrameType::Ping, &[])?;
        let _ = self.expect(FrameType::Pong, Instant::now() + DEFAULT_TIMEOUT)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rseq_link::wire::{encode_trace_delay, encode_trace_rw, TRACE_OP_DELAY, TRACE_OP_READ, TRACE_OP_WRITE};

    /// 脚本式传输:`read` 顺序吐出预置字节,`write` 全捕获到 `writes`。
    /// 用于确定性地测试 HostLink 的帧解析与状态机,无需真实 MCU。
    struct ScriptTransport {
        rx: Vec<u8>,
        pos: usize,
        writes: Vec<u8>,
    }

    impl Transport for ScriptTransport {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize, LinkError> {
            let n = (self.rx.len() - self.pos).min(buf.len());
            if n == 0 {
                return Ok(0);
            }
            buf[..n].copy_from_slice(&self.rx[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
        fn write(&mut self, data: &[u8]) -> Result<(), LinkError> {
            self.writes.extend(data);
            Ok(())
        }
    }

    fn frame(ty: FrameType, payload: &[u8]) -> Vec<u8> {
        let mut buf = vec![0u8; payload.len() + OVERHEAD];
        let n = encode_into(ty, payload, &mut buf);
        buf.truncate(n);
        buf
    }

    fn trace_rw_frame(op: u8, addr: u32, data: &[u8]) -> Vec<u8> {
        let mut buf = vec![0u8; data.len() + 1 + 4 + 2 + OVERHEAD];
        let n = encode_trace_rw(&mut buf, op, addr, data);
        buf.truncate(n);
        buf
    }

    #[test]
    fn exec_collects_traces_and_result() {
        // MCU 预备的响应流:ACK(Exec) + Trace(Read) + Trace(Write) + Trace(Delay) + Result(Ok)
        let mut rx = Vec::new();
        rx.extend(frame(FrameType::Ack, &[]));
        rx.extend(trace_rw_frame(TRACE_OP_READ, 0x10, &[0x01, 0x02]));
        rx.extend(trace_rw_frame(TRACE_OP_WRITE, 0x20, &[0xaa]));
        let mut d = vec![0u8; 32];
        let n = encode_trace_delay(&mut d, 500);
        rx.extend(&d[..n]);
        rx.extend(frame(FrameType::Result, &[ExecStatus::Ok as u8]));

        let mut link = HostLink::new(ScriptTransport { rx, pos: 0, writes: Vec::new() });
        let res = link.exec().unwrap();
        assert_eq!(res.status, ExecStatus::Ok);
        assert_eq!(
            res.traces,
            vec![
                BusOp::Read { addr: 0x10, data: vec![0x01, 0x02] },
                BusOp::Write { addr: 0x20, data: vec![0xaa] },
                BusOp::Delay { us: 500 },
            ]
        );
    }

    #[test]
    fn load_writes_load_frame_and_consumes_ack() {
        let rx = frame(FrameType::Ack, &[]);
        let mut link = HostLink::new(ScriptTransport { rx, pos: 0, writes: Vec::new() });
        link.load(&[0x01, 0x02, 0x03]).unwrap();
        // 写出的首帧应是 LOAD:sync0 sync1 type=Load ...
        let w = &link.transport_mut().writes;
        assert_eq!(&w[..3], &[0x55, 0xAA, FrameType::Load as u8]);
    }

    #[test]
    fn ping_expects_pong() {
        let rx = frame(FrameType::Pong, &[]);
        let mut link = HostLink::new(ScriptTransport { rx, pos: 0, writes: Vec::new() });
        link.ping().unwrap();
        let w = &link.transport_mut().writes;
        assert_eq!(w[2], FrameType::Ping as u8);
    }

    #[test]
    fn trace_ref_to_busop_round_trip() {
        let data = [0x11, 0x22];
        let b: BusOp = TraceRef::Write { addr: 0x1234, data: &data }.into();
        assert_eq!(b, BusOp::Write { addr: 0x1234, data: vec![0x11, 0x22] });
    }
}
