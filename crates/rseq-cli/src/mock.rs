use rseq_vm::{Bus, BusError};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BusOp {
    Read { addr: u32, data: Vec<u8> },
    Write { addr: u32, data: Vec<u8> },
    Delay { us: u32 },
}

pub struct MockBus {
    memory: HashMap<u32, u8>,
    ops: Vec<BusOp>,
}

impl MockBus {
    pub fn new() -> Self {
        Self {
            memory: HashMap::new(),
            ops: Vec::new(),
        }
    }

    pub fn ops(&self) -> &[BusOp] {
        &self.ops
    }

    pub fn memory(&self) -> &HashMap<u32, u8> {
        &self.memory
    }
}

impl Default for MockBus {
    fn default() -> Self {
        Self::new()
    }
}

impl Bus for MockBus {
    fn read(&mut self, addr: u32, data: &mut [u8]) -> Result<(), BusError> {
        for i in 0..data.len() {
            data[i] = self.memory.get(&(addr + i as u32)).copied().unwrap_or(0);
        }
        self.ops.push(BusOp::Read {
            addr,
            data: data.to_vec(),
        });
        Ok(())
    }

    fn write(&mut self, addr: u32, data: &[u8]) -> Result<(), BusError> {
        for (i, &byte) in data.iter().enumerate() {
            self.memory.insert(addr + i as u32, byte);
        }
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
