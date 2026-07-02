//! 总线操作观测记录。
//!
//! [`BusOp`] 描述 MCU 在执行字节码时实际发生的一条总线操作,是主机侧
//! 多处共享的统一类型:
//! - 主机端 `MockBus`(位于 `rseq-cli`)直接把 `Bus` 调用记录为 `BusOp`;
//! - `rseq::link::HostLink` 把 MCU 回传的 Trace 帧解码成 `BusOp`;
//! - CLI 把 `&[BusOp]` 渲染成可读日志。
//!
//! 三处共用同一类型,便于回环对比(MockBus 记录 ↔ 链路解码)。

/// 一条已执行的总线操作。
///
/// - `Read`/`Write` 的 `data` 为读出/写入的字节序列;
/// - `Delay` 的 `us` 为延时微秒数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BusOp {
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
    /// `print!("msg")` 日志。
    Log {
        msg: String,
    },
    /// `wait!(pin)` 命中：一次中断等待结束（边沿到达）。
    Irq {
        pin: u8,
    },
}
