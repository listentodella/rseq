
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

                    // TODO: 存储读取结果（当前 VM 版本主要是写入操作）
                    // 这里先执行读取和延迟
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
