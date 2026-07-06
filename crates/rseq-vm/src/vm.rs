use crate::bus::{Bus, BusError, BusKind, ReportArg};
use crate::opcode::{Opcode, REPORT_ARG_BYTES, REPORT_ARG_U32};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmError {
    InvalidOpcode,
    BusError(BusError),
    ProgramTooShort,
    InvalidLength,
    DivideByZero,
}

/// 单步执行结果：`Continue` 继续下一条指令；`Returned` 命中 Return，应终止。
/// 供 `step()` 在 `Loop` 内递归时把"body 中出现 Return"向上传播。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Continue,
    Returned,
}

/// 通用寄存器数量。寄存器以 u8 索引，故最多 256 个。
pub const REG_COUNT: usize = 256;
/// `let data = read!(..., len)` 的原始数据缓冲数量。第一版只保留一个
/// 4096 字节 slot，避免 MCU 栈/内存预算失控。
pub const DATA_BUF_COUNT: usize = 1;
pub const DATA_BUF_LEN: usize = 4096;

pub struct Vm<'a, B: Bus> {
    bus: &'a mut B,
    pc: usize,
    program: &'a [u8],
    /// 通用寄存器文件，供算术/逻辑指令使用。
    regs: [u32; REG_COUNT],
    data_bufs: [[u8; DATA_BUF_LEN]; DATA_BUF_COUNT],
    data_lens: [usize; DATA_BUF_COUNT],
}

impl<'a, B: Bus> Vm<'a, B> {
    pub fn new(bus: &'a mut B, program: &'a [u8]) -> Self {
        Self {
            bus,
            pc: 0,
            program,
            regs: [0; REG_COUNT],
            data_bufs: [[0; DATA_BUF_LEN]; DATA_BUF_COUNT],
            data_lens: [0; DATA_BUF_COUNT],
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

    /// 读一个 i32 立即数（跳转偏移用）。复用 read_u32 的边界检查。
    fn read_i32(&mut self) -> Result<i32, VmError> {
        Ok(self.read_u32()? as i32)
    }

    fn read_len(&mut self) -> Result<usize, VmError> {
        let len = self.read_u32()?;
        if len == 0 || len > 4096 {
            return Err(VmError::InvalidLength);
        }
        Ok(len as usize)
    }

    /// 执行下一条指令，返回是否命中 Return。
    /// 抽成独立方法是为了让 `Loop` 能在 body 边界内递归调用单步执行。
    fn step(&mut self) -> Result<Step, VmError> {
        if self.pc >= self.program.len() {
            return Err(VmError::ProgramTooShort);
        }
        let opcode_byte = self.program[self.pc];
        self.pc += 1;

        match Opcode::from_u8(opcode_byte) {
            Some(Opcode::SetBus) => {
                let kind = BusKind::from_u8(self.read_u8()?).ok_or(VmError::InvalidOpcode)?;
                let arg = self.read_u32()?;
                self.bus
                    .set_bus_kind(kind, arg)
                    .map_err(VmError::BusError)?;
            }
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
            Some(Opcode::ReadDyn) => {
                let addr = self.read_u32()?;
                let delay = self.read_u32()?;
                let src = self.read_u8()? as usize;
                let len = self.regs[src] as usize;
                if len > 4096 {
                    return Err(VmError::InvalidLength);
                }

                if len != 0 {
                    let mut buffer = [0u8; 4096];
                    let data = &mut buffer[..len];
                    self.bus.read(addr, data).map_err(VmError::BusError)?;
                }

                if delay > 0 {
                    self.bus.delay_us(delay).map_err(VmError::BusError)?;
                }
            }
            Some(Opcode::ReadBuf) => {
                let addr = self.read_u32()?;
                let delay = self.read_u32()?;
                let len_src = self.read_u8()? as usize;
                let dst_buf = self.read_u8()? as usize;
                if dst_buf >= DATA_BUF_COUNT {
                    return Err(VmError::InvalidLength);
                }
                let len = self.regs[len_src] as usize;
                if len > DATA_BUF_LEN {
                    return Err(VmError::InvalidLength);
                }

                self.data_lens[dst_buf] = len;
                if len != 0 {
                    let data = &mut self.data_bufs[dst_buf][..len];
                    self.bus.read(addr, data).map_err(VmError::BusError)?;
                }

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
            // 比较：dst = lhs OP rhs ? 1 : 0。Lt/Le/Gt/Ge 按有符号 i32。
            Some(
                op @ (Opcode::CmpEq
                | Opcode::CmpNe
                | Opcode::CmpLt
                | Opcode::CmpLe
                | Opcode::CmpGt
                | Opcode::CmpGe),
            ) => {
                let dst = self.read_u8()? as usize;
                let lhs = self.regs[self.read_u8()? as usize];
                let rhs = self.regs[self.read_u8()? as usize];
                let (li, ri) = (lhs as i32, rhs as i32);
                let result: u32 = match op {
                    Opcode::CmpEq => (lhs == rhs) as u32,
                    Opcode::CmpNe => (lhs != rhs) as u32,
                    Opcode::CmpLt => (li < ri) as u32,
                    Opcode::CmpLe => (li <= ri) as u32,
                    Opcode::CmpGt => (li > ri) as u32,
                    Opcode::CmpGe => (li >= ri) as u32,
                    _ => unreachable!(),
                };
                self.regs[dst] = result;
            }
            // 逻辑与/或（急求值非短路）：dst = (lhs!=0) OP (rhs!=0) ? 1 : 0。
            Some(op @ (Opcode::LogAnd | Opcode::LogOr)) => {
                let dst = self.read_u8()? as usize;
                let lhs = self.regs[self.read_u8()? as usize];
                let rhs = self.regs[self.read_u8()? as usize];
                let result = match op {
                    Opcode::LogAnd => ((lhs != 0) && (rhs != 0)) as u32,
                    Opcode::LogOr => ((lhs != 0) || (rhs != 0)) as u32,
                    _ => unreachable!(),
                };
                self.regs[dst] = result;
            }
            // 逻辑非：dst = (src==0) ? 1 : 0。
            Some(Opcode::LogNot) => {
                let dst = self.read_u8()? as usize;
                let src = self.regs[self.read_u8()? as usize];
                self.regs[dst] = (src == 0) as u32;
            }
            // 条件跳转：cond==0 则 pc += off（off 相对读完 off 后的 pc）。
            Some(Opcode::JumpIfZero) => {
                let cond = self.regs[self.read_u8()? as usize];
                let off = self.read_i32()?;
                if cond == 0 {
                    self.pc = self
                        .pc
                        .checked_add_signed(off as isize)
                        .ok_or(VmError::InvalidLength)?;
                    if self.pc > self.program.len() {
                        return Err(VmError::InvalidLength);
                    }
                }
            }
            // 无条件跳转：pc += off。
            Some(Opcode::Jump) => {
                let off = self.read_i32()?;
                self.pc = self
                    .pc
                    .checked_add_signed(off as isize)
                    .ok_or(VmError::InvalidLength)?;
                if self.pc > self.program.len() {
                    return Err(VmError::InvalidLength);
                }
            }
            // print!("msg")：读 len(u32) + utf8 字节，调 bus.log。不涉总线时序。
            Some(Opcode::Log) => {
                let len = self.read_u32()? as usize;
                let end = self.pc.checked_add(len).ok_or(VmError::InvalidLength)?;
                if end > self.program.len() {
                    return Err(VmError::ProgramTooShort);
                }
                let bytes = &self.program[self.pc..end];
                self.pc = end;
                let msg = core::str::from_utf8(bytes).map_err(|_| VmError::InvalidOpcode)?;
                self.bus.log(msg).map_err(VmError::BusError)?;
            }
            // print!("fmt", v1, ...)：读 n_vars + 寄存器索引 + fmt，调 bus.log_vars。
            // n_vars 上限 8（栈数组），编译器强制。
            Some(Opcode::LogVar) => {
                let n = self.read_u8()? as usize;
                if n > 8 {
                    return Err(VmError::InvalidLength);
                }
                let mut reg_idx = [0u8; 8];
                for slot in reg_idx.iter_mut().take(n) {
                    *slot = self.read_u8()?;
                }
                let fmt_len = self.read_u32()? as usize;
                let end = self.pc.checked_add(fmt_len).ok_or(VmError::InvalidLength)?;
                if end > self.program.len() {
                    return Err(VmError::ProgramTooShort);
                }
                let fmt_bytes = &self.program[self.pc..end];
                self.pc = end;
                let fmt = core::str::from_utf8(fmt_bytes).map_err(|_| VmError::InvalidOpcode)?;
                let mut vals = [0u32; 8];
                for (i, slot) in reg_idx.iter().enumerate().take(n) {
                    vals[i] = self.regs[*slot as usize];
                }
                self.bus
                    .log_vars(fmt, &vals[..n])
                    .map_err(VmError::BusError)?;
            }
            // wait!(pin, timeout_ms)：阻塞至中断边沿或超时。超时由总线以
            // BusError::Timeout 返回，VM 透传为 VmError::BusError。
            Some(Opcode::WaitIrq) => {
                let pin = self.read_u8()?;
                let timeout_ms = self.read_u32()?;
                self.bus
                    .wait_irq(pin, timeout_ms)
                    .map_err(VmError::BusError)?;
            }
            // report!(kind, ...)：按 typed args 上报 u32 和/或原始字节。
            Some(Opcode::Report) => {
                let kind = self.read_u32()?;
                let n = self.read_u8()? as usize;
                if n > 8 {
                    return Err(VmError::InvalidLength);
                }
                let mut tags = [0u8; 8];
                let mut idxs = [0u8; 8];
                for i in 0..n {
                    tags[i] = self.read_u8()?;
                    idxs[i] = self.read_u8()?;
                }

                let mut args = [ReportArg::U32(0); 8];
                for i in 0..n {
                    args[i] = match tags[i] {
                        REPORT_ARG_U32 => ReportArg::U32(self.regs[idxs[i] as usize]),
                        REPORT_ARG_BYTES => {
                            let buf = idxs[i] as usize;
                            if buf >= DATA_BUF_COUNT {
                                return Err(VmError::InvalidLength);
                            }
                            let len = self.data_lens[buf];
                            ReportArg::Bytes(&self.data_bufs[buf][..len])
                        }
                        _ => return Err(VmError::InvalidOpcode),
                    };
                }
                self.bus
                    .report(kind, &args[..n])
                    .map_err(VmError::BusError)?;
            }
            Some(Opcode::WriteVar) => {
                let addr = self.read_u32()?;
                let len = self.read_u32()?;
                let delay = self.read_u32()?;
                let src = self.read_u8()? as usize;

                if len == 0 || len > 4 {
                    return Err(VmError::InvalidLength);
                }
                let val = self.regs[src];
                let mut buffer = [0u8; 4];
                let data = &mut buffer[..len as usize];
                for (i, b) in data.iter_mut().enumerate() {
                    *b = (val >> (8 * i)) as u8;
                }

                self.bus.write(addr, data).map_err(VmError::BusError)?;

                if delay > 0 {
                    self.bus.delay_us(delay).map_err(VmError::BusError)?;
                }
            }
            // `repeat!(N) { ... }`：读 count 与 body_len，把 body 重复执行 count 次。
            // body 只编译一次（在 Loop 帧内），靠计数回跳复用，字节码不随 N 膨胀。
            // 嵌套 repeat! 经递归 step() 自然处理——递归深度等于嵌套层数（非迭代数）。
            Some(Opcode::Loop) => {
                let count = self.read_u32()?;
                let body_len = self.read_u32()? as usize;
                // body_len 必须落在程序范围内，防止 body_end 越界/回绕。
                let body_end = match self.pc.checked_add(body_len) {
                    Some(end) if end <= self.program.len() => end,
                    _ => return Err(VmError::InvalidLength),
                };
                let loop_start = self.pc;
                for _ in 0..count {
                    self.pc = loop_start;
                    while self.pc < body_end {
                        match self.step()? {
                            Step::Continue => {}
                            // body 内出现 Return（DSL 不会产生，防御性）：向上传播终止。
                            Step::Returned => return Ok(Step::Returned),
                        }
                    }
                }
                // 跳过 body（count==0 时也直接落到 body_end，相当于 no-op）。
                self.pc = body_end;
            }
            Some(Opcode::Return) => {
                return Ok(Step::Returned);
            }
            // UpdateVar 尚未在 VM 中实现。
            Some(Opcode::UpdateVar) => {
                return Err(VmError::InvalidOpcode);
            }
            None => {
                return Err(VmError::InvalidOpcode);
            }
        }

        Ok(Step::Continue)
    }

    /// 逐条执行指令直到 Return 或出错。
    pub fn run(&mut self) -> Result<(), VmError> {
        loop {
            match self.step()? {
                Step::Continue => {}
                Step::Returned => return Ok(()),
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
    use crate::bus::{Bus, BusError, ReportArg};

    /// 简易 mock bus，仅用于 VM 单元测试。
    struct TestBus {
        mem: [u8; 256],
        selected_bus: Option<(BusKind, u32)>,
    }

    impl TestBus {
        fn new() -> Self {
            Self {
                mem: [0; 256],
                selected_bus: None,
            }
        }
    }

    impl Bus for TestBus {
        fn set_bus_kind(&mut self, kind: BusKind, arg: u32) -> Result<(), BusError> {
            self.selected_bus = Some((kind, arg));
            Ok(())
        }

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
            0x10,
            0x00,
            0x00,
            0x00, // addr
            0x02,
            0x00,
            0x00,
            0x00, // len
            0x00,
            0x00,
            0x00,
            0x00, // delay
            0x00, // dst reg
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
            Opcode::LoadConst as u8,
            0x00,
            0x0A,
            0x00,
            0x00,
            0x00,
            Opcode::LoadConst as u8,
            0x01,
            0x14,
            0x00,
            0x00,
            0x00,
            Opcode::Add as u8,
            0x02,
            0x00,
            0x01,
            Opcode::Return as u8,
        ];
        let mut bus = TestBus::new();
        let mut vm = Vm::new(&mut bus, &program);
        vm.run().unwrap();
        assert_eq!(vm.regs[2], 30);
    }

    #[test]
    fn set_bus_opcode_selects_bus_kind() {
        let program = [
            Opcode::SetBus as u8,
            BusKind::I2c as u8,
            0x6a,
            0x00,
            0x00,
            0x00,
            Opcode::Return as u8,
        ];
        let mut bus = TestBus::new();
        let mut vm = Vm::new(&mut bus, &program);
        vm.run().unwrap();
        assert_eq!(bus.selected_bus, Some((BusKind::I2c, 0x6a)));
    }

    #[test]
    fn shift_and_logic_opcodes_execute() {
        // LoadConst r0=0xF0 ; LoadConst r1=4 ; Shl r2=r0<<r1 ; And r3=r2&r0 ; Return
        let program = [
            Opcode::LoadConst as u8,
            0x00,
            0xF0,
            0x00,
            0x00,
            0x00,
            Opcode::LoadConst as u8,
            0x01,
            0x04,
            0x00,
            0x00,
            0x00,
            Opcode::Shl as u8,
            0x02,
            0x00,
            0x01,
            Opcode::And as u8,
            0x03,
            0x02,
            0x00,
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
            Opcode::LoadConst as u8,
            0x00,
            0x0A,
            0x00,
            0x00,
            0x00,
            Opcode::LoadConst as u8,
            0x01,
            0x00,
            0x00,
            0x00,
            0x00,
            Opcode::Div as u8,
            0x02,
            0x00,
            0x01,
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

    // ── Loop / repeat! ──────────────────────────────────────────────

    /// 记录 read/write 调用次数与 log 消息的总线，用于断言 Loop/Log 行为。
    #[derive(Default)]
    struct CountBus {
        reads: u32,
        writes: u32,
        logs: Vec<String>,
        read_lens: Vec<usize>,
        reports: Vec<(u32, Vec<OwnedReportArg>)>,
        wait_calls: u32,
        last_wait: (u8, u32),
        /// 非 0 时 `wait_irq` 返回 `Err(Timeout)`，用于测超时传播。
        wait_fail: bool,
    }

    impl Bus for CountBus {
        fn read(&mut self, _addr: u32, data: &mut [u8]) -> Result<(), BusError> {
            self.reads += 1;
            self.read_lens.push(data.len());
            Ok(())
        }
        fn write(&mut self, _addr: u32, _data: &[u8]) -> Result<(), BusError> {
            self.writes += 1;
            Ok(())
        }
        fn delay_us(&mut self, _us: u32) -> Result<(), BusError> {
            Ok(())
        }
        fn log(&mut self, msg: &str) -> Result<(), BusError> {
            self.logs.push(msg.to_owned());
            Ok(())
        }
        fn wait_irq(&mut self, pin: u8, timeout_ms: u32) -> Result<(), BusError> {
            self.wait_calls += 1;
            self.last_wait = (pin, timeout_ms);
            if self.wait_fail {
                Err(BusError::Timeout)
            } else {
                Ok(())
            }
        }
        fn report(&mut self, kind: u32, args: &[ReportArg<'_>]) -> Result<(), BusError> {
            self.reports.push((
                kind,
                args.iter()
                    .map(|arg| match arg {
                        ReportArg::U32(v) => OwnedReportArg::U32(*v),
                        ReportArg::Bytes(bytes) => OwnedReportArg::Bytes(bytes.to_vec()),
                    })
                    .collect(),
            ));
            Ok(())
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum OwnedReportArg {
        U32(u32),
        Bytes(Vec<u8>),
    }

    /// 构造 `read!(addr, 1, 0)` 的 13 字节编码。
    fn read_one(addr: u32) -> Vec<u8> {
        let mut v = vec![Opcode::Read as u8];
        v.extend_from_slice(&addr.to_le_bytes());
        v.extend_from_slice(&1u32.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v
    }

    /// 构造 `Loop | count | body_len | body`。
    fn loop_frame(count: u32, body: &[u8]) -> Vec<u8> {
        let mut v = vec![Opcode::Loop as u8];
        v.extend_from_slice(&count.to_le_bytes());
        v.extend_from_slice(&(body.len() as u32).to_le_bytes());
        v.extend_from_slice(body);
        v
    }

    #[test]
    fn loop_repeats_body_count_times() {
        // repeat!(3) { read!(0x10, 1, 0) }
        let body = read_one(0x10);
        let mut prog = loop_frame(3, &body);
        prog.push(Opcode::Return as u8);

        let mut bus = CountBus::default();
        Vm::new(&mut bus, &prog).run().unwrap();
        assert_eq!(bus.reads, 3);
        assert_eq!(bus.writes, 0);
    }

    #[test]
    fn read_dyn_uses_len_register() {
        // r0=3; ReadDyn addr=0x20 delay=0 len=r0; Return
        let mut prog = vec![Opcode::LoadConst as u8, 0];
        prog.extend_from_slice(&3u32.to_le_bytes());
        prog.push(Opcode::ReadDyn as u8);
        prog.extend_from_slice(&0x20u32.to_le_bytes());
        prog.extend_from_slice(&0u32.to_le_bytes());
        prog.push(0);
        prog.push(Opcode::Return as u8);

        let mut bus = CountBus::default();
        Vm::new(&mut bus, &prog).run().unwrap();
        assert_eq!(bus.reads, 1);
        assert_eq!(bus.read_lens, vec![3]);
    }

    #[test]
    fn report_sends_register_values_to_bus() {
        // r0=42; r1=0xaa; Report kind=0x10 values=[r0,r1]; Return
        let mut prog = vec![Opcode::LoadConst as u8, 0];
        prog.extend_from_slice(&42u32.to_le_bytes());
        prog.push(Opcode::LoadConst as u8);
        prog.push(1);
        prog.extend_from_slice(&0xaau32.to_le_bytes());
        prog.push(Opcode::Report as u8);
        prog.extend_from_slice(&0x10u32.to_le_bytes());
        prog.push(2);
        prog.push(REPORT_ARG_U32);
        prog.push(0);
        prog.push(REPORT_ARG_U32);
        prog.push(1);
        prog.push(Opcode::Return as u8);

        let mut bus = CountBus::default();
        Vm::new(&mut bus, &prog).run().unwrap();
        assert_eq!(
            bus.reports,
            vec![(
                0x10,
                vec![OwnedReportArg::U32(42), OwnedReportArg::U32(0xaa)]
            )]
        );
    }

    #[test]
    fn read_buf_then_report_sends_bytes() {
        // r0=3; ReadBuf addr=0x20 len=r0 -> buf0; Report kind=0x01 [r0, buf0]
        let mut prog = vec![Opcode::LoadConst as u8, 0];
        prog.extend_from_slice(&3u32.to_le_bytes());
        prog.push(Opcode::ReadBuf as u8);
        prog.extend_from_slice(&0x20u32.to_le_bytes());
        prog.extend_from_slice(&0u32.to_le_bytes());
        prog.push(0);
        prog.push(0);
        prog.push(Opcode::Report as u8);
        prog.extend_from_slice(&0x01u32.to_le_bytes());
        prog.push(2);
        prog.push(REPORT_ARG_U32);
        prog.push(0);
        prog.push(REPORT_ARG_BYTES);
        prog.push(0);
        prog.push(Opcode::Return as u8);

        let mut bus = CountBus::default();
        Vm::new(&mut bus, &prog).run().unwrap();
        assert_eq!(
            bus.reports,
            vec![(
                0x01,
                vec![OwnedReportArg::U32(3), OwnedReportArg::Bytes(vec![0, 0, 0])]
            )]
        );
    }

    #[test]
    fn loop_nested_multiplies_iterations() {
        // repeat!(2) { repeat!(3) { read!(0x10, 1, 0) } }
        let inner = loop_frame(3, &read_one(0x10));
        let mut prog = loop_frame(2, &inner);
        prog.push(Opcode::Return as u8);

        let mut bus = CountBus::default();
        Vm::new(&mut bus, &prog).run().unwrap();
        assert_eq!(bus.reads, 2 * 3);
    }

    #[test]
    fn loop_count_zero_is_noop() {
        // repeat!(0) { read!(0x10, 1, 0) } → body 被跳过
        let body = read_one(0x10);
        let mut prog = loop_frame(0, &body);
        prog.push(Opcode::Return as u8);

        let mut bus = CountBus::default();
        Vm::new(&mut bus, &prog).run().unwrap();
        assert_eq!(bus.reads, 0);
    }

    #[test]
    fn loop_with_write_repeats_side_effect() {
        // repeat!(3) { write!(0x10, 0xAA, 0) }
        let mut body = vec![Opcode::Write as u8];
        body.extend_from_slice(&0x10u32.to_le_bytes());
        body.extend_from_slice(&1u32.to_le_bytes()); // len
        body.extend_from_slice(&0u32.to_le_bytes()); // delay
        body.push(0xAA); // data
        let mut prog = loop_frame(3, &body);
        prog.push(Opcode::Return as u8);

        let mut bus = CountBus::default();
        Vm::new(&mut bus, &prog).run().unwrap();
        assert_eq!(bus.writes, 3);
    }

    // ── 比较 / 逻辑 / 跳转 ─────────────────────────────────────────

    /// 构造 `write!(addr, byte, 0)` 的 14 字节编码。
    fn write_one(addr: u32, byte: u8) -> Vec<u8> {
        let mut v = vec![Opcode::Write as u8];
        v.extend_from_slice(&addr.to_le_bytes());
        v.extend_from_slice(&1u32.to_le_bytes()); // len
        v.extend_from_slice(&0u32.to_le_bytes()); // delay
        v.push(byte);
        v
    }

    #[test]
    fn cmp_lt_is_signed() {
        // r0 = -1 (0xFFFFFFFF), r1 = 1, r2 = (r0 < r1) → 1（有符号 -1 < 1）
        let prog = vec![
            Opcode::LoadConst as u8,
            0,
            0xFF,
            0xFF,
            0xFF,
            0xFF,
            Opcode::LoadConst as u8,
            1,
            0x01,
            0x00,
            0x00,
            0x00,
            Opcode::CmpLt as u8,
            2,
            0,
            1,
            Opcode::Return as u8,
        ];
        let mut bus = TestBus::new();
        let mut vm = Vm::new(&mut bus, &prog);
        vm.run().unwrap();
        assert_eq!(vm.regs[2], 1);
    }

    #[test]
    fn jump_if_zero_skips_when_zero() {
        let w = write_one(0x10, 0xAA);
        let off = w.len() as i32;

        // r0=0 → 跳过 Write → 0 次 write
        let mut prog = vec![Opcode::LoadConst as u8, 0, 0, 0, 0, 0];
        prog.push(Opcode::JumpIfZero as u8);
        prog.push(0);
        prog.extend_from_slice(&off.to_le_bytes());
        prog.extend(&w);
        prog.push(Opcode::Return as u8);
        let mut bus = CountBus::default();
        Vm::new(&mut bus, &prog).run().unwrap();
        assert_eq!(bus.writes, 0);

        // r0=1 → 不跳 → 1 次 write
        let mut prog = vec![Opcode::LoadConst as u8, 0, 1, 0, 0, 0];
        prog.push(Opcode::JumpIfZero as u8);
        prog.push(0);
        prog.extend_from_slice(&off.to_le_bytes());
        prog.extend(&w);
        prog.push(Opcode::Return as u8);
        let mut bus = CountBus::default();
        Vm::new(&mut bus, &prog).run().unwrap();
        assert_eq!(bus.writes, 1);
    }

    #[test]
    fn logical_and_and_not() {
        // r0=2, r1=0; r2=r0&&r1→0; r3=!r0→0; r4=!r1→1
        let prog = vec![
            Opcode::LoadConst as u8,
            0,
            2,
            0,
            0,
            0,
            Opcode::LoadConst as u8,
            1,
            0,
            0,
            0,
            0,
            Opcode::LogAnd as u8,
            2,
            0,
            1,
            Opcode::LogNot as u8,
            3,
            0,
            Opcode::LogNot as u8,
            4,
            1,
            Opcode::Return as u8,
        ];
        let mut bus = TestBus::new();
        let mut vm = Vm::new(&mut bus, &prog);
        vm.run().unwrap();
        assert_eq!(vm.regs[2], 0); // 2 && 0
        assert_eq!(vm.regs[3], 0); // !2
        assert_eq!(vm.regs[4], 1); // !0
    }

    #[test]
    fn if_else_jumps_to_correct_branch() {
        // if (cond) { write 0x10=0x01 } else { write 0x11=0x02 }
        let then_w = write_one(0x10, 0x01);
        let else_w = write_one(0x11, 0x02);
        let jump_instr_len: i32 = 1 + 4; // Jump = 操作码 + i32

        let build = |cond: u32| -> Vec<u8> {
            let mut p = vec![Opcode::LoadConst as u8, 0];
            p.extend_from_slice(&cond.to_le_bytes());
            p.push(Opcode::JumpIfZero as u8);
            p.push(0);
            p.extend_from_slice(&(then_w.len() as i32 + jump_instr_len).to_le_bytes());
            p.extend(&then_w);
            p.push(Opcode::Jump as u8);
            p.extend_from_slice(&(else_w.len() as i32).to_le_bytes());
            p.extend(&else_w);
            p.push(Opcode::Return as u8);
            p
        };

        // cond=1 → then
        let mut bus = TestBus::new();
        Vm::new(&mut bus, &build(1)).run().unwrap();
        assert_eq!(bus.mem[0x10], 0x01);
        assert_eq!(bus.mem[0x11], 0x00);

        // cond=0 → else
        let mut bus = TestBus::new();
        Vm::new(&mut bus, &build(0)).run().unwrap();
        assert_eq!(bus.mem[0x10], 0x00);
        assert_eq!(bus.mem[0x11], 0x02);
    }

    // ── print! / Log ───────────────────────────────────────────────

    #[test]
    fn log_opcode_calls_bus_log() {
        // print!("hi") → Log | len=2 | "hi"
        let mut prog = vec![Opcode::Log as u8];
        prog.extend_from_slice(&2u32.to_le_bytes());
        prog.extend(b"hi");
        prog.push(Opcode::Return as u8);
        let mut bus = CountBus::default();
        Vm::new(&mut bus, &prog).run().unwrap();
        assert_eq!(bus.logs, vec!["hi".to_owned()]);
    }

    #[test]
    fn logvar_opcode_formats_vars() {
        // print!("v={} h={x}", r0, r1)  with r0=42, r1=0xaa
        let mut prog = vec![Opcode::LoadConst as u8, 0, 42, 0, 0, 0];
        prog.push(Opcode::LoadConst as u8);
        prog.push(1);
        prog.extend_from_slice(&0xaau32.to_le_bytes());
        let fmt = b"v={} h={x}";
        prog.push(Opcode::LogVar as u8);
        prog.push(2); // n_vars
        prog.push(0); // reg0
        prog.push(1); // reg1
        prog.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
        prog.extend(fmt);
        prog.push(Opcode::Return as u8);
        let mut bus = CountBus::default();
        Vm::new(&mut bus, &prog).run().unwrap();
        // 默认 log_vars 就地格式化后委托 log → CountBus.log 记录。
        assert_eq!(bus.logs, vec!["v=42 h=0xaa".to_owned()]);
    }

    // ── wait! / WaitIrq ────────────────────────────────────────────

    #[test]
    fn wait_irq_opcode_calls_bus_wait() {
        // wait!(pin=2, timeout=1234) → WaitIrq | pin=2 | timeout=1234 ; Return
        let prog = vec![
            Opcode::WaitIrq as u8,
            0x02,
            0xD2,
            0x04,
            0x00,
            0x00,
            Opcode::Return as u8,
        ];
        let mut bus = CountBus::default();
        Vm::new(&mut bus, &prog).run().unwrap();
        assert_eq!(bus.wait_calls, 1);
        assert_eq!(bus.last_wait, (2, 1234));
    }

    #[test]
    fn wait_irq_timeout_propagates() {
        // 总线返回 Timeout → Vm::run 返回 VmError::BusError(Timeout)。
        let prog = vec![
            Opcode::WaitIrq as u8,
            0x00,
            0xE8,
            0x03,
            0x00,
            0x00, // pin=0, timeout=1000
            Opcode::Return as u8,
        ];
        let mut bus = CountBus {
            wait_fail: true,
            ..Default::default()
        };
        assert_eq!(
            Vm::new(&mut bus, &prog).run(),
            Err(VmError::BusError(BusError::Timeout))
        );
    }
}
