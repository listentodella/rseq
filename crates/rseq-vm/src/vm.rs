
use crate::bus::{Bus, BusError};
use crate::opcode::Opcode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmError {
    InvalidOpcode,
    BusError(BusError),
    ProgramTooShort,
    InvalidLength,
}

pub struct Vm<'a, B: Bus> {
    bus: &'a mut B,
    pc: usize,
    program: &'a [u8],
}

impl<'a, B: Bus> Vm<'a, B> {
    pub fn new(bus: &'a mut B, program: &'a [u8]) -> Self {
        Self {
            bus,
            pc: 0,
            program,
        }
    }

    fn read_u8(&mut self) -> Result<u8, VmError> {
        if self.pc >= self.program.len() {
            return Err(VmError::ProgramTooShort);
        }
        let byte = self.program[self.pc];
        self.pc += 1;
        Ok(byte)
    }

    fn read_u32(&mut self) -> Result<u32, VmError> {
        if self.pc + 4 > self.program.len() {
            return Err(VmError::ProgramTooShort);
        }
        let bytes = [
            self.program[self.pc],
            self.program[self.pc + 1],
            self.program[self.pc + 2],
            self.program[self.pc + 3],
        ];
        self.pc += 4;
        Ok(u32::from_le_bytes(bytes))
    }

    fn read_len(&mut self) -> Result<usize, VmError> {
        let len = self.read_u32()?;
        if len == 0 || len > 4096 {
            return Err(VmError::InvalidLength);
        }
        Ok(len as usize)
    }

    pub fn run(&mut self) -> Result<(), VmError> {
        loop {
            if self.pc >= self.program.len() {
                return Err(VmError::ProgramTooShort);
            }
            let opcode_byte = self.program[self.pc];
            self.pc += 1;

            match Opcode::from_u8(opcode_byte) {
                Some(Opcode::Read) => {
                    let addr = self.read_u32()?;
                    let len = self.read_len()?;
                    let delay = self.read_u32()?;

                    let mut buffer = [0u8; 4096];
                    let data = &mut buffer[..len];
                    self.bus.read(addr, data).map_err(VmError::BusError)?;

                    if delay > 0 {
                        self.bus.delay_us(delay).map_err(VmError::BusError)?;
                    }
                }
                Some(Opcode::Write) => {
                    let addr = self.read_u32()?;
                    let len = self.read_len()?;
                    let delay = self.read_u32()?;

                    if self.pc + len > self.program.len() {
                        return Err(VmError::ProgramTooShort);
                    }
                    let data = &self.program[self.pc..self.pc + len];
                    self.pc += len;

                    self.bus.write(addr, data).map_err(VmError::BusError)?;

                    if delay > 0 {
                        self.bus.delay_us(delay).map_err(VmError::BusError)?;
                    }
                }
                Some(Opcode::Update) => {
                    let addr = self.read_u32()?;
                    let width = self.read_len()?;
                    let delay = self.read_u32()?;
                    let field_count = self.read_u32()? as usize;

                    let mut buffer = [0u8; 4096];
                    let data = &mut buffer[..width];
                    self.bus.read(addr, data).map_err(VmError::BusError)?;

                    let mut reg_val = bytes_to_u64_le(data);
                    for _ in 0..field_count {
                        let bit_lo = self.read_u8()?;
                        let bit_hi = self.read_u8()?;
                        let value = self.read_u32()?;
                        reg_val = merge_field(reg_val, bit_lo, bit_hi, value);
                    }
                    u64_to_bytes_le(reg_val, data);

                    self.bus.write(addr, data).map_err(VmError::BusError)?;

                    if delay > 0 {
                        self.bus.delay_us(delay).map_err(VmError::BusError)?;
                    }
                }
                Some(Opcode::Return) => {
                    return Ok(());
                }
                None => {
                    return Err(VmError::InvalidOpcode);
                }
            }
        }
    }
}

fn bytes_to_u64_le(bytes: &[u8]) -> u64 {
    let mut val = 0u64;
    for (i, &b) in bytes.iter().enumerate() {
        val |= (b as u64) << (8 * i);
    }
    val
}

fn u64_to_bytes_le(val: u64, bytes: &mut [u8]) {
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = (val >> (8 * i)) as u8;
    }
}

fn merge_field(reg_val: u64, bit_lo: u8, bit_hi: u8, value: u32) -> u64 {
    let width = (bit_hi - bit_lo + 1) as u32;
    let mask = if width >= 64 {
        u64::MAX
    } else {
        ((1u64 << width) - 1) << bit_lo
    };
    let field_val = (value as u64) & ((1u64 << width) - 1);
    (reg_val & !mask) | (field_val << bit_lo)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_field_preserves_other_bits() {
        let old = 0x2Au64;
        // bit 0: cs_pu_dis
        let new = merge_field(old, 0, 0, 1);
        assert_eq!(new, 0x2B);
        // bit 1: sda_scl_pu_dis = 0
        let new = merge_field(old, 1, 1, 0);
        assert_eq!(new, 0x28);
    }

    #[test]
    fn merge_multi_bit_field() {
        let old = 0x00u64;
        // bits 4:3 = 3
        let new = merge_field(old, 3, 4, 3);
        assert_eq!(new, 0x18);
    }
}
