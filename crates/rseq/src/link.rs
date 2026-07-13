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

use rseq_link::frame::{FrameDecoder, FrameType, HOST_FRAME_BUF, OVERHEAD, encode_into};
use rseq_link::wire::{
    CONTROL_MAX_READ_LEN, CONTROL_MAX_WRITE_LEN, ControlResultRef, ControlStatus, ExecStatus,
    TraceRef, decode_control_result, decode_trace, encode_control_bus_read_into,
    encode_control_bus_write_into, encode_load_main_into, encode_load_segments_into,
};
use rseq_link::{LinkError, Transport};

use crate::trace::{BusOp, ReportArg, ReportMeta};

/// 一次 EXEC 的结果:执行状态码 + 期间回传并解码的总线轨迹。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecResult {
    /// MCU 字节码执行的终止状态(Ok 表示正常 Return)。
    pub status: ExecStatus,
    /// EXEC 期间收集到的总线操作,顺序与 MCU 执行顺序一致。
    pub traces: Vec<BusOp>,
}

/// 直接控制读的结果。它走 Control/ControlResult 帧，不会替换 MCU 当前加载的 rseq 程序。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlReadResult {
    pub request_id: u16,
    pub addr: u32,
    pub data: Vec<u8>,
}

/// 直接控制写的结果。它走 Control/ControlResult 帧，不会替换 MCU 当前加载的 rseq 程序。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlWriteResult {
    pub request_id: u16,
    pub addr: u32,
    pub len: u16,
}

/// 把解码出的 Trace 记录转成主机侧共享的 [`BusOp`](数据拷贝出帧缓冲)。
impl From<TraceRef<'_>> for BusOp {
    fn from(r: TraceRef<'_>) -> Self {
        match r {
            TraceRef::Read { addr, data } => BusOp::Read {
                addr,
                data: data.to_vec(),
            },
            TraceRef::Write { addr, data } => BusOp::Write {
                addr,
                data: data.to_vec(),
            },
            TraceRef::Delay { us } => BusOp::Delay { us },
            TraceRef::Log { msg } => BusOp::Log {
                msg: String::from_utf8_lossy(msg).into_owned(),
            },
            TraceRef::Irq { pin } => BusOp::Irq { pin },
            TraceRef::BusSelect { kind, arg } => BusOp::BusSelect { kind, arg },
            TraceRef::Report { meta, kind, args } => BusOp::Report {
                meta: meta.map(|meta| ReportMeta {
                    flags: meta.flags,
                    frame_id: meta.frame_id,
                    timestamp_us: meta.timestamp_us,
                }),
                kind,
                args: args
                    .as_slice()
                    .iter()
                    .map(|arg| match arg {
                        rseq_link::wire::ReportArgRef::U32(v) => ReportArg::U32(*v),
                        rseq_link::wire::ReportArgRef::Bytes(bytes) => {
                            ReportArg::Bytes(bytes.to_vec())
                        }
                    })
                    .collect(),
            },
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
    /// EXEC 期间等待 Trace/Result 流的超时上限。中断脚本（含 `wait!`）
    /// 需要更长的等待，故暴露 [`HostLink::set_exec_timeout`] 供 CLI 调整。
    exec_timeout: Duration,
    next_control_id: u16,
}

impl<T: Transport> HostLink<T> {
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            dec: FrameDecoder::<HOST_FRAME_BUF>::new(),
            inbox: VecDeque::new(),
            exec_timeout: DEFAULT_TIMEOUT,
            next_control_id: 1,
        }
    }

    /// 取得内部传输的可变引用(高级用法,如调整串口参数)。
    pub fn transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }

    /// 设置 EXEC 期间等待 Trace/Result 流的超时上限。中断脚本需比默认 5s 更长。
    pub fn set_exec_timeout(&mut self, timeout: Duration) {
        self.exec_timeout = timeout;
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

    /// 等待一条控制响应，同时丢弃期间积压的 Trace/Result。
    ///
    /// 用于 `Stop`/`Reset` 这类“主机要夺回控制权”的命令：MCU 可能已经在
    /// 连续上报 FIFO 数据，若把所有旧 Trace 都 deferred 到 inbox，ACK 还没到
    /// 主机内存就会被历史数据拖住。这里保留非观测类帧，丢弃观测流。
    fn expect_control_frame(
        &mut self,
        want: FrameType,
        deadline: Instant,
    ) -> Result<Vec<u8>, LinkError> {
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
                Some((FrameType::Trace | FrameType::Result, _)) => {
                    // Drop stale observations while waiting for a control response.
                }
                Some(other) => deferred.push(other),
            }
        }
    }

    fn expect_control_ack(&mut self, deadline: Instant) -> Result<(), LinkError> {
        let _ = self.expect_control_frame(FrameType::Ack, deadline)?;
        Ok(())
    }

    // ── 高层协议 ────────────────────────────────────────────

    /// 下发主程序字节码;MCU 收到后回 ACK。bytecode 末尾应以 Return 结尾。
    pub fn load(&mut self, bytecode: &[u8]) -> Result<(), LinkError> {
        let mut payload = vec![0u8; 2 + 3 + bytecode.len()];
        let n = encode_load_main_into(&mut payload, bytecode);
        self.send_frame(FrameType::Load, &payload[..n])?;
        self.expect_control_ack(Instant::now() + DEFAULT_TIMEOUT)?;
        Ok(())
    }

    /// 下发多段字节码（main + irq 段）;MCU 收到后回 ACK。
    pub fn load_segments(&mut self, segments: &[(u8, &[u8])]) -> Result<(), LinkError> {
        let mut total_len = 2; // version + seg_count
        for (_, bytes) in segments {
            total_len += 3 + bytes.len(); // kind + len(u16) + bytecode
        }
        let mut payload = vec![0u8; total_len];
        let n = encode_load_segments_into(&mut payload, segments);
        self.send_frame(FrameType::Load, &payload[..n])?;
        self.expect_control_ack(Instant::now() + DEFAULT_TIMEOUT)?;
        Ok(())
    }

    /// 触发执行;返回执行状态与期间回传的总线轨迹。
    pub fn exec(&mut self) -> Result<ExecResult, LinkError> {
        self.send_frame(FrameType::Exec, &[])?;
        self.expect_control_ack(Instant::now() + DEFAULT_TIMEOUT)?;
        let deadline = Instant::now() + self.exec_timeout;
        let mut traces: Vec<BusOp> = Vec::new();
        loop {
            let (ty, p) = self.next_frame(deadline)?.ok_or(LinkError::Timeout)?;
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

    /// 在 EXEC 之外继续观察 MCU→Host Trace。用于自动 IRQ handler 后台上报。
    /// 超时返回 `Ok(None)`，调用方可继续轮询或用于刷新 UI。
    pub fn observe_next_trace(&mut self, timeout: Duration) -> Result<Option<BusOp>, LinkError> {
        let deadline = Instant::now() + timeout;
        loop {
            let Some((ty, p)) = self.next_frame(deadline)? else {
                return Ok(None);
            };
            if ty == FrameType::Trace {
                if let Some(r) = decode_trace(&p) {
                    return Ok(Some(BusOp::from(r)));
                }
            }
        }
    }

    fn alloc_control_id(&mut self) -> u16 {
        let id = self.next_control_id;
        self.next_control_id = self.next_control_id.wrapping_add(1);
        if self.next_control_id == 0 {
            self.next_control_id = 1;
        }
        id
    }

    /// 直接读取 MCU 当前物理总线上的一段寄存器。
    ///
    /// 这条路径使用 Control/ControlResult 帧，不会 LOAD/EXEC 临时脚本，也不会清除
    /// 已注册的 IRQ handler。等待响应期间收到的 Trace 会被丢弃；UI 类调用方若想
    /// 保留 Trace 流，应使用 [`HostLink::control_read_observing`]。
    pub fn control_read(&mut self, addr: u32, len: u16) -> Result<ControlReadResult, LinkError> {
        self.control_read_observing(addr, len, DEFAULT_TIMEOUT, |_| {})
    }

    /// 直接读取寄存器，并在等待 ControlResult 时把穿插到来的 Trace 交给回调。
    pub fn control_read_observing<F>(
        &mut self,
        addr: u32,
        len: u16,
        timeout: Duration,
        mut on_trace: F,
    ) -> Result<ControlReadResult, LinkError>
    where
        F: FnMut(BusOp),
    {
        if len == 0 || len as usize > CONTROL_MAX_READ_LEN {
            return Err(LinkError::Nak(ControlStatus::AccessSizeMismatch as u8));
        }

        let request_id = self.alloc_control_id();
        let mut payload = [0u8; 1 + 2 + 4 + 2];
        let n = encode_control_bus_read_into(&mut payload, request_id, addr, len);
        self.send_frame(FrameType::Control, &payload[..n])?;

        let deadline = Instant::now() + timeout;
        loop {
            let (ty, p) = self.next_frame(deadline)?.ok_or(LinkError::Timeout)?;
            match ty {
                FrameType::ControlResult => {
                    let Some(ControlResultRef::BusRead {
                        request_id: got_id,
                        status,
                        addr: got_addr,
                        data,
                    }) = decode_control_result(&p)
                    else {
                        return Err(LinkError::UnknownType);
                    };
                    if got_id != request_id {
                        continue;
                    }
                    if status != ControlStatus::Ok {
                        return Err(LinkError::Nak(status as u8));
                    }
                    return Ok(ControlReadResult {
                        request_id: got_id,
                        addr: got_addr,
                        data: data.to_vec(),
                    });
                }
                FrameType::Trace => {
                    if let Some(r) = decode_trace(&p) {
                        on_trace(BusOp::from(r));
                    }
                }
                FrameType::Result => {
                    // A stale EXEC result can be left in a long-running report stream; ignore it.
                }
                _ => {}
            }
        }
    }

    /// 直接写入 MCU 当前物理总线上的一段寄存器。
    ///
    /// 这条路径使用 Control/ControlResult 帧，不会 LOAD/EXEC 临时脚本，也不会清除
    /// 已注册的 IRQ handler。等待响应期间收到的 Trace 会被丢弃；UI 类调用方若想
    /// 保留 Trace 流，应使用 [`HostLink::control_write_observing`]。
    pub fn control_write(
        &mut self,
        addr: u32,
        data: &[u8],
    ) -> Result<ControlWriteResult, LinkError> {
        self.control_write_observing(addr, data, DEFAULT_TIMEOUT, |_| {})
    }

    /// 直接写入寄存器，并在等待 ControlResult 时把穿插到来的 Trace 交给回调。
    pub fn control_write_observing<F>(
        &mut self,
        addr: u32,
        data: &[u8],
        timeout: Duration,
        mut on_trace: F,
    ) -> Result<ControlWriteResult, LinkError>
    where
        F: FnMut(BusOp),
    {
        if data.is_empty() || data.len() > CONTROL_MAX_WRITE_LEN {
            return Err(LinkError::Nak(ControlStatus::AccessSizeMismatch as u8));
        }

        let request_id = self.alloc_control_id();
        let mut payload = vec![0u8; 1 + 2 + 4 + 2 + data.len()];
        let n = encode_control_bus_write_into(&mut payload, request_id, addr, data);
        self.send_frame(FrameType::Control, &payload[..n])?;

        let deadline = Instant::now() + timeout;
        loop {
            let (ty, p) = self.next_frame(deadline)?.ok_or(LinkError::Timeout)?;
            match ty {
                FrameType::ControlResult => {
                    let Some(ControlResultRef::BusWrite {
                        request_id: got_id,
                        status,
                        addr: got_addr,
                        len,
                    }) = decode_control_result(&p)
                    else {
                        return Err(LinkError::UnknownType);
                    };
                    if got_id != request_id {
                        continue;
                    }
                    if status != ControlStatus::Ok {
                        return Err(LinkError::Nak(status as u8));
                    }
                    return Ok(ControlWriteResult {
                        request_id: got_id,
                        addr: got_addr,
                        len,
                    });
                }
                FrameType::Trace => {
                    if let Some(r) = decode_trace(&p) {
                        on_trace(BusOp::from(r));
                    }
                }
                FrameType::Result => {
                    // A stale EXEC result can be left in a long-running report stream; ignore it.
                }
                _ => {}
            }
        }
    }

    /// 复位 MCU 程序区;收到 ACK 即返回。
    pub fn reset(&mut self) -> Result<(), LinkError> {
        self.send_frame(FrameType::Reset, &[])?;
        self.expect_control_ack(Instant::now() + DEFAULT_TIMEOUT)?;
        Ok(())
    }

    /// 请求 MCU 停止后台 IRQ/report 流。MCU 会清除已注册的 IRQ handler 与
    /// pending 标志，但不会擦除已加载的主程序字节码。
    pub fn stop_reports(&mut self) -> Result<(), LinkError> {
        self.send_frame(FrameType::Stop, &[])?;
        self.expect_control_ack(Instant::now() + DEFAULT_TIMEOUT)?;
        Ok(())
    }

    /// Temporarily suspend background IRQ/report execution without removing the
    /// loaded or active IRQ handlers. Intended for short register-control
    /// transactions that must not compete with a high-rate report stream.
    pub fn pause_reports(&mut self) -> Result<(), LinkError> {
        self.pause_reports_timeout(DEFAULT_TIMEOUT)
    }

    /// Same as [`HostLink::pause_reports`], but with a caller supplied timeout.
    ///
    /// UI control changes use this as a best-effort fast path: new firmware will
    /// ACK quickly and pause the MCU report loop, while older firmware can time
    /// out quickly and let the host fall back to suppressing reports locally.
    pub fn pause_reports_timeout(&mut self, timeout: Duration) -> Result<(), LinkError> {
        self.send_frame(FrameType::Pause, &[])?;
        self.expect_control_ack(Instant::now() + timeout)?;
        Ok(())
    }

    /// Resume background IRQ/report execution after [`HostLink::pause_reports`].
    pub fn resume_reports(&mut self) -> Result<(), LinkError> {
        self.resume_reports_timeout(DEFAULT_TIMEOUT)
    }

    /// Same as [`HostLink::resume_reports`], but with a caller supplied timeout.
    pub fn resume_reports_timeout(&mut self, timeout: Duration) -> Result<(), LinkError> {
        self.send_frame(FrameType::Resume, &[])?;
        self.expect_control_ack(Instant::now() + timeout)?;
        Ok(())
    }

    /// 探活;收到 Pong 即返回。
    pub fn ping(&mut self) -> Result<(), LinkError> {
        self.send_frame(FrameType::Ping, &[])?;
        let _ = self.expect_control_frame(FrameType::Pong, Instant::now() + DEFAULT_TIMEOUT)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rseq_link::wire::{
        REPORT_FLAG_TIMESTAMP_VALID, ReportArgRef, ReportMeta as WireReportMeta, TRACE_OP_READ,
        TRACE_OP_WRITE, decode_control_request, encode_control_bus_read_result_into,
        encode_control_bus_write_result_into, encode_trace_delay, encode_trace_log,
        encode_trace_report, encode_trace_report_v2, encode_trace_rw,
    };

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

        let mut link = HostLink::new(ScriptTransport {
            rx,
            pos: 0,
            writes: Vec::new(),
        });
        let res = link.exec().unwrap();
        assert_eq!(res.status, ExecStatus::Ok);
        assert_eq!(
            res.traces,
            vec![
                BusOp::Read {
                    addr: 0x10,
                    data: vec![0x01, 0x02]
                },
                BusOp::Write {
                    addr: 0x20,
                    data: vec![0xaa]
                },
                BusOp::Delay { us: 500 },
            ]
        );
    }

    #[test]
    fn load_writes_load_frame_and_consumes_ack() {
        let rx = frame(FrameType::Ack, &[]);
        let mut link = HostLink::new(ScriptTransport {
            rx,
            pos: 0,
            writes: Vec::new(),
        });
        link.load(&[0x01, 0x02, 0x03]).unwrap();
        // 写出的首帧应是 LOAD:sync0 sync1 type=Load ...
        let w = &link.transport_mut().writes;
        assert_eq!(&w[..3], &[0x55, 0xAA, FrameType::Load as u8]);
    }

    #[test]
    fn load_drops_stale_traces_until_ack() {
        let mut rx = Vec::new();
        let mut stale = vec![0u8; 32];
        let n = encode_trace_log(&mut stale, "old report");
        rx.extend(&stale[..n]);
        rx.extend(frame(FrameType::Ack, &[]));

        let mut link = HostLink::new(ScriptTransport {
            rx,
            pos: 0,
            writes: Vec::new(),
        });
        link.load(&[0x01, 0x02, 0x03]).unwrap();
        let w = &link.transport_mut().writes;
        assert_eq!(w[2], FrameType::Load as u8);
        assert!(link.inbox.is_empty());
    }

    #[test]
    fn exec_drops_stale_traces_before_ack() {
        let mut rx = Vec::new();
        let mut stale = vec![0u8; 32];
        let n = encode_trace_log(&mut stale, "old report");
        rx.extend(&stale[..n]);
        rx.extend(frame(FrameType::Ack, &[]));
        rx.extend(trace_rw_frame(TRACE_OP_READ, 0x10, &[0x01]));
        rx.extend(frame(FrameType::Result, &[ExecStatus::Ok as u8]));

        let mut link = HostLink::new(ScriptTransport {
            rx,
            pos: 0,
            writes: Vec::new(),
        });
        let res = link.exec().unwrap();
        assert_eq!(res.status, ExecStatus::Ok);
        assert_eq!(
            res.traces,
            vec![BusOp::Read {
                addr: 0x10,
                data: vec![0x01]
            }]
        );
    }

    #[test]
    fn ping_expects_pong() {
        let rx = frame(FrameType::Pong, &[]);
        let mut link = HostLink::new(ScriptTransport {
            rx,
            pos: 0,
            writes: Vec::new(),
        });
        link.ping().unwrap();
        let w = &link.transport_mut().writes;
        assert_eq!(w[2], FrameType::Ping as u8);
    }

    #[test]
    fn stop_reports_drops_stale_traces_until_ack() {
        let mut rx = Vec::new();
        let mut stale = vec![0u8; 32];
        let n = encode_trace_log(&mut stale, "stale");
        rx.extend(&stale[..n]);
        rx.extend(frame(FrameType::Ack, &[]));

        let mut link = HostLink::new(ScriptTransport {
            rx,
            pos: 0,
            writes: Vec::new(),
        });
        link.stop_reports().unwrap();
        let w = &link.transport_mut().writes;
        assert_eq!(w[2], FrameType::Stop as u8);
        assert!(link.inbox.is_empty());
    }

    #[test]
    fn pause_and_resume_reports_use_dedicated_frames() {
        let rx = [frame(FrameType::Ack, &[]), frame(FrameType::Ack, &[])].concat();
        let mut link = HostLink::new(ScriptTransport {
            rx,
            pos: 0,
            writes: Vec::new(),
        });

        link.pause_reports().unwrap();
        link.resume_reports().unwrap();

        let writes = &link.transport_mut().writes;
        assert_eq!(writes[2], FrameType::Pause as u8);
        let second = frame(FrameType::Pause, &[]).len();
        assert_eq!(writes[second + 2], FrameType::Resume as u8);
    }

    #[test]
    fn control_read_writes_control_frame_and_reads_result() {
        let mut result_payload = vec![0u8; 32];
        let n = encode_control_bus_read_result_into(
            &mut result_payload,
            1,
            ControlStatus::Ok,
            0x2e,
            &[0x44, 0x55],
        );
        let rx = frame(FrameType::ControlResult, &result_payload[..n]);

        let mut link = HostLink::new(ScriptTransport {
            rx,
            pos: 0,
            writes: Vec::new(),
        });
        let res = link.control_read(0x2e, 2).unwrap();

        assert_eq!(
            res,
            ControlReadResult {
                request_id: 1,
                addr: 0x2e,
                data: vec![0x44, 0x55]
            }
        );
        let w = &link.transport_mut().writes;
        assert_eq!(w[2], FrameType::Control as u8);
        let payload_len = u16::from_le_bytes([w[3], w[4]]) as usize;
        assert_eq!(
            decode_control_request(&w[5..5 + payload_len]),
            Some(rseq_link::wire::ControlRequestRef::BusRead {
                request_id: 1,
                addr: 0x2e,
                len: 2
            })
        );
    }

    #[test]
    fn control_read_observes_trace_while_waiting_for_result() {
        let mut rx = Vec::new();
        rx.extend(trace_rw_frame(TRACE_OP_READ, 0x10, &[0x01]));
        let mut result_payload = vec![0u8; 32];
        let n = encode_control_bus_read_result_into(
            &mut result_payload,
            1,
            ControlStatus::Ok,
            0x2e,
            &[0xaa],
        );
        rx.extend(frame(FrameType::ControlResult, &result_payload[..n]));

        let mut link = HostLink::new(ScriptTransport {
            rx,
            pos: 0,
            writes: Vec::new(),
        });
        let mut observed = Vec::new();
        let res = link
            .control_read_observing(0x2e, 1, DEFAULT_TIMEOUT, |op| observed.push(op))
            .unwrap();

        assert_eq!(res.data, vec![0xaa]);
        assert_eq!(
            observed,
            vec![BusOp::Read {
                addr: 0x10,
                data: vec![0x01]
            }]
        );
    }

    #[test]
    fn control_write_writes_control_frame_and_reads_result() {
        let mut result_payload = vec![0u8; 16];
        let n = encode_control_bus_write_result_into(
            &mut result_payload,
            1,
            ControlStatus::Ok,
            0x20,
            2,
        );
        let rx = frame(FrameType::ControlResult, &result_payload[..n]);

        let mut link = HostLink::new(ScriptTransport {
            rx,
            pos: 0,
            writes: Vec::new(),
        });
        let res = link.control_write(0x20, &[0xaa, 0x55]).unwrap();

        assert_eq!(
            res,
            ControlWriteResult {
                request_id: 1,
                addr: 0x20,
                len: 2
            }
        );
        let w = &link.transport_mut().writes;
        assert_eq!(w[2], FrameType::Control as u8);
        let payload_len = u16::from_le_bytes([w[3], w[4]]) as usize;
        assert_eq!(
            decode_control_request(&w[5..5 + payload_len]),
            Some(rseq_link::wire::ControlRequestRef::BusWrite {
                request_id: 1,
                addr: 0x20,
                data: &[0xaa, 0x55]
            })
        );
    }

    #[test]
    fn trace_ref_to_busop_round_trip() {
        let data = [0x11, 0x22];
        let b: BusOp = TraceRef::Write {
            addr: 0x1234,
            data: &data,
        }
        .into();
        assert_eq!(
            b,
            BusOp::Write {
                addr: 0x1234,
                data: vec![0x11, 0x22]
            }
        );
    }

    #[test]
    fn exec_collects_log_trace() {
        // MCU 响应流：ACK(Exec) + Log trace("hello") + Result(Ok)
        let mut rx = Vec::new();
        rx.extend(frame(FrameType::Ack, &[]));
        let mut lb = vec![0u8; 32];
        let ln = encode_trace_log(&mut lb, "hello");
        rx.extend(&lb[..ln]);
        rx.extend(frame(FrameType::Result, &[ExecStatus::Ok as u8]));

        let mut link = HostLink::new(ScriptTransport {
            rx,
            pos: 0,
            writes: Vec::new(),
        });
        let res = link.exec().unwrap();
        assert_eq!(res.status, ExecStatus::Ok);
        assert_eq!(
            res.traces,
            vec![BusOp::Log {
                msg: "hello".to_string()
            }]
        );
    }

    #[test]
    fn exec_collects_irq_trace() {
        // MCU 响应流：ACK(Exec) + Irq trace(pin=0) + Result(Ok)
        use rseq_link::wire::encode_trace_irq;
        let mut rx = Vec::new();
        rx.extend(frame(FrameType::Ack, &[]));
        let mut ib = vec![0u8; 32];
        let n = encode_trace_irq(&mut ib, 0);
        rx.extend(&ib[..n]);
        rx.extend(frame(FrameType::Result, &[ExecStatus::Ok as u8]));

        let mut link = HostLink::new(ScriptTransport {
            rx,
            pos: 0,
            writes: Vec::new(),
        });
        let res = link.exec().unwrap();
        assert_eq!(res.status, ExecStatus::Ok);
        assert_eq!(res.traces, vec![BusOp::Irq { pin: 0 }]);
    }

    #[test]
    fn exec_collects_report_trace() {
        let mut rx = Vec::new();
        rx.extend(frame(FrameType::Ack, &[]));
        let mut rb = vec![0u8; 64];
        let n = encode_trace_report(
            &mut rb,
            0x10,
            &[ReportArgRef::U32(42), ReportArgRef::Bytes(&[0xde, 0xad])],
        );
        rx.extend(&rb[..n]);
        rx.extend(frame(FrameType::Result, &[ExecStatus::Ok as u8]));

        let mut link = HostLink::new(ScriptTransport {
            rx,
            pos: 0,
            writes: Vec::new(),
        });
        let res = link.exec().unwrap();
        assert_eq!(res.status, ExecStatus::Ok);
        assert_eq!(
            res.traces,
            vec![BusOp::Report {
                meta: None,
                kind: 0x10,
                args: vec![ReportArg::U32(42), ReportArg::Bytes(vec![0xde, 0xad]),],
            }]
        );
    }

    #[test]
    fn observe_next_trace_receives_without_writing() {
        let mut rx = Vec::new();
        let mut rb = vec![0u8; 64];
        let n = encode_trace_report(
            &mut rb,
            0x10,
            &[ReportArgRef::U32(42), ReportArgRef::Bytes(&[0xde, 0xad])],
        );
        rx.extend(&rb[..n]);

        let mut link = HostLink::new(ScriptTransport {
            rx,
            pos: 0,
            writes: Vec::new(),
        });
        let op = link
            .observe_next_trace(Duration::from_secs(1))
            .unwrap()
            .unwrap();

        assert_eq!(
            op,
            BusOp::Report {
                meta: None,
                kind: 0x10,
                args: vec![ReportArg::U32(42), ReportArg::Bytes(vec![0xde, 0xad]),],
            }
        );
        assert!(link.transport_mut().writes.is_empty());
    }

    #[test]
    fn observe_next_trace_preserves_report_v2_meta() {
        let mut rx = Vec::new();
        let mut rb = vec![0u8; 96];
        let n = encode_trace_report_v2(
            &mut rb,
            WireReportMeta {
                flags: REPORT_FLAG_TIMESTAMP_VALID,
                frame_id: 7,
                timestamp_us: 123_456,
            },
            0x10,
            &[ReportArgRef::U32(42)],
        );
        rx.extend(&rb[..n]);

        let mut link = HostLink::new(ScriptTransport {
            rx,
            pos: 0,
            writes: Vec::new(),
        });
        let op = link
            .observe_next_trace(Duration::from_secs(1))
            .unwrap()
            .unwrap();

        assert_eq!(
            op,
            BusOp::Report {
                meta: Some(ReportMeta {
                    flags: REPORT_FLAG_TIMESTAMP_VALID,
                    frame_id: 7,
                    timestamp_us: 123_456,
                }),
                kind: 0x10,
                args: vec![ReportArg::U32(42)],
            }
        );
    }
}
