use crate::bus::{Bus, BusError};
use crate::opcode::Opcode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmError {
    InvalidOpcode,
    BusError(BusError),
    ProgramTooShort,
    InvalidLength,
    DivideByZero,
}

/// 通用寄存器数量。寄存器以 u8 索引，故最多 256 个。
pub const REG_COUNT: usize = 256;

pub struct Vm<'a, B: Bus> {
    bus: &'a mut B,
    pc: usize,
    program: &'a [u8],
    /// 通用寄存器文件，供算术/逻辑指令使用。
    regs: [u32; REG_COUNT],
}

impl<'a, B: Bus> Vm<'a, B> {
    pub fn new(bus: &'a mut B, program: &'a [u8]) -> Self {
        Self {
            bus,
            pc: 0,
            program,
            regs: [0; REG_COUNT],
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
                Some(Opcode::ReadVar) => {
                    let addr = self.read_u32()?;
                    let len = self.read_u32()?;
                    let delay = self.read_u32()?;
                    let dst = self.read_u8()? as usize;

                    if len == 0 || len > 4 {
                        return Err(VmError::InvalidLength);
                    }
                    let mut buffer = [0u8; 4];
                    let data = &mut buffer[..len as usize];
                    self.bus.read(addr, data).map_err(VmError::BusError)?;

                    let mut val = 0u32;
                    for (i, &b) in data.iter().enumerate() {
                        val |= (b as u32) << (8 * i);
                    }
                    self.regs[dst] = val;

                    if delay > 0 {
                        self.bus.delay_us(delay).map_err(VmError::BusError)?;
                    }
                }
                Some(Opcode::LoadConst) => {
                    let dst = self.read_u8()? as usize;
                    let imm = self.read_u32()?;
                    self.regs[dst] = imm;
                }
                Some(Opcode::Move) => {
                    let dst = self.read_u8()? as usize;
                    let src = self.read_u8()? as usize;
                    self.regs[dst] = self.regs[src];
                }
                Some(
                    op @ (Opcode::Add
                    | Opcode::Sub
                    | Opcode::Mul
                    | Opcode::Div
                    | Opcode::Mod
                    | Opcode::Shl
                    | Opcode::Shr
                    | Opcode::And
                    | Opcode::Or
                    | Opcode::Xor),
                ) => {
                    let dst = self.read_u8()? as usize;
                    let lhs = self.regs[self.read_u8()? as usize];
                    let rhs = self.regs[self.read_u8()? as usize];
                    let result = match op {
                        Opcode::Add => lhs.wrapping_add(rhs),
                        Opcode::Sub => lhs.wrapping_sub(rhs),
                        Opcode::Mul => lhs.wrapping_mul(rhs),
                        Opcode::Div => {
                            if rhs == 0 {
                                return Err(VmError::DivideByZero);
                            }
                            lhs / rhs
                        }
                        Opcode::Mod => {
                            if rhs == 0 {
                                return Err(VmError::DivideByZero);
                            }
                            lhs % rhs
                        }
                        // 移位量取 rhs 低位，避免 >= 32 时 panic。
                        Opcode::Shl => lhs.wrapping_shl(rhs),
                        Opcode::Shr => lhs.wrapping_shr(rhs),
                        Opcode::And => lhs & rhs,
                        Opcode::Or => lhs | rhs,
                        Opcode::Xor => lhs ^ rhs,
                        // 上面的 match 已覆盖全部进入此分支的 opcode。
                        _ => unreachable!(),
                    };
                    self.regs[dst] = result;
                }
                Some(Opcode::Return) => {
                    return Ok(());
                }
                // WriteVar / UpdateVar 尚未在 VM 中实现。
                Some(Opcode::WriteVar | Opcode::UpdateVar) => {
                    return Err(VmError::InvalidOpcode);
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
    use crate::bus::{Bus, BusError};

    /// 简易 mock bus，仅用于 VM 单元测试。
    struct TestBus {
        mem: [u8; 256],
    }

    impl TestBus {
        fn new() -> Self {
            Self { mem: [0; 256] }
        }
    }

    impl Bus for TestBus {
        fn read(&mut self, addr: u32, data: &mut [u8]) -> Result<(), BusError> {
            for (i, slot) in data.iter_mut().enumerate() {
                *slot = self.mem[(addr as usize + i) % 256];
            }
            Ok(())
        }
        fn write(&mut self, addr: u32, data: &[u8]) -> Result<(), BusError> {
            for (i, &b) in data.iter().enumerate() {
                self.mem[(addr as usize + i) % 256] = b;
            }
            Ok(())
        }
        fn delay_us(&mut self, _us: u32) -> Result<(), BusError> {
            Ok(())
        }
    }

    #[test]
    fn read_var_stores_le_value_into_register() {
        // 程序: ReadVar addr=0x10 len=2 delay=0 dst=0 ; Return
        let program = [
            Opcode::ReadVar as u8,
            0x10, 0x00, 0x00, 0x00, // addr
            0x02, 0x00, 0x00, 0x00, // len
            0x00, 0x00, 0x00, 0x00, // delay
            0x00,                   // dst reg
            Opcode::Return as u8,
        ];
        let mut bus = TestBus::new();
        bus.mem[0x10] = 0x34;
        bus.mem[0x11] = 0x12;

        let mut vm = Vm::new(&mut bus, &program);
        vm.run().unwrap();
        assert_eq!(vm.regs[0], 0x1234);
    }

    #[test]
    fn load_const_and_add_execute_correctly() {
        // 程序: LoadConst r0=10 ; LoadConst r1=20 ; Add r2=r0+r1 ; Return
        let program = [
            Opcode::LoadConst as u8, 0x00, 0x0A, 0x00, 0x00, 0x00,
            Opcode::LoadConst as u8, 0x01, 0x14, 0x00, 0x00, 0x00,
            Opcode::Add as u8, 0x02, 0x00, 0x01,
            Opcode::Return as u8,
        ];
        let mut bus = TestBus::new();
        let mut vm = Vm::new(&mut bus, &program);
        vm.run().unwrap();
        assert_eq!(vm.regs[2], 30);
    }

    #[test]
    fn shift_and_logic_opcodes_execute() {
        // LoadConst r0=0xF0 ; LoadConst r1=4 ; Shl r2=r0<<r1 ; And r3=r2&r0 ; Return
        let program = [
            Opcode::LoadConst as u8, 0x00, 0xF0, 0x00, 0x00, 0x00,
            Opcode::LoadConst as u8, 0x01, 0x04, 0x00, 0x00, 0x00,
            Opcode::Shl as u8, 0x02, 0x00, 0x01,
            Opcode::And as u8, 0x03, 0x02, 0x00,
            Opcode::Return as u8,
        ];
        let mut bus = TestBus::new();
        let mut vm = Vm::new(&mut bus, &program);
        vm.run().unwrap();
        assert_eq!(vm.regs[2], 0xF0 << 4);
        assert_eq!(vm.regs[3], (0xF0 << 4) & 0xF0);
    }

    #[test]
    fn divide_by_zero_returns_error() {
        let program = [
            Opcode::LoadConst as u8, 0x00, 0x0A, 0x00, 0x00, 0x00,
            Opcode::LoadConst as u8, 0x01, 0x00, 0x00, 0x00, 0x00,
            Opcode::Div as u8, 0x02, 0x00, 0x01,
            Opcode::Return as u8,
        ];
        let mut bus = TestBus::new();
        let mut vm = Vm::new(&mut bus, &program);
        assert_eq!(vm.run(), Err(VmError::DivideByZero));
    }

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
