#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusError {
    InvalidAddress,
    AccessSizeMismatch,
    Timeout,
    HardwareFailure,
}

/// 总线操作 trait，MCU 侧需要实现这个 trait
pub trait Bus {
    /// 从总线读取 n 个字节
    fn read(&mut self, addr: u32, data: &mut [u8]) -> Result<(), BusError>;

    /// 向总线写入 n 个字节
    fn write(&mut self, addr: u32, data: &[u8]) -> Result<(), BusError>;

    /// 延迟微秒
    fn delay_us(&mut self, us: u32) -> Result<(), BusError>;
}
