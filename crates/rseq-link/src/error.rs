//! 链路层错误类型,主机驱动与传输层共用。

/// 链路错误。MCU 侧通常不需要这个类型——它主要用于主机驱动与传输实现。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkError {
    /// 底层 IO 错误(串口读写失败、对端关闭等)。
    Io,
    /// 收到帧但 CRC 校验失败(已丢弃,继续找下一帧 sync)。
    Crc,
    /// 等待 Ack / Result 超时。
    Timeout,
    /// MCU 拒绝了命令(附带其状态码)。
    Nak(u8),
    /// 帧超过本端缓冲容量。
    TooLarge,
    /// 未知 frame type。
    UnknownType,
    /// 对端已关闭(EOF)。
    Closed,
}

impl core::fmt::Display for LinkError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Io => f.write_str("link IO error"),
            Self::Crc => f.write_str("frame CRC mismatch"),
            Self::Timeout => f.write_str("link timeout"),
            Self::Nak(status) => write!(f, "MCU rejected command (status {status})"),
            Self::TooLarge => f.write_str("frame too large for buffer"),
            Self::UnknownType => f.write_str("unknown frame type"),
            Self::Closed => f.write_str("link closed"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for LinkError {}
