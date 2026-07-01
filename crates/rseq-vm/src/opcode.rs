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
    Read = 0x01,
    Write = 0x02,
    Update = 0x03,
    WriteVar = 0x04,
    UpdateVar = 0x05,
    ReadVar = 0x06,
    /// `repeat!(N) { ... }` 定长循环：`count:u32 | body_len:u32 | body`，
    /// VM 计数回跳执行 body 共 count 次。不引入条件/跳转，仅做有界重复。
    Loop = 0x07,
    Return = 0xFF,
}

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
            0x01 => Some(Self::Read),
            0x02 => Some(Self::Write),
            0x03 => Some(Self::Update),
            0x04 => Some(Self::WriteVar),
            0x05 => Some(Self::UpdateVar),
            0x06 => Some(Self::ReadVar),
            0x07 => Some(Self::Loop),
            0xFF => Some(Self::Return),
            _ => None,
        }
    }
}
