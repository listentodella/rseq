//! 模拟 MCU:在进程内运行 rseq 字节码,通过 `rseq-link` 帧协议与主机交互。
//!
//! - [`mcu_loop`] 是 MCU 侧主循环:解码帧 → 处理 LOAD/EXEC/RESET/PING → 回复 ACK/Trace/Result。
//!   EXEC 时用 [`TracingBus`] 包裹 [`SimBus`],把每次总线操作流式回传为 Trace 帧。
//! - [`run_self_test`] 用进程内回环管道(MockTransport)跑一遍"编译→下发→执行→比对轨迹",
//!   供二进制 `--self-test` 与集成测试复用。
//!
//! 真实 MCU 移植时,把 `SimBus` 换成 HAL 的 `Bus` 实现、`Transport` 换成 UART 即可,
//! `mcu_loop` 的协议逻辑不变。

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use rseq_link::frame::{FrameDecoder, FrameType, HOST_FRAME_BUF, OVERHEAD, encode_into};
use rseq_link::wire::{ExecStatus, SEG_KIND_MAIN, load_segments};
use rseq_link::{LinkError, TracingBus, Transport};
use rseq_vm::{Bus, BusError, Vm};

/// MCU 程序读缓冲(每次最多从 transport 读这么多字节)。
const READ_CHUNK: usize = 256;

/// 简单的内存映射总线,作为仿真用的"MCU 总线"。
/// 真实场景下替换为 HAL 的 `Bus` 实现即可。
pub struct SimBus {
    mem: [u8; 4096],
}

impl SimBus {
    pub fn new() -> Self {
        Self { mem: [0; 4096] }
    }
}

impl Default for SimBus {
    fn default() -> Self {
        Self::new()
    }
}

impl Bus for SimBus {
    fn read(&mut self, addr: u32, data: &mut [u8]) -> Result<(), BusError> {
        for (i, slot) in data.iter_mut().enumerate() {
            *slot = self.mem[(addr as usize + i) % 4096];
        }
        Ok(())
    }
    fn write(&mut self, addr: u32, data: &[u8]) -> Result<(), BusError> {
        for (i, &b) in data.iter().enumerate() {
            self.mem[(addr as usize + i) % 4096] = b;
        }
        Ok(())
    }
    fn delay_us(&mut self, _us: u32) -> Result<(), BusError> {
        Ok(())
    }
}

/// 编码并发送一帧。
fn send_frame<T: Transport>(t: &mut T, ty: FrameType, payload: &[u8]) -> Result<(), LinkError> {
    let mut buf = vec![0u8; payload.len() + OVERHEAD];
    let n = encode_into(ty, payload, &mut buf);
    t.write(&buf[..n])
}

/// MCU 侧主循环。
///
/// 在 `transport` 上反复:读帧 → 处理 → 回复,直到 `stop` 被置位。
/// - LOAD:解析段,存主程序字节码(irq 段当前忽略),回 ACK;
/// - EXEC:回 ACK,用 `TracingBus` 包裹 `bus` 跑字节码(每次总线操作发 Trace),回 Result;
/// - RESET:清程序区,回 ACK;
/// - PING:回 Pong。
pub fn mcu_loop<B: Bus, T: Transport>(
    mut transport: T,
    mut bus: B,
    stop: Arc<AtomicBool>,
) -> Result<(), LinkError> {
    let mut dec: FrameDecoder<HOST_FRAME_BUF> = FrameDecoder::new();
    let mut read_buf = [0u8; READ_CHUNK];
    let mut inbox: VecDeque<(FrameType, Vec<u8>)> = VecDeque::new();
    let mut bytecode: Vec<u8> = Vec::new();

    loop {
        // 取下一帧(在等待期间也响应 stop)。
        let (ty, payload) = loop {
            if stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            if let Some(f) = inbox.pop_front() {
                break f;
            }
            let n = transport.read(&mut read_buf)?;
            if n == 0 {
                // 无数据:回环管道瞬时空读时退避;真实串口 read 通常会阻塞到超时。
                thread::sleep(Duration::from_micros(200));
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
                        if kind == SEG_KIND_MAIN {
                            bytecode = bytes.to_vec();
                        }
                        // irq 段(kind 0x01/0x02)当前按既定范围忽略。
                    }
                }
                send_frame(&mut transport, FrameType::Ack, &[])?;
            }
            FrameType::Exec => {
                send_frame(&mut transport, FrameType::Ack, &[])?;
                let status = if bytecode.is_empty() {
                    ExecStatus::ProgramTooShort
                } else {
                    // TracingBus 借用 transport 作 LinkTx(&mut T 经 blanket 也是 Transport);
                    // 跑完后 into_inner 回收总线并释放对 transport 的借用。
                    let mut tracing = TracingBus::new(bus, &mut transport);
                    let res = Vm::new(&mut tracing, &bytecode).run();
                    let (b, _) = tracing.into_inner();
                    bus = b;
                    match res {
                        Ok(()) => ExecStatus::Ok,
                        Err(e) => ExecStatus::from_vm_error(e),
                    }
                };
                send_frame(&mut transport, FrameType::Result, &[status as u8])?;
            }
            FrameType::Reset => {
                bytecode.clear();
                send_frame(&mut transport, FrameType::Ack, &[])?;
            }
            FrameType::Ping => {
                send_frame(&mut transport, FrameType::Pong, &[])?;
            }
            // MCU→Host 方向的帧在 MCU 侧忽略。
            _ => {}
        }
    }
}

/// 进程内端到端自检:编译一段示例源码 → 经回环管道 LOAD/EXEC → 比对轨迹。
///
/// 期望轨迹与源码语义一致(Write→Delay→Write)。供 `--self-test` 与集成测试复用。
pub fn run_self_test() -> Result<(), Box<dyn std::error::Error>> {
    use rseq::link::HostLink;
    use rseq::trace::BusOp;
    use rseq_link::MockTransport;
    use rseq_link::wire::ExecStatus as EStatus;

    let src = "write!(0x40, [0x01, 0x02, 0x03], 500);\nwrite!(0x100, 0xaa);\n";
    let program = rseq::parse(src).map_err(|e| format!("parse: {e:?}"))?;
    let bytecode = rseq::compile(&program).map_err(|e| format!("compile: {e:?}"))?;

    let (host_t, mcu_t) = MockTransport::pair();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_mcu = stop.clone();
    // MCU 侧在后台线程跑;主线程作主机。线程句柄丢弃即 detach,
    // stop 置位后 mcu_loop 会在 ms 级内自行退出。
    let _mcu = thread::Builder::new()
        .name("mcu-sim".into())
        .spawn(move || {
            let _ = mcu_loop(mcu_t, SimBus::new(), stop_mcu);
        })?;

    let mut host = HostLink::new(host_t);
    host.load(&bytecode)?;
    let res = host.exec()?;

    stop.store(true, Ordering::SeqCst);

    let expected = vec![
        BusOp::Write {
            addr: 0x40,
            data: vec![0x01, 0x02, 0x03],
        },
        BusOp::Delay { us: 500 },
        BusOp::Write {
            addr: 0x100,
            data: vec![0xaa],
        },
    ];
    if res.status != EStatus::Ok {
        return Err(format!("exec status not Ok: {:?}", res.status).into());
    }
    if res.traces != expected {
        return Err(format!(
            "trace mismatch:\n  got: {:?}\n  exp: {:?}",
            res.traces, expected
        )
        .into());
    }
    Ok(())
}
