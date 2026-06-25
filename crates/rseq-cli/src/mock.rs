use rseq_vm::{Bus, BusError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BusOp {
    Read { addr: u32, data: Vec<u8> },
    Write { addr: u32, data: Vec<u8> },
    Delay { us: u32 },
}

pub struct MockBus {
    ops: Vec<BusOp>,
}

impl MockBus {
    pub fn new() -> Self {
        Self { ops: Vec::new() }
    }

    pub fn ops(&self) -> &[BusOp] {
        &self.ops
    }
}

impl Default for MockBus {
    fn default() -> Self {
        Self::new()
    }
}

impl Bus for MockBus {
    fn read(&mut self, addr: u32, data: &mut [u8]) -> Result<(), BusError> {
        self.ops.push(BusOp::Read {
            addr,
            data: data.to_vec(),
        });
        Ok(())
    }

    fn write(&mut self, addr: u32, data: &[u8]) -> Result<(), BusError> {
        self.ops.push(BusOp::Write {
            addr,
            data: data.to_vec(),
        });
        Ok(())
    }

    fn delay_us(&mut self, us: u32) -> Result<(), BusError> {
        self.ops.push(BusOp::Delay { us });
        Ok(())
    }
}
