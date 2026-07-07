#![cfg_attr(not(feature = "std"), no_std)]

//! Register Sequence 传输层。
//!
//! 提供主机↔MCU 之间的二进制帧协议:长度前缀 + CRC32 + ACK,以及把
//! `rseq_vm::Bus` 的每次总线操作流式回传为 Trace 帧的 [`TracingBus`]。
//!
//! 核心无分配(no_std 默认):帧编解码、CRC32、Trace 载荷、`TracingBus`、
//! `Transport`/`LinkTx` trait。`std` feature 启用进程内回环管道
//! [`MockTransport`];`serial` feature 启用串口实现 [`SerialTransport`]。

// 单测在 no_std 构建下也需要 `std`(`Vec`/`vec!`/`Arc`/`Mutex`):
// `#[macro_use] extern crate std` 链接 std 并把 `vec!` 等宏引入 crate 域;
// 各测试模块再 `use std::prelude::v1::*` 引入 `Vec`/`String` 等类型。
#[cfg(test)]
#[macro_use]
extern crate std;

pub mod crc32;
pub mod error;
pub mod frame;
pub mod tracing_bus;
pub mod transport;
pub mod wire;

pub use error::LinkError;
pub use frame::{Frame, FrameDecoder, FrameType, HOST_FRAME_BUF, MAX_TRACE_FRAME};
pub use tracing_bus::{LinkTx, TracingBus};
pub use wire::{
    CONTROL_MAX_READ_LEN, CONTROL_MAX_WRITE_LEN, CONTROL_OP_BUS_READ, CONTROL_OP_BUS_WRITE,
    ControlRequestRef, ControlResultRef, ControlStatus, ExecStatus, LOAD_VERSION, LoadSegs,
    REPORT_ARG_BYTES, REPORT_ARG_U32, REPORT_FLAG_TIMESTAMP_VALID, ReportArgRef, ReportArgs,
    ReportMeta, SEG_KIND_IRQ_HANDLER, SEG_KIND_IRQ_TABLE, SEG_KIND_MAIN, TRACE_OP_DELAY,
    TRACE_OP_READ, TRACE_OP_REPORT, TRACE_OP_REPORT_V2, TRACE_OP_WRITE, TraceRef,
    decode_control_request, decode_control_result, encode_control_bus_read_into,
    encode_control_bus_read_result_into, encode_control_bus_write_into,
    encode_control_bus_write_result_into,
};

#[cfg(feature = "std")]
pub use transport::MockTransport;
pub use transport::Transport;
#[cfg(feature = "serial")]
pub use transport::{SerialPortInfo, SerialTransport};
