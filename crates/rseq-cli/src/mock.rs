
use rseq_vm::{Bus, BusError};
use std::collections::HashMap;

pub struct MockBus {
    memory: HashMap<u32, u8>,
    delay_count: u32,
}

impl MockBus {
    pub fn new() -> Self {
        Self {
            memory: HashMap::new(),
            delay_count: 0,
        }
    }

    pub fn get_memory(&self) -> &HashMap<u32, u8> {
        &self.memory
    }

    pub fn get_delay_count(&self) -> u32 {
        self.delay_count
    }

    pub fn set_memory(&mut self, addr: u32, value: u8) {
        self.memory.insert(addr, value);
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
        Ok(())
    }

    fn write(&mut self, addr: u32, data: &[u8]) -> Result<(), BusError> {
        for i in 0..data.len() {
            self.memory.insert(addr + i as u32, data[i]);
        }
        Ok(())
    }

    fn delay_us(&mut self, us: u32) -> Result<(), BusError> {
        self.delay_count += us;
        Ok(())
    }
}
