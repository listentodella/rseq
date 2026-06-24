
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opcode {
    Read = 0x01,
    Write = 0x02,
    Return = 0xFF,
}

impl Opcode {
    pub fn from_u8(n: u8) -> Option<Self> {
        match n {
            0x01 => Some(Self::Read),
            0x02 => Some(Self::Write),
            0xFF => Some(Self::Return),
            _ => None,
        }
    }
}
