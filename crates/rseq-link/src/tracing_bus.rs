//! 把真实目标总线包一层,在每次总线操作后向主机回传 Trace 帧。
//!
//! MCU 端 = 真实 I2C/SPI `Bus` 实现 + `TracingBus` + `LinkTx`(UART)。
//! TX 失败时尽量不影响总线操作本身(观测是尽力而为)。

use rseq_vm::{Bus, BusError};

use crate::error::LinkError;
use crate::frame::MAX_TRACE_FRAME;
use crate::wire::{encode_trace_delay, encode_trace_log, encode_trace_rw};

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

/// 包裹真实总线 `B`,在每次 `read`/`write`/`delay_us` 后经 `L` 回传一条 Trace 帧。
///
/// `BUF` 为内部帧缓冲大小,默认 [`MAX_TRACE_FRAME`]——可容纳 VM 最大 4096 字节
/// 读写产生的 Trace 帧。若 MCU RAM 紧张且已知读写更短,可显式指定更小的 `BUF`。
pub struct TracingBus<B, L, const BUF: usize = MAX_TRACE_FRAME> {
    pub inner: B,
    tx: L,
    buf: [u8; BUF],
}

/// 默认缓冲(`MAX_TRACE_FRAME`)的构造入口——多数场景直接用这个,
/// 调用方无需指定 const 参数。
impl<B, L: LinkTx> TracingBus<B, L, MAX_TRACE_FRAME> {
    pub fn new(inner: B, tx: L) -> Self {
        Self {
            inner,
            tx,
            buf: [0; MAX_TRACE_FRAME],
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
            inner,
            tx,
            buf: [0; BUF],
        }
    }

    /// 取得内部真实总线的可变引用(少数需要直接访问硬件的场景)。
    pub fn inner_mut(&mut self) -> &mut B {
        &mut self.inner
    }

    /// 拆出内部总线与链路出口(销毁 `TracingBus`)。
    /// MCU 端每次 EXEC 后用此回收总线对象,并释放对 transport 的借用。
    pub fn into_inner(self) -> (B, L) {
        (self.inner, self.tx)
    }

    fn send(&mut self, frame_len: usize) {
        // 观测失败不应中断寄存器操作,只尽力发送。
        let _ = self.tx.write(&self.buf[..frame_len]);
    }
}

impl<B: Bus, L: LinkTx, const BUF: usize> Bus for TracingBus<B, L, BUF> {
    fn read(&mut self, addr: u32, data: &mut [u8]) -> Result<(), BusError> {
        self.inner.read(addr, data)?;
        // VM 限制 read len ≤ 4096,故 data.len() ≤ MAX_TRACE_PAYLOAD 的 data 部分。
        let n = encode_trace_rw(
            &mut self.buf,
            crate::wire::TRACE_OP_READ,
            addr,
            data,
        );
        self.send(n);
        Ok(())
    }

    fn write(&mut self, addr: u32, data: &[u8]) -> Result<(), BusError> {
        self.inner.write(addr, data)?;
        let n = encode_trace_rw(
            &mut self.buf,
            crate::wire::TRACE_OP_WRITE,
            addr,
            data,
        );
        self.send(n);
        Ok(())
    }

    fn delay_us(&mut self, us: u32) -> Result<(), BusError> {
        self.inner.delay_us(us)?;
        let n = encode_trace_delay(&mut self.buf, us);
        self.send(n);
        Ok(())
    }

    /// `print!`：先让真总线打印（真机=printk），再回传一条 Log trace 给主机。
    /// 观测失败不影响总线动作本身（尽力发送）。
    fn log(&mut self, msg: &str) -> Result<(), BusError> {
        self.inner.log(msg)?;
        let n = encode_trace_log(&mut self.buf, msg);
        self.send(n);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::prelude::v1::*;
    use crate::wire::{MAX_TRACE_PAYLOAD, TRACE_OP_DELAY, TRACE_OP_READ, TRACE_OP_WRITE, decode_trace};
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
        Read { addr: u32, data: Vec<u8> },
        Write { addr: u32, data: Vec<u8> },
        Delay { us: u32 },
        Log { msg: Vec<u8> },
    }

    fn decoded_traces(bytes: &[u8]) -> Vec<OwnedTrace> {
        let mut dec =
            crate::frame::FrameDecoder::<{ crate::frame::HOST_FRAME_BUF }>::new();
        let mut out: Vec<OwnedTrace> = Vec::new();
        dec.feed(bytes, |_ty, p| {
            if let Some(r) = decode_trace(p) {
                out.push(match r {
                    crate::wire::TraceRef::Read { addr, data } => {
                        OwnedTrace::Read { addr, data: data.to_vec() }
                    }
                    crate::wire::TraceRef::Write { addr, data } => {
                        OwnedTrace::Write { addr, data: data.to_vec() }
                    }
                    crate::wire::TraceRef::Delay { us } => OwnedTrace::Delay { us },
                    crate::wire::TraceRef::Log { msg } => {
                        OwnedTrace::Log { msg: msg.to_vec() }
                    }
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
            OwnedTrace::Read { addr: 0x10, data: vec![0, 0, 0] }
        );
        assert_eq!(
            traces[1],
            OwnedTrace::Write { addr: 0x20, data: vec![0xaa, 0xbb] }
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
}
