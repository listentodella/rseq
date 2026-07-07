//! 字节流传输抽象与实现。
//!
//! - [`Transport`] trait:主机驱动、回环仿真、真实 MCU UART 共同实现的最小接口。
//! - [`MockTransport`]:进程内双工管道,用于测试与 `--self-test`(需 `std` feature)。
//! - [`SerialTransport`]:串口实现(需 `serial` feature)。
//! - [`TcpTransport`]:TCP 字节流实现(需 `std` feature)。

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
pub use self::tcp::TcpTransport;

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

// ── TCP 字节流实现(std) ─────────────────────────────────────
#[cfg(feature = "std")]
mod tcp {
    use super::{LinkError, Transport};
    use std::io::{Read, Write};
    use std::net::{TcpStream, ToSocketAddrs};
    use std::time::Duration;

    /// TCP 字节流传输实现。
    ///
    /// 适用于远端机器把 USB CDC/UART 端口转成 TCP 字节流的场景。上层仍然使用
    /// rseq-link 帧协议；TCP 端只负责透明转发字节。
    pub struct TcpTransport {
        stream: TcpStream,
    }

    impl TcpTransport {
        /// 连接到 `host:port`，并设置短读超时，便于 `HostLink` 等待 Trace 时轮询截止时间。
        pub fn connect<A: ToSocketAddrs>(addr: A) -> Result<Self, LinkError> {
            let mut last_error = LinkError::Io;
            let addrs = addr.to_socket_addrs().map_err(|_| LinkError::Io)?;
            for socket_addr in addrs {
                match TcpStream::connect_timeout(&socket_addr, Duration::from_secs(5)) {
                    Ok(stream) => return Self::from_stream(stream),
                    Err(_) => last_error = LinkError::Io,
                }
            }
            Err(last_error)
        }

        /// 连接用于只观察已运行 MCU 的上报流。
        ///
        /// TCP 没有本地串口缓冲可清，因此与 [`Self::connect`] 等价。
        pub fn connect_observing<A: ToSocketAddrs>(addr: A) -> Result<Self, LinkError> {
            Self::connect(addr)
        }

        /// 从已经建立的 `TcpStream` 构造传输，主要供测试或自定义连接器使用。
        pub fn from_stream(stream: TcpStream) -> Result<Self, LinkError> {
            stream.set_nodelay(true).map_err(|_| LinkError::Io)?;
            stream
                .set_read_timeout(Some(Duration::from_millis(100)))
                .map_err(|_| LinkError::Io)?;
            stream
                .set_write_timeout(Some(Duration::from_secs(2)))
                .map_err(|_| LinkError::Io)?;
            Ok(Self { stream })
        }
    }

    impl Transport for TcpTransport {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize, LinkError> {
            match self.stream.read(buf) {
                Ok(0) => Err(LinkError::Closed),
                Ok(n) => Ok(n),
                Err(e)
                    if e.kind() == std::io::ErrorKind::TimedOut
                        || e.kind() == std::io::ErrorKind::WouldBlock =>
                {
                    Ok(0)
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::ConnectionReset
                        || e.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    Err(LinkError::Closed)
                }
                Err(_) => Err(LinkError::Io),
            }
        }

        fn write(&mut self, data: &[u8]) -> Result<(), LinkError> {
            self.stream.write_all(data).map_err(|err| match err.kind() {
                std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::UnexpectedEof => LinkError::Closed,
                _ => LinkError::Io,
            })
        }
    }
}

// ── 串口实现(serial feature) ─────────────────────────────────
#[cfg(feature = "serial")]
pub use self::serial::{SerialPortInfo, SerialTransport};

#[cfg(feature = "serial")]
mod serial {
    use super::{LinkError, Transport};
    use serialport::{ClearBuffer, SerialPort};
    use std::io::{Read, Write};
    use std::time::Duration;

    /// Host-visible serial port candidate for connection UIs.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct SerialPortInfo {
        pub port_name: String,
        pub port_type: String,
        pub manufacturer: Option<String>,
        pub product: Option<String>,
        pub serial_number: Option<String>,
        pub vid: Option<u16>,
        pub pid: Option<u16>,
    }

    /// 串口传输实现,包装 `serialport` 的 `Box<dyn SerialPort>`。
    pub struct SerialTransport {
        port: Box<dyn SerialPort>,
    }

    impl SerialTransport {
        /// 打开串口,设置波特率与 100ms 读超时(便于主机在等待时检查截止时间)。
        pub fn open(path: &str, baud: u32) -> Result<Self, LinkError> {
            Self::open_configured(path, baud, true)
        }

        /// 打开串口用于只观察已经运行的 MCU 上报流。
        ///
        /// 与 [`Self::open`] 不同，这里不清空系统串口缓冲，避免 watch/TUI 在
        /// FIFO report 正在连续到达时切掉半帧。DTR/RTS 仍保持 ready 状态，和
        /// 普通终端打开 CDC-ACM 的行为一致。
        pub fn open_observing(path: &str, baud: u32) -> Result<Self, LinkError> {
            Self::open_configured(path, baud, false)
        }

        /// 枚举本机可见串口,供 CLI/TUI/GUI 选择连接端口。
        pub fn available_ports() -> Vec<SerialPortInfo> {
            serialport::available_ports()
                .map(|ports| ports.into_iter().map(serial_port_info).collect())
                .unwrap_or_default()
        }

        fn open_configured(path: &str, baud: u32, clear_buffers: bool) -> Result<Self, LinkError> {
            let mut port = serialport::new(path, baud)
                .timeout(Duration::from_millis(100))
                .open()
                .map_err(|_| LinkError::Io)?;
            let _ = port.write_data_terminal_ready(true);
            let _ = port.write_request_to_send(true);
            if clear_buffers {
                let _ = port.clear(ClearBuffer::All);
            }
            Ok(Self { port })
        }
    }

    fn serial_port_info(info: serialport::SerialPortInfo) -> SerialPortInfo {
        let mut out = SerialPortInfo {
            port_name: info.port_name,
            port_type: serial_port_type_label(&info.port_type).to_string(),
            manufacturer: None,
            product: None,
            serial_number: None,
            vid: None,
            pid: None,
        };

        if let serialport::SerialPortType::UsbPort(usb) = info.port_type {
            out.manufacturer = usb.manufacturer;
            out.product = usb.product;
            out.serial_number = usb.serial_number;
            out.vid = Some(usb.vid);
            out.pid = Some(usb.pid);
        }
        out
    }

    fn serial_port_type_label(port_type: &serialport::SerialPortType) -> &'static str {
        match port_type {
            serialport::SerialPortType::UsbPort(_) => "USB",
            serialport::SerialPortType::BluetoothPort => "Bluetooth",
            serialport::SerialPortType::PciPort => "PCI",
            serialport::SerialPortType::Unknown => "Serial",
        }
    }

    impl Transport for SerialTransport {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize, LinkError> {
            // A read timeout means "no data available right now", not an error —
            // return 0 so the HostLink backoff loop can retry during long MCU
            // delays (e.g. a 200 ms sensor-settling gap between Trace frames).
            match self.port.read(buf) {
                Ok(n) => Ok(n),
                Err(e)
                    if e.kind() == std::io::ErrorKind::TimedOut
                        || e.kind() == std::io::ErrorKind::WouldBlock =>
                {
                    Ok(0)
                }
                Err(_) => Err(LinkError::Io),
            }
        }

        fn write(&mut self, data: &[u8]) -> Result<(), LinkError> {
            self.port.write_all(data).map_err(|_| LinkError::Io)
        }
    }
}
