//! rseq MCU firmware (Nucleo F429ZI).
//!
//! Speaks the rseq-link frame protocol ([0x55 0xAA] sync + CRC32) over the USB
//! CDC-ACM port: receives Load/Exec/Reset/Ping, sends Ack/Trace/Result/Pong.
//! On Exec, the rseq VM runs the loaded bytecode against [`ImuSpiBus`] (a real
//! QMI8660 IMU over SPI), and a [`TracingBus`] emits a Trace frame per bus op.
//!
//! no_std + alloc; the `zephyr` crate supplies the global allocator, panic
//! handler, and log backend (logs go to USART3 / ST-Link VCP).

#![no_std]
extern crate alloc;

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

/* log macros bypassed — use rust_printk for console output (USART3). */

use rseq_link::frame::{encode_into, FrameDecoder, FrameType, OVERHEAD};
use rseq_link::wire::{load_segments, ExecStatus, SEG_KIND_IRQ_INT1, SEG_KIND_MAIN};
use rseq_link::{LinkError, TracingBus, Transport};
use rseq_vm::{Bus, BusError, Vm};

// ── IRQ 处理器存储 ────────────────────────────────────────────
/// 最多支持 2 个中断引脚（INT1/INT2）
const MAX_IRQ_HANDLERS: usize = 2;

/// 一个中断处理器：注册后在 ISR 触发时自动运行
struct IrqHandler {
    bytecode: Vec<u8>,
}

/// 全局中断处理器数组（pin_id → handler）
static mut IRQ_HANDLERS: [Option<IrqHandler>; MAX_IRQ_HANDLERS] = [None, None];

/// IRQ 待处理标志（ISR 中设置，主循环中检查）
static IRQ_PENDING: [AtomicBool; MAX_IRQ_HANDLERS] =
    [AtomicBool::new(false), AtomicBool::new(false)];

mod ffi {
    extern "C" {
        pub fn rust_usb_enable() -> i32;
        pub fn rust_uart_init() -> i32;
        pub fn rust_uart_read(buf: *mut u8, len: usize) -> i32;
        pub fn rust_uart_write(data: *const u8, len: usize) -> i32;
        pub fn rust_event_wait(timeout_ms: u32) -> i32;
        pub fn rust_kernel_delay_us(us: u32);
        pub fn rust_printk(s: *const u8, len: usize);

        pub fn rust_spi_bus_is_ready() -> i32;
        pub fn rust_spi_bus_transceive(
            tx: *const u8,
            tx_len: usize,
            rx: *mut u8,
            rx_len: usize,
        ) -> i32;
        pub fn rust_spi_cs_init() -> i32;
        pub fn rust_spi_cs_set_low() -> i32;
        pub fn rust_spi_cs_set_high() -> i32;
        pub fn rust_irq_init() -> i32;
        pub fn rust_irq_wait(pin: u8, timeout_ms: u32) -> i32;
    }
}

fn check(ret: i32) -> Result<(), i32> {
    if ret == 0 {
        Ok(())
    } else {
        Err(ret)
    }
}

/// Raw console output (USART3 via the C `rust_printk` FFI), independent of the
/// log backend so bring-up diagnostics are visible even if `set_logger` fails.
fn printk(s: &str) {
    unsafe { ffi::rust_printk(s.as_ptr(), s.len()) };
}

// ============================================================================
// Transport: rseq-link over the CDC-ACM UART FFI
// ============================================================================

/// [`Transport`] backed by the CDC-ACM UART. `read` blocks for the first byte
/// then drains whatever is immediately available (the rseq-link lockstep
/// protocol means at most one command is in flight at a time).
struct CdcTransport;

impl Transport for CdcTransport {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, LinkError> {
        if buf.is_empty() {
            return Ok(0);
        }
        let ret = unsafe { ffi::rust_uart_read(buf.as_mut_ptr(), buf.len()) };
        if ret < 0 {
            return Err(LinkError::Io);
        }
        Ok(ret as usize)
    }

    fn write(&mut self, data: &[u8]) -> Result<(), LinkError> {
        check(unsafe { ffi::rust_uart_write(data.as_ptr(), data.len()) }).map_err(|_| LinkError::Io)
    }
}

// ============================================================================
// Bus: rseq-vm Bus over the QMI8660 SPI FFI
// ============================================================================

/// Scratch for SPI frames: 1 header byte + payload. QMI8660 reads are <=4 and
/// writes are small, so 64 is ample; oversized ops fail with AccessSizeMismatch.
const SPI_SCRATCH: usize = 64;
const SPI_MAX_PAYLOAD: usize = SPI_SCRATCH - 1;
const QMI8660_FIFO_DATA_REG: u8 = 0x57;

/// rseq [`Bus`] over the IMU's SPI. The rseq DSL encodes a plain 8-bit register
/// number as the `u32` address, so `addr & 0xff` is the register. QMI8660 SPI
/// convention: read header = `reg | 0x80`, write header = `reg & 0x7f`, and the
/// first received byte (response to the header) is a dummy that must be stripped.
struct ImuSpiBus;

impl ImuSpiBus {
    fn new() -> Result<Self, i32> {
        printk("rseq: ImuSpiBus::new start\n");

        check(unsafe { ffi::rust_spi_bus_is_ready() })?;
        printk("rseq: spi ready\n");

        check(unsafe { ffi::rust_spi_cs_init() })?;
        printk("rseq: cs init ok\n");

        let ret = unsafe { ffi::rust_irq_init() };
        if ret == 0 {
            printk("rseq: irq init ok\n");
        } else {
            printk("rseq: irq init FAILED\n");
        }
        check(ret)?;

        Ok(Self)
    }

    fn cs_low() -> Result<(), BusError> {
        check(unsafe { ffi::rust_spi_cs_set_low() }).map_err(|_| BusError::HardwareFailure)
    }

    fn cs_high() {
        unsafe {
            ffi::rust_spi_cs_set_high();
        }
    }

    fn read_chunk(reg: u8, data: &mut [u8]) -> Result<(), BusError> {
        let len = data.len();
        if len == 0 || len + 1 > SPI_SCRATCH {
            return Err(BusError::AccessSizeMismatch);
        }

        let mut tx = [0u8; SPI_SCRATCH];
        let mut rx = [0u8; SPI_SCRATCH];
        tx[0] = reg | 0x80;
        let total = 1 + len;

        Self::cs_low()?;
        let ret =
            unsafe { ffi::rust_spi_bus_transceive(tx.as_ptr(), total, rx.as_mut_ptr(), total) };
        Self::cs_high();
        if let Err(_e) = check(ret) {
            printk(&alloc::format!(
                "rseq: spi read fail reg=0x{reg:02x} ret={ret}\n"
            ));
            return Err(BusError::HardwareFailure);
        }

        data.copy_from_slice(&rx[1..1 + len]);
        Ok(())
    }
}

impl Bus for ImuSpiBus {
    fn read(&mut self, addr: u32, data: &mut [u8]) -> Result<(), BusError> {
        let len = data.len();
        if len == 0 {
            return Err(BusError::AccessSizeMismatch);
        }

        let reg = (addr & 0xff) as u8;

        if len <= SPI_MAX_PAYLOAD {
            return Self::read_chunk(reg, data);
        }

        if reg != QMI8660_FIFO_DATA_REG {
            return Err(BusError::AccessSizeMismatch);
        }

        for chunk in data.chunks_mut(SPI_MAX_PAYLOAD) {
            Self::read_chunk(reg, chunk)?;
        }
        Ok(())
    }

    fn write(&mut self, addr: u32, data: &[u8]) -> Result<(), BusError> {
        let len = data.len();
        if len == 0 || len + 1 > SPI_SCRATCH {
            return Err(BusError::AccessSizeMismatch);
        }

        let reg = (addr & 0xff) as u8;
        let mut tx = [0u8; SPI_SCRATCH];
        tx[0] = reg & 0x7f; // write
        tx[1..1 + len].copy_from_slice(data);
        let total = 1 + len;

        // printk(&alloc::format!(
        //     "rseq: spi write start reg=0x{reg:02x} len={len} first=0x{:02x}\n",
        //     data[0]
        // ));
        Self::cs_low()?;
        let ret =
            unsafe { ffi::rust_spi_bus_transceive(tx.as_ptr(), total, core::ptr::null_mut(), 0) };
        Self::cs_high();
        if let Err(_e) = check(ret) {
            printk(&alloc::format!(
                "rseq: spi write fail reg=0x{reg:02x} ret={ret}\n"
            ));
            return Err(BusError::HardwareFailure);
        }
        // printk(&alloc::format!("rseq: spi write ok reg=0x{reg:02x}\n"));
        Ok(())
    }

    fn delay_us(&mut self, us: u32) -> Result<(), BusError> {
        unsafe { ffi::rust_kernel_delay_us(us) };
        Ok(())
    }

    /// `print!("msg")`：经 `rust_printk` 输出到 USART3 控制台。
    /// TracingBus 在此之上还会回传一条 Log trace 给主机 CDC。
    fn log(&mut self, msg: &str) -> Result<(), BusError> {
        unsafe { ffi::rust_printk(msg.as_ptr(), msg.len()) };
        Ok(())
    }

    fn wait_irq(&mut self, pin: u8, timeout_ms: u32) -> Result<(), BusError> {
        match unsafe { ffi::rust_irq_wait(pin, timeout_ms) } {
            0 => Ok(()),
            -1 => Err(BusError::Timeout),
            _ => Err(BusError::HardwareFailure),
        }
    }
}

// ============================================================================
// IRQ 回调：从 C ISR 调用，设置待处理标志
// ============================================================================

/// C ISR 回调：INT1 触发时设置标志，由主循环轮询处理。
/// 不在 ISR 中运行 VM（避免阻塞 SPI/I2C）。
#[no_mangle]
pub extern "C" fn rust_irq_int1_triggered() {
    IRQ_PENDING[0].store(true, Ordering::Release);
}

// ============================================================================
// rseq-link mcu_loop (no_std port of rseq-mcu-sim's loop)
// ============================================================================

const READ_CHUNK: usize = 256;
/// Smaller than HOST_FRAME_BUF (64K) to keep the decoder off the stack; a
/// LOAD frame larger than this is dropped (auto-resync). QMI8660 programs are
/// tiny, so 2 KiB is ample.
const DEC_BUF: usize = 2048;

fn send_frame<T: Transport>(t: &mut T, ty: FrameType, payload: &[u8]) -> Result<(), LinkError> {
    let mut buf = alloc::vec![0u8; payload.len() + OVERHEAD];
    let n = encode_into(ty, payload, &mut buf);
    t.write(&buf[..n])
}

fn run_pending_irqs<B: Bus>(bus: &mut B) {
    for pin_id in 0..MAX_IRQ_HANDLERS {
        if !IRQ_PENDING[pin_id].swap(false, Ordering::Acquire) {
            continue;
        }

        unsafe {
            if let Some(handler) = &IRQ_HANDLERS[pin_id] {
                if let Err(e) = Vm::new(bus, &handler.bytecode).run() {
                    printk(&alloc::format!("rseq: irq handler error: {:?}\n", e));
                }
            }
        }
    }
}

/// MCU-side main loop: decode frames from `transport`, dispatch Load/Exec/
/// Reset/Ping, and reply Ack/Trace/Result/Pong. `stop` is polled each iteration.
/// The transport's `read` is expected to block (the CDC UART does), so there is
/// no idle sleep here.
fn mcu_loop<B: Bus, T: Transport>(
    mut transport: T,
    mut bus: B,
    stop: &AtomicBool,
) -> Result<(), LinkError> {
    let mut dec: FrameDecoder<DEC_BUF> = FrameDecoder::new();
    let mut read_buf = [0u8; READ_CHUNK];
    let mut inbox: VecDeque<(FrameType, Vec<u8>)> = VecDeque::new();
    let mut bytecode: Vec<u8> = Vec::new();

    printk("rseq: main loop start\n");

    loop {
        run_pending_irqs(&mut bus);

        // Pull the next complete frame, responding to `stop` while waiting.
        let (ty, payload) = loop {
            if stop.load(Ordering::Relaxed) {
                printk("rseq: stop requested\n");
                return Ok(());
            }

            run_pending_irqs(&mut bus);

            if let Some(f) = inbox.pop_front() {
                break f;
            }

            // 读取超时返回 0（允许 IRQ 轮询）
            let n = match transport.read(&mut read_buf) {
                Ok(n) => n,
                Err(e) => {
                    printk("rseq: transport.read error\n");
                    return Err(e);
                }
            };

            if n == 0 {
                // Sleep until CDC RX or INT1 wakes the loop. The long timeout is
                // only a defensive fallback; normal IRQ latency is event-driven.
                unsafe {
                    ffi::rust_event_wait(1000);
                }
                continue;
            }

            let mut captured: Vec<(FrameType, Vec<u8>)> = Vec::new();
            dec.feed(&read_buf[..n], |ty, p| {
                captured.push((ty, p.to_vec()));
            });
            for f in captured {
                inbox.push_back(f);
            }
        };

        match ty {
            FrameType::Load => {
                if let Some((_ver, segs)) = load_segments(&payload) {
                    for (kind, bytes) in segs {
                        match kind {
                            SEG_KIND_MAIN => {
                                bytecode = bytes.to_vec();
                            }
                            SEG_KIND_IRQ_INT1 => {
                                // 注册 INT1 中断处理器
                                unsafe {
                                    IRQ_HANDLERS[0] = Some(IrqHandler {
                                        bytecode: bytes.to_vec(),
                                    });
                                }
                                printk("rseq: irq int1 handler registered\n");
                            }
                            _ => {
                                // 忽略未知段类型
                            }
                        }
                    }
                }
                send_frame(&mut transport, FrameType::Ack, &[])?;
            }
            FrameType::Exec => {
                printk("rseq: EXEC start\n");
                if let Err(_e) = send_frame(&mut transport, FrameType::Ack, &[]) {
                    printk("rseq: send Ack failed\n");
                    continue;
                }
                let status = if bytecode.is_empty() {
                    ExecStatus::ProgramTooShort
                } else {
                    // TracingBus borrows transport as LinkTx during EXEC;
                    // into_inner reclaims the bus and releases the borrow.
                    let mut tracing = TracingBus::new(bus, &mut transport);
                    let res = Vm::new(&mut tracing, &bytecode).run();
                    let (b, _) = tracing.into_inner();
                    bus = b;
                    match res {
                        Ok(()) => ExecStatus::Ok,
                        Err(e) => ExecStatus::from_vm_error(e),
                    }
                };
                printk("rseq: sending Result frame\n");
                if let Err(_e) = send_frame(&mut transport, FrameType::Result, &[status as u8]) {
                    printk("rseq: send Result failed\n");
                    continue;
                }
                printk("rseq: EXEC complete, back to loop\n");
            }
            FrameType::Reset => {
                printk("rseq: RESET\n");
                bytecode.clear();
                // 清除中断处理器
                unsafe {
                    for i in 0..MAX_IRQ_HANDLERS {
                        IRQ_HANDLERS[i] = None;
                    }
                }
                printk("rseq: reset, irq handlers cleared\n");
                send_frame(&mut transport, FrameType::Ack, &[])?;
            }
            FrameType::Ping => {
                printk("rseq: PING\n");
                send_frame(&mut transport, FrameType::Pong, &[])?;
            }
            // MCU→Host frames are ignored on the MCU side.
            _ => {
                printk("rseq: unknown frame type\n");
            }
        }

        printk("rseq: match complete, looping back\n");
        printk("rseq: --- end of iteration, back to top ---\n");
    }
}

static STOP: AtomicBool = AtomicBool::new(false);

#[no_mangle]
pub extern "C" fn rust_main() {
    // Pulls in the zephyr crate's runtime (global allocator + panic handler).
    // (The log backend itself isn't relied on — rust_printk is used for output.)
    unsafe {
        let _ = zephyr::set_logger();
    }
    printk("rseq: rust_main start\n");

    let r = unsafe { ffi::rust_usb_enable() };
    printk(&alloc::format!("rseq: rust_usb_enable={}\n", r));
    if r != 0 {
        return;
    }

    let r = unsafe { ffi::rust_uart_init() };
    printk(&alloc::format!("rseq: rust_uart_init={}\n", r));
    if r != 0 {
        return;
    }

    let bus = match ImuSpiBus::new() {
        Ok(b) => {
            printk("rseq: imu spi ready\n");
            b
        }
        Err(e) => {
            printk(&alloc::format!("rseq: imu spi fail={}\n", e));
            return;
        }
    };

    printk("rseq: enter mcu_loop\n");
    if mcu_loop(CdcTransport, bus, &STOP).is_err() {
        printk("rseq: mcu_loop error\n");
    }
}
