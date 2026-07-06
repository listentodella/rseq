//! 总线操作观测记录。
//!
//! [`BusOp`] 描述 MCU 在执行字节码时实际发生的一条总线操作,是主机侧
//! 多处共享的统一类型:
//! - 主机端 `MockBus`(位于 `rseq-cli`)直接把 `Bus` 调用记录为 `BusOp`;
//! - `rseq::link::HostLink` 把 MCU 回传的 Trace 帧解码成 `BusOp`;
//! - CLI 把 `&[BusOp]` 渲染成可读日志。
//!
//! 三处共用同一类型,便于回环对比(MockBus 记录 ↔ 链路解码)。

/// `report!` 的一个已解码参数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReportArg {
    U32(u32),
    Bytes(Vec<u8>),
}

/// `report!` 上报帧的链路元信息。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReportMeta {
    pub flags: u8,
    pub frame_id: u32,
    pub timestamp_us: u64,
}

impl ReportMeta {
    pub const fn timestamp_valid(&self) -> bool {
        self.flags & rseq_link::wire::REPORT_FLAG_TIMESTAMP_VALID != 0
    }
}

/// 一条已执行的总线操作。
///
/// - `Read`/`Write` 的 `data` 为读出/写入的字节序列;
/// - `Delay` 的 `us` 为延时微秒数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BusOp {
    /// `bus!(...)` 选择后续读写所使用的物理总线。
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
    /// `print!("msg")` 日志。
    Log {
        msg: String,
    },
    /// `wait!(pin)` 命中：一次中断等待结束（边沿到达）。
    Irq {
        pin: u8,
    },
    /// `report!(kind, ...)` 结构化数据上报。
    Report {
        meta: Option<ReportMeta>,
        kind: u32,
        args: Vec<ReportArg>,
    },
}
