# rseq-mcu-sim

进程内**模拟 MCU**:跑同一套 [rseq-link 帧协议](../rseq-link/README.md),在无真实硬件时充当对端,用于联调与端到端测试。

- **库** `mcu_loop`:MCU 主循环(解帧 → LOAD/EXEC/RESET/PING → 回 Ack/Trace/Result);`SimBus`:4KB 内存映射总线;`run_self_test`:端到端自检。
- **二进制** `rseq-mcu-sim`:`--self-test` 自检、`--serial PATH [BAUD]` 占据串口当 MCU。
- **集成测试** `tests/loopback.rs`:经回环管道比对 BusOp 轨迹。

> 真实 MCU 移植时,把 `SimBus` 换成 HAL 的 `Bus`、`Transport` 换成 UART,`mcu_loop` 的协议逻辑**无需改动**。

---

## 1. 它在链路中的位置

```
.rseq 源码 → rseq 编译字节码 → rseq-cli(HostLink) ──帧──→ MCU(mcu_loop)
                                                          │ TracingBus 包 SimBus 跑 Vm
                                                          │ 每次总线操作发 Trace
   ←──────────────────────── Trace / Result / Ack ─────────┘
```

`rseq-mcu-sim` 就是上图中"MCU"一角的参考实现。没有硬件时,主机与模拟 MCU 之间用 `MockTransport` 的进程内回环管道相连。

---

## 2. 快速开始

### 2.1 自检(一条命令验证整条链路)

```bash
cargo run -p rseq-mcu-sim -- --self-test
# → rseq-mcu-sim: self-test passed
```

自检流程:编译示例源码(`write!(0x40,[01,02,03],500); write!(0x100,0xaa);`)→ 经回环管道 LOAD/EXEC → 比对回传轨迹为 `Write→Delay→Write`。

### 2.2 当串口 MCU(对接真实上位机)

需要 `serial` feature:

```bash
cargo run -p rseq-mcu-sim --features serial -- --serial /dev/ttyUSB1 115200
```

另一终端用 CLI 下发:

```bash
cargo run -p rseq-cli -- --file examples/qmi8660_init.rseq --serial /dev/ttyUSB0 --baud 115200
```

> 两个串口对连(或虚拟串口对)即可:一端跑 `rseq-mcu-sim --serial`,另一端跑 `rseq-cli --serial`。

---

## 3. 库 API

### `mcu_loop`

```rust
pub fn mcu_loop<B: Bus, T: Transport>(
    transport: T,
    bus: B,
    stop: Arc<AtomicBool>,
) -> Result<(), LinkError>
```

MCU 侧主循环:读帧 → 处理 → 回复,直到 `stop` 被置位。

| 收到 | 动作 |
|------|------|
| `Load` | 解析段,存主程序字节码(irq 段忽略)→ 回 `Ack` |
| `Exec` | 回 `Ack` → 用 `TracingBus` 包裹 `bus` 跑 `Vm`(每次总线操作发 `Trace`)→ 回 `Result(status)` |
| `Reset` | 清程序区 → 回 `Ack` |
| `Stop` | 清后台流(模拟器无 IRQ handler, 仅确认协议) → 回 `Ack` |
| `Ping` | 回 `Pong` |

EXEC 时:`TracingBus::new(bus, &mut transport)` 借用 transport 作 `LinkTx`,跑完 `into_inner()` 回收总线并释放借用,随后继续读下一帧。

### `SimBus`

```rust
pub struct SimBus { /* 4KB 内存 */ }   // impl Bus
```

简单内存映射总线(地址 mod 4096),仿真用的"MCU 总线"。真实场景替换为 HAL 的 `Bus` 即可。

### `run_self_test`

```rust
pub fn run_self_test() -> Result<(), Box<dyn std::error::Error>>
```

编译示例 → 回环 LOAD/EXEC → 比对轨迹。供二进制 `--self-test` 与集成测试复用。

---

## 4. 端到端联调(进程内回环)

不启动二进制,直接在代码里跑:

```rust
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use rseq::link::HostLink;
use rseq_link::MockTransport;
use rseq_mcu_sim::{mcu_loop, SimBus};

let (host_t, mcu_t) = MockTransport::pair();
let stop = Arc::new(AtomicBool::new(false));
let stop_mcu = stop.clone();
let _mcu = std::thread::spawn(move || { let _ = mcu_loop(mcu_t, SimBus::new(), stop_mcu); });

let mut host = HostLink::new(host_t);
host.load(&bytecode).unwrap();
let res = host.exec().unwrap();   // res.traces: Vec<BusOp>
stop.store(true, Ordering::SeqCst);
```

---

## 5. 移植到真实 MCU

`mcu_loop` 是协议参考实现。落地到具体芯片:

1. 实现 `Bus`(I²C/SPI/寄存器读写 + `delay_us`)——替换 `SimBus`。
2. 实现 `Transport`(UART 收发)。
3. 调用 `mcu_loop(your_transport, your_bus, stop)`,或照其逻辑自行编排。

协议细节(帧布局、CRC、载荷)见 [rseq-link/README.md](../rseq-link/README.md)。

---

## 6. Feature flags

| feature | 启用 |
|---------|------|
| (默认,空) | `mcu_loop` / `SimBus` / `run_self_test` + 回环自检 |
| `serial` | 二进制 `--serial` 路径(`SerialTransport`,依赖 `serialport`) |

---

## 7. 测试

```bash
cargo run -p rseq-mcu-sim -- --self-test        # 端到端自检
cargo test -p rseq-mcu-sim                       # 3 项回环集成测试
```

`tests/loopback.rs` 三项:
- `loopback_self_test_matches_expected_traces` — 复用 `run_self_test`;
- `loopback_exec_traces_match` — 显式 `load→exec→比对 BusOp`;
- `loopback_ping_pong` — Ping/Pong 往返。

也可跑全量:

```bash
cargo test --workspace
```
