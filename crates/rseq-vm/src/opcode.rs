
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opcode {
    Read = 0x01,
    Write = 0x02,
    Update = 0x03,
    Return = 0xFF,
}

impl Opcode {
    pub fn from_u8(n: u8) -> Option<Self> {
        match n {
            0x01 => Some(Self::Read),
            0x02 => Some(Self::Write),
            0x03 => Some(Self::Update),
            0xFF => Some(Self::Return),
            _ => None,
        }
    }
}
