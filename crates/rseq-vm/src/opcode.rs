#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opcode {
    LoadConst = 0x10,
    Move = 0x11,
    Add = 0x12,
    Sub = 0x13,
    Mul = 0x14,
    Div = 0x15,
    Mod = 0x16,
    Shl = 0x17,
    Shr = 0x18,
    And = 0x19,
    Or = 0x1A,
    Xor = 0x1B,
    // ── 比较（dst = lhs OP rhs ? 1 : 0；Lt/Le/Gt/Ge 按有符号 i32）──
    CmpEq = 0x1C,
    CmpNe = 0x1D,
    CmpLt = 0x1E,
    CmpLe = 0x1F,
    CmpGt = 0x20,
    CmpGe = 0x21,
    // ── 逻辑（dst = (lhs!=0) OP (rhs!=0) ? 1 : 0；急求值非短路）──
    LogAnd = 0x22,
    LogOr = 0x23,
    // ── 逻辑非（dst = (src==0) ? 1 : 0）──
    LogNot = 0x24,
    // ── 跳转（off 为 i32，相对读完 off 后的 pc；前向为正）──
    JumpIfZero = 0x25,
    Jump = 0x26,
    /// `print!("msg")`：读 len(u32) + utf8 字节，调 `Bus::log`。不涉总线时序，
    /// 由 `TracingBus` 回传 Log trace、MCU IMU 总线走 printk。
    Log = 0x27,
    /// `print!("fmt", v1, v2, ...)`：读 n_vars + 各寄存器索引 + fmt 字符串，
    /// 调 `Bus::log_vars`（默认实现就地格式化 `{}`/`{x}` 后委托 `log`）。
    LogVar = 0x28,
    /// `wait!(pin)`：读 `pin:u8` + `timeout_ms:u32`，调 `Bus::wait_irq`。
    /// MCU 侧阻塞至该中断引脚发生边沿（或超时返回 `BusError::Timeout`）；
    /// 主机/模拟侧默认 no-op 放行，由内联派发序列（紧跟本指令之后的
    /// `ReadVar` 快照 + 按掩码 `And`/`JumpIfZero` 分支）判多状态。
    WaitIrq = 0x29,
    /// `report!(kind, ...)`：读 `kind:u32` + typed args，调 `Bus::report`
    /// 上报一条二进制事件。用于 MCU→Host 的结构化数据出口。
    Report = 0x2A,
    /// `bus!(spi|i2c|i3c[, arg])`：读 `kind:u8` + `arg:u32`，调
    /// `Bus::set_bus_kind` 选择后续寄存器读写的物理总线。
    SetBus = 0x2B,
    /// `bus_probe!(kind, { ... })`：读候选 bus args 和一个寄存器期望值，
    /// 逐个尝试 `set_bus_kind + read`，首个匹配者成为后续读写的 active bus。
    ProbeBus = 0x2C,
    Read = 0x01,
    Write = 0x02,
    Update = 0x03,
    WriteVar = 0x04,
    UpdateVar = 0x05,
    ReadVar = 0x06,
    /// `repeat!(N) { ... }` 定长循环：`count:u32 | body_len:u32 | body`，
    /// VM 计数回跳执行 body 共 count 次。不引入条件/跳转，仅做有界重复。
    Loop = 0x07,
    /// `read!(addr, len_reg)`：运行时从寄存器读取长度，读取结果丢弃。
    /// 用于 FIFO 这类必须先读长度、再按实际长度 drain 的顺序读寄存器。
    ReadDyn = 0x08,
    /// `let data = read!(addr, len_reg)`：运行时从寄存器读取长度，把原始字节
    /// 保存到 VM 的有界数据缓冲，供后续 `report!(..., data)` 上报。
    ReadBuf = 0x09,
    Return = 0xFF,
}

pub const REPORT_ARG_U32: u8 = 0x01;
pub const REPORT_ARG_BYTES: u8 = 0x02;

impl Opcode {
    pub fn from_u8(n: u8) -> Option<Self> {
        match n {
            0x10 => Some(Self::LoadConst),
            0x11 => Some(Self::Move),
            0x12 => Some(Self::Add),
            0x13 => Some(Self::Sub),
            0x14 => Some(Self::Mul),
            0x15 => Some(Self::Div),
            0x16 => Some(Self::Mod),
            0x17 => Some(Self::Shl),
            0x18 => Some(Self::Shr),
            0x19 => Some(Self::And),
            0x1A => Some(Self::Or),
            0x1B => Some(Self::Xor),
            0x1C => Some(Self::CmpEq),
            0x1D => Some(Self::CmpNe),
            0x1E => Some(Self::CmpLt),
            0x1F => Some(Self::CmpLe),
            0x20 => Some(Self::CmpGt),
            0x21 => Some(Self::CmpGe),
            0x22 => Some(Self::LogAnd),
            0x23 => Some(Self::LogOr),
            0x24 => Some(Self::LogNot),
            0x25 => Some(Self::JumpIfZero),
            0x26 => Some(Self::Jump),
            0x27 => Some(Self::Log),
            0x28 => Some(Self::LogVar),
            0x29 => Some(Self::WaitIrq),
            0x2A => Some(Self::Report),
            0x2B => Some(Self::SetBus),
            0x2C => Some(Self::ProbeBus),
            0x01 => Some(Self::Read),
            0x02 => Some(Self::Write),
            0x03 => Some(Self::Update),
            0x04 => Some(Self::WriteVar),
            0x05 => Some(Self::UpdateVar),
            0x06 => Some(Self::ReadVar),
            0x07 => Some(Self::Loop),
            0x08 => Some(Self::ReadDyn),
            0x09 => Some(Self::ReadBuf),
            0xFF => Some(Self::Return),
            _ => None,
        }
    }
}
