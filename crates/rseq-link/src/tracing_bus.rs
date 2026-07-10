//! 把真实目标总线包一层,在每次总线操作后向主机回传 Trace 帧。
//!
//! MCU 端 = 真实 I2C/SPI `Bus` 实现 + `TracingBus` + `LinkTx`(UART)。
//! TX 失败时尽量不影响总线操作本身(观测是尽力而为)。

use core::sync::atomic::{AtomicU32, Ordering};

use rseq_vm::{Bus, BusError, ReportArg};

use crate::error::LinkError;
use crate::frame::MAX_TRACE_FRAME;
use crate::wire::{
    MAX_REPORT_ARGS, REPORT_FLAG_TIMESTAMP_VALID, ReportArgRef, ReportMeta,
    encode_trace_bus_select, encode_trace_delay, encode_trace_irq, encode_trace_log,
    encode_trace_report_v2, encode_trace_rw,
};

static NEXT_REPORT_FRAME_ID: AtomicU32 = AtomicU32::new(1);

/// 只能发送字节流的链路出口。`TracingBus` 只发不收。
pub trait LinkTx {
    fn write(&mut self, data: &[u8]) -> Result<(), LinkError>;
}

/// 任何实现了 [`Transport`](crate::transport::Transport) 的对象都可作 `LinkTx`。
impl<T: crate::transport::Transport> LinkTx for T {
    fn write(&mut self, data: &[u8]) -> Result<(), LinkError> {
        crate::transport::Transport::write(self, data)
    }
}

struct TraceCommon<B, L, const BUF: usize> {
    pub inner: B,
    tx: L,
    buf: [u8; BUF],
    clock_us: Option<fn() -> u64>,
}

impl<B, L: LinkTx, const BUF: usize> TraceCommon<B, L, BUF> {
    fn next_report_meta(&self) -> ReportMeta {
        let timestamp_us = self.clock_us.map(|clock| clock()).unwrap_or(0);
        let flags = if self.clock_us.is_some() {
            REPORT_FLAG_TIMESTAMP_VALID
        } else {
            0
        };
        ReportMeta {
            flags,
            frame_id: NEXT_REPORT_FRAME_ID.fetch_add(1, Ordering::Relaxed),
            timestamp_us,
        }
    }

    fn send(&mut self, frame_len: usize) {
        // 观测失败不应中断寄存器操作,只尽力发送。
        let _ = self.tx.write(&self.buf[..frame_len]);
    }

    fn send_report(&mut self, kind: u32, args: &[ReportArg<'_>]) -> Result<(), BusError> {
        if args.len() > MAX_REPORT_ARGS {
            return Err(BusError::AccessSizeMismatch);
        }
        let mut wire_args = [ReportArgRef::U32(0); MAX_REPORT_ARGS];
        for (slot, arg) in wire_args.iter_mut().zip(args.iter()) {
            *slot = match arg {
                ReportArg::U32(v) => ReportArgRef::U32(*v),
                ReportArg::Bytes(bytes) => ReportArgRef::Bytes(bytes),
            };
        }
        let meta = self.next_report_meta();
        let n = encode_trace_report_v2(&mut self.buf, meta, kind, &wire_args[..args.len()]);
        self.send(n);
        Ok(())
    }
}

/// 包裹真实总线 `B`,在每次 `read`/`write`/`delay_us` 后经 `L` 回传一条 Trace 帧。
///
/// `BUF` 为内部帧缓冲大小,默认 [`MAX_TRACE_FRAME`]——可容纳 VM 最大 4096 字节
/// 读写产生的 Trace 帧。若 MCU RAM 紧张且已知读写更短,可显式指定更小的 `BUF`。
pub struct TracingBus<B, L, const BUF: usize = MAX_TRACE_FRAME> {
    common: TraceCommon<B, L, BUF>,
}

/// 只回传 `report!` 的轻量总线包装器。
///
/// MCU 自动 IRQ handler 会频繁读取 FIFO 并上报数据；如果同时把每次
/// `read!`/`write!` trace 也发回 host，慢速 UART 会在关键路径中阻塞 FIFO
/// drain。这个包装器保留真实总线访问和 `report!` 元数据，但抑制普通 bus
/// trace，适合持续实时上报。
pub struct ReportOnlyBus<B, L, const BUF: usize = MAX_TRACE_FRAME> {
    common: TraceCommon<B, L, BUF>,
}

/// 默认缓冲(`MAX_TRACE_FRAME`)的构造入口——多数场景直接用这个,
/// 调用方无需指定 const 参数。
impl<B, L: LinkTx> TracingBus<B, L, MAX_TRACE_FRAME> {
    pub fn new(inner: B, tx: L) -> Self {
        Self {
            common: TraceCommon {
                inner,
                tx,
                buf: [0; MAX_TRACE_FRAME],
                clock_us: None,
            },
        }
    }

    /// 使用调用方提供的单调时钟构造。`report!` Trace 会携带该时钟读数
    /// 作为 `timestamp_us`。
    pub fn new_with_clock(inner: B, tx: L, clock_us: fn() -> u64) -> Self {
        Self {
            common: TraceCommon {
                inner,
                tx,
                buf: [0; MAX_TRACE_FRAME],
                clock_us: Some(clock_us),
            },
        }
    }
}

impl<B, L: LinkTx> ReportOnlyBus<B, L, MAX_TRACE_FRAME> {
    pub fn new(inner: B, tx: L) -> Self {
        Self {
            common: TraceCommon {
                inner,
                tx,
                buf: [0; MAX_TRACE_FRAME],
                clock_us: None,
            },
        }
    }

    pub fn new_with_clock(inner: B, tx: L, clock_us: fn() -> u64) -> Self {
        Self {
            common: TraceCommon {
                inner,
                tx,
                buf: [0; MAX_TRACE_FRAME],
                clock_us: Some(clock_us),
            },
        }
    }
}

impl<B, L: LinkTx, const BUF: usize> TracingBus<B, L, BUF> {
    /// 用指定大小的缓冲构造。`BUF` 必须 ≥ [`MAX_TRACE_FRAME`]——
    /// 编译期无法约束 const generic 下界,在构造时断言一次,避免运行期越界。
    pub fn with_buf(inner: B, tx: L) -> Self {
        assert!(
            BUF >= MAX_TRACE_FRAME,
            "TracingBus BUF must be >= MAX_TRACE_FRAME ({MAX_TRACE_FRAME})"
        );
        Self {
            common: TraceCommon {
                inner,
                tx,
                buf: [0; BUF],
                clock_us: None,
            },
        }
    }

    /// 用指定大小的缓冲与单调时钟构造。
    pub fn with_buf_and_clock(inner: B, tx: L, clock_us: fn() -> u64) -> Self {
        assert!(
            BUF >= MAX_TRACE_FRAME,
            "TracingBus BUF must be >= MAX_TRACE_FRAME ({MAX_TRACE_FRAME})"
        );
        Self {
            common: TraceCommon {
                inner,
                tx,
                buf: [0; BUF],
                clock_us: Some(clock_us),
            },
        }
    }

    /// 取得内部真实总线的可变引用(少数需要直接访问硬件的场景)。
    pub fn inner_mut(&mut self) -> &mut B {
        &mut self.common.inner
    }

    /// 拆出内部总线与链路出口(销毁 `TracingBus`)。
    /// MCU 端每次 EXEC 后用此回收总线对象,并释放对 transport 的借用。
    pub fn into_inner(self) -> (B, L) {
        (self.common.inner, self.common.tx)
    }
}

impl<B, L: LinkTx, const BUF: usize> ReportOnlyBus<B, L, BUF> {
    pub fn with_buf(inner: B, tx: L) -> Self {
        assert!(
            BUF >= MAX_TRACE_FRAME,
            "ReportOnlyBus BUF must be >= MAX_TRACE_FRAME ({MAX_TRACE_FRAME})"
        );
        Self {
            common: TraceCommon {
                inner,
                tx,
                buf: [0; BUF],
                clock_us: None,
            },
        }
    }

    pub fn with_buf_and_clock(inner: B, tx: L, clock_us: fn() -> u64) -> Self {
        assert!(
            BUF >= MAX_TRACE_FRAME,
            "ReportOnlyBus BUF must be >= MAX_TRACE_FRAME ({MAX_TRACE_FRAME})"
        );
        Self {
            common: TraceCommon {
                inner,
                tx,
                buf: [0; BUF],
                clock_us: Some(clock_us),
            },
        }
    }

    pub fn inner_mut(&mut self) -> &mut B {
        &mut self.common.inner
    }

    pub fn into_inner(self) -> (B, L) {
        (self.common.inner, self.common.tx)
    }
}

impl<B: Bus, L: LinkTx, const BUF: usize> Bus for TracingBus<B, L, BUF> {
    fn set_bus_kind(&mut self, kind: rseq_vm::BusKind, arg: u32) -> Result<(), BusError> {
        self.common.inner.set_bus_kind(kind, arg)?;
        let n = encode_trace_bus_select(&mut self.common.buf, kind, arg);
        self.common.send(n);
        Ok(())
    }

    fn read(&mut self, addr: u32, data: &mut [u8]) -> Result<(), BusError> {
        self.common.inner.read(addr, data)?;
        // VM 限制 read len ≤ 4096,故 data.len() ≤ MAX_TRACE_PAYLOAD 的 data 部分。
        let n = encode_trace_rw(&mut self.common.buf, crate::wire::TRACE_OP_READ, addr, data);
        self.common.send(n);
        Ok(())
    }

    fn write(&mut self, addr: u32, data: &[u8]) -> Result<(), BusError> {
        self.common.inner.write(addr, data)?;
        let n = encode_trace_rw(&mut self.common.buf, crate::wire::TRACE_OP_WRITE, addr, data);
        self.common.send(n);
        Ok(())
    }

    fn delay_us(&mut self, us: u32) -> Result<(), BusError> {
        self.common.inner.delay_us(us)?;
        let n = encode_trace_delay(&mut self.common.buf, us);
        self.common.send(n);
        Ok(())
    }

    /// `print!`：先让真总线打印（真机=printk），再回传一条 Log trace 给主机。
    /// 观测失败不影响总线动作本身（尽力发送）。
    fn log(&mut self, msg: &str) -> Result<(), BusError> {
        self.common.inner.log(msg)?;
        let n = encode_trace_log(&mut self.common.buf, msg);
        self.common.send(n);
        Ok(())
    }

    /// `wait!(pin)`：先让真总线阻塞等待边沿（真机 IMU 总线在 PB8 上
    /// `k_sem_take`），命中后再回传一条 Irq trace 给主机，标记此处发生过
    /// 一次中断。inner 超时返回 `Timeout` 时直接传播，不发 trace。
    fn wait_irq(&mut self, pin: u8, timeout_ms: u32) -> Result<(), BusError> {
        self.common.inner.wait_irq(pin, timeout_ms)?;
        let n = encode_trace_irq(&mut self.common.buf, pin);
        self.common.send(n);
        Ok(())
    }

    /// `report!`：先让底层总线处理（默认 no-op），再回传一条 Report trace。
    fn report(&mut self, kind: u32, args: &[ReportArg<'_>]) -> Result<(), BusError> {
        self.common.inner.report(kind, args)?;
        self.common.send_report(kind, args)
    }
}

impl<B: Bus, L: LinkTx, const BUF: usize> Bus for ReportOnlyBus<B, L, BUF> {
    fn set_bus_kind(&mut self, kind: rseq_vm::BusKind, arg: u32) -> Result<(), BusError> {
        self.common.inner.set_bus_kind(kind, arg)
    }

    fn read(&mut self, addr: u32, data: &mut [u8]) -> Result<(), BusError> {
        self.common.inner.read(addr, data)
    }

    fn write(&mut self, addr: u32, data: &[u8]) -> Result<(), BusError> {
        self.common.inner.write(addr, data)
    }

    fn delay_us(&mut self, us: u32) -> Result<(), BusError> {
        self.common.inner.delay_us(us)
    }

    fn log(&mut self, msg: &str) -> Result<(), BusError> {
        self.common.inner.log(msg)
    }

    fn wait_irq(&mut self, pin: u8, timeout_ms: u32) -> Result<(), BusError> {
        self.common.inner.wait_irq(pin, timeout_ms)
    }

    fn report(&mut self, kind: u32, args: &[ReportArg<'_>]) -> Result<(), BusError> {
        self.common.inner.report(kind, args)?;
        self.common.send_report(kind, args)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{
        MAX_TRACE_PAYLOAD, TRACE_OP_DELAY, TRACE_OP_READ, TRACE_OP_REPORT, TRACE_OP_REPORT_V2,
        TRACE_OP_WRITE, decode_trace,
    };
    use std::prelude::v1::*;
    use std::sync::{Arc, Mutex};

    /// 记录所有发送字节的全捕获 LinkTx。
    #[derive(Default)]
    struct CaptureTx(Arc<Mutex<Vec<u8>>>);

    impl LinkTx for CaptureTx {
        fn write(&mut self, data: &[u8]) -> Result<(), LinkError> {
            self.0.lock().unwrap().extend(data);
            Ok(())
        }
    }

    /// 恒返回 0 的总线,便于断言读出的 data。
    struct ZeroBus;
    impl Bus for ZeroBus {
        fn set_bus_kind(&mut self, _kind: rseq_vm::BusKind, _arg: u32) -> Result<(), BusError> {
            Ok(())
        }

        fn read(&mut self, _addr: u32, data: &mut [u8]) -> Result<(), BusError> {
            data.fill(0);
            Ok(())
        }
        fn write(&mut self, _addr: u32, _data: &[u8]) -> Result<(), BusError> {
            Ok(())
        }
        fn delay_us(&mut self, _us: u32) -> Result<(), BusError> {
            Ok(())
        }
    }

    /// 解码后的 Trace 的 owned 镜像——数据从帧缓冲拷出,避免借用逃逸出闭包。
    #[derive(Debug, PartialEq, Eq)]
    enum OwnedTrace {
        BusSelect {
            kind: rseq_vm::BusKind,
            arg: u32,
        },
        Read {
            addr: u32,
            data: Vec<u8>,
        },
        Write {
            addr: u32,
            data: Vec<u8>,
        },
        Delay {
            us: u32,
        },
        Log {
            msg: Vec<u8>,
        },
        Irq {
            pin: u8,
        },
        Report {
            meta: Option<crate::wire::ReportMeta>,
            kind: u32,
            args: Vec<OwnedReportArg>,
        },
    }

    #[derive(Debug, PartialEq, Eq)]
    enum OwnedReportArg {
        U32(u32),
        Bytes(Vec<u8>),
    }

    fn decoded_traces(bytes: &[u8]) -> Vec<OwnedTrace> {
        let mut dec = crate::frame::FrameDecoder::<{ crate::frame::HOST_FRAME_BUF }>::new();
        let mut out: Vec<OwnedTrace> = Vec::new();
        dec.feed(bytes, |_ty, p| {
            if let Some(r) = decode_trace(p) {
                out.push(match r {
                    crate::wire::TraceRef::Read { addr, data } => OwnedTrace::Read {
                        addr,
                        data: data.to_vec(),
                    },
                    crate::wire::TraceRef::Write { addr, data } => OwnedTrace::Write {
                        addr,
                        data: data.to_vec(),
                    },
                    crate::wire::TraceRef::Delay { us } => OwnedTrace::Delay { us },
                    crate::wire::TraceRef::Log { msg } => OwnedTrace::Log { msg: msg.to_vec() },
                    crate::wire::TraceRef::Irq { pin } => OwnedTrace::Irq { pin },
                    crate::wire::TraceRef::BusSelect { kind, arg } => {
                        OwnedTrace::BusSelect { kind, arg }
                    }
                    crate::wire::TraceRef::Report { meta, kind, args } => OwnedTrace::Report {
                        meta,
                        kind,
                        args: args
                            .as_slice()
                            .iter()
                            .map(|arg| match arg {
                                ReportArgRef::U32(v) => OwnedReportArg::U32(*v),
                                ReportArgRef::Bytes(bytes) => OwnedReportArg::Bytes(bytes.to_vec()),
                            })
                            .collect(),
                    },
                });
            }
        });
        out
    }

    #[test]
    fn emits_read_write_delay_traces() {
        let cap = Arc::new(Mutex::new(Vec::new()));
        let mut bus = TracingBus::new(ZeroBus, CaptureTx(cap.clone()));
        let mut buf = [0u8; 3];
        bus.read(0x10, &mut buf).unwrap();
        bus.write(0x20, &[0xaa, 0xbb]).unwrap();
        bus.delay_us(123).unwrap();

        let captured = cap.lock().unwrap().clone();
        let traces = decoded_traces(&captured);
        assert_eq!(traces.len(), 3);
        assert_eq!(
            traces[0],
            OwnedTrace::Read {
                addr: 0x10,
                data: vec![0, 0, 0]
            }
        );
        assert_eq!(
            traces[1],
            OwnedTrace::Write {
                addr: 0x20,
                data: vec![0xaa, 0xbb]
            }
        );
        assert_eq!(traces[2], OwnedTrace::Delay { us: 123 });
    }

    #[test]
    fn tx_failure_does_not_break_bus_op() {
        struct FailTx;
        impl LinkTx for FailTx {
            fn write(&mut self, _data: &[u8]) -> Result<(), LinkError> {
                Err(LinkError::Io)
            }
        }
        let mut bus = TracingBus::new(ZeroBus, FailTx);
        // TX 失败,但总线操作应仍然成功。
        assert!(bus.write(0x30, &[0x01]).is_ok());
        assert!(bus.delay_us(1).is_ok());
    }

    #[test]
    fn op_constants_match_wire_decode() {
        assert_eq!(TRACE_OP_READ, 0x01);
        assert_eq!(TRACE_OP_WRITE, 0x02);
        assert_eq!(TRACE_OP_DELAY, 0x03);
        let _ = MAX_TRACE_PAYLOAD;
    }

    #[test]
    fn tracing_bus_set_bus_emits_bus_trace() {
        let cap = Arc::new(Mutex::new(Vec::new()));
        let mut bus = TracingBus::new(ZeroBus, CaptureTx(cap.clone()));
        bus.set_bus_kind(rseq_vm::BusKind::I2c, 0x6a).unwrap();

        let captured = cap.lock().unwrap().clone();
        let traces = decoded_traces(&captured);
        assert_eq!(
            traces,
            vec![OwnedTrace::BusSelect {
                kind: rseq_vm::BusKind::I2c,
                arg: 0x6a
            }]
        );
    }

    #[test]
    fn tracing_bus_wait_emits_irq_trace() {
        // ZeroBus.wait_irq 用默认 no-op（立即 Ok），TracingBus 应在其后
        // 回传一条 Irq trace（pin=0）。
        let cap = Arc::new(Mutex::new(Vec::new()));
        let mut bus = TracingBus::new(ZeroBus, CaptureTx(cap.clone()));
        bus.wait_irq(0, 1000).unwrap();

        let captured = cap.lock().unwrap().clone();
        let traces = decoded_traces(&captured);
        assert_eq!(traces, vec![OwnedTrace::Irq { pin: 0 }]);
    }

    #[test]
    fn tracing_bus_report_emits_report_trace() {
        let cap = Arc::new(Mutex::new(Vec::new()));
        let mut bus = TracingBus::new(ZeroBus, CaptureTx(cap.clone()));
        bus.report(0x10, &[ReportArg::U32(42), ReportArg::Bytes(&[0xde, 0xad])])
            .unwrap();

        let captured = cap.lock().unwrap().clone();
        let traces = decoded_traces(&captured);
        assert_eq!(traces.len(), 1);
        let OwnedTrace::Report { meta, kind, args } = &traces[0] else {
            panic!("expected report trace");
        };
        let meta = meta.expect("report v2 meta");
        assert_eq!(meta.flags, 0);
        assert_eq!(meta.timestamp_us, 0);
        assert_eq!(*kind, 0x10);
        assert_eq!(
            args,
            &vec![
                OwnedReportArg::U32(42),
                OwnedReportArg::Bytes(vec![0xde, 0xad]),
            ]
        );
    }

    #[test]
    fn report_only_bus_suppresses_non_report_traces() {
        let cap = Arc::new(Mutex::new(Vec::new()));
        let mut bus = ReportOnlyBus::new_with_clock(ZeroBus, CaptureTx(cap.clone()), || 123_456);

        bus.read(0x10, &mut [0u8; 2]).unwrap();
        bus.write(0x11, &[1, 2]).unwrap();
        bus.delay_us(42).unwrap();
        bus.log("hidden").unwrap();
        bus.report(0x22, &[ReportArg::U32(7)]).unwrap();

        let captured = cap.lock().unwrap().clone();
        let traces = decoded_traces(&captured);
        assert_eq!(traces.len(), 1);
        let OwnedTrace::Report { meta, kind, args } = &traces[0] else {
            panic!("expected report trace");
        };
        let meta = meta.expect("report v2 meta");
        assert_eq!(meta.timestamp_us, 123_456);
        assert_eq!(*kind, 0x22);
        assert_eq!(args, &vec![OwnedReportArg::U32(7)]);
    }

    #[test]
    fn report_op_constant_matches_wire_decode() {
        assert_eq!(TRACE_OP_REPORT, 0x06);
        assert_eq!(TRACE_OP_REPORT_V2, 0x07);
        assert_eq!(crate::wire::TRACE_OP_BUS_SELECT, 0x08);
    }
}
