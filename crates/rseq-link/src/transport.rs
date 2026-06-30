//! 字节流传输抽象与实现。
//!
//! - [`Transport`] trait:主机驱动、回环仿真、真实 MCU UART 共同实现的最小接口。
//! - [`MockTransport`]:进程内双工管道,用于测试与 `--self-test`(需 `std` feature)。
//! - [`SerialTransport`]:串口实现(需 `serial` feature)。

use crate::error::LinkError;

/// 字节流传输:主机与 MCU 之间收发原始字节的最小接口。
pub trait Transport {
    /// 尽量读取字节到 `buf`,返回实际读到的字节数。0 表示当前无数据(非 EOF)。
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, LinkError>;

    /// 完整写入 `data`(阻塞直到全部发出)。
    fn write(&mut self, data: &[u8]) -> Result<(), LinkError>;
}

/// 任何 `Transport` 的可变引用也是 `Transport`——便于把 `&mut ConcreteTransport`
/// 临时借给只需写入的组件(如 `TracingBus` 作为 `LinkTx`)。
impl<T: Transport + ?Sized> Transport for &mut T {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, LinkError> {
        (**self).read(buf)
    }
    fn write(&mut self, data: &[u8]) -> Result<(), LinkError> {
        (**self).write(data)
    }
}

// ── 进程内双工管道(std) ──────────────────────────────────────
#[cfg(feature = "std")]
pub use self::mock::MockTransport;

#[cfg(feature = "std")]
mod mock {
    use super::{LinkError, Transport};
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    /// 进程内双工字节管道,两端可互通。用于测试与回环仿真。
    pub struct MockTransport {
        /// 本端读取的队列(对端写入)。
        rx: Arc<Mutex<VecDeque<u8>>>,
        /// 本端写入的队列(对端读取)。
        tx: Arc<Mutex<VecDeque<u8>>>,
    }

    impl MockTransport {
        /// 创建一对互联的管道端。
        pub fn pair() -> (Self, Self) {
            let a_to_b = Arc::new(Mutex::new(VecDeque::new()));
            let b_to_a = Arc::new(Mutex::new(VecDeque::new()));
            let a = Self {
                rx: b_to_a.clone(),
                tx: a_to_b.clone(),
            };
            let b = Self {
                rx: a_to_b,
                tx: b_to_a,
            };
            (a, b)
        }
    }

    impl Transport for MockTransport {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize, LinkError> {
            let mut q = self.rx.lock().unwrap();
            let n = q.len().min(buf.len());
            for slot in buf.iter_mut().take(n) {
                *slot = q.pop_front().unwrap();
            }
            Ok(n)
        }

        fn write(&mut self, data: &[u8]) -> Result<(), LinkError> {
            let mut q = self.tx.lock().unwrap();
            q.extend(data);
            Ok(())
        }
    }
}

// ── 串口实现(serial feature) ─────────────────────────────────
#[cfg(feature = "serial")]
pub use self::serial::SerialTransport;

#[cfg(feature = "serial")]
mod serial {
    use super::{LinkError, Transport};
    use serialport::SerialPort;
    use std::io::{Read, Write};
    use std::time::Duration;

    /// 串口传输实现,包装 `serialport` 的 `Box<dyn SerialPort>`。
    pub struct SerialTransport {
        port: Box<dyn SerialPort>,
    }

    impl SerialTransport {
        /// 打开串口,设置波特率与 100ms 读超时(便于主机在等待时检查截止时间)。
        pub fn open(path: &str, baud: u32) -> Result<Self, LinkError> {
            let port = serialport::new(path, baud)
                .timeout(Duration::from_millis(100))
                .open()
                .map_err(|_| LinkError::Io)?;
            Ok(Self { port })
        }
    }

    impl Transport for SerialTransport {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize, LinkError> {
            self.port.read(buf).map_err(|_| LinkError::Io)
        }

        fn write(&mut self, data: &[u8]) -> Result<(), LinkError> {
            self.port.write_all(data).map_err(|_| LinkError::Io)
        }
    }
}
