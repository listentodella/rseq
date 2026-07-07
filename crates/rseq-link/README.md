# rseq-link

Register Sequence **传输层**:主机 ↔ MCU 之间的二进制帧协议与可移植的总线观测抽象。

- **协议核心 `no_std`**:帧编解码、CRC32、Trace 载荷、`TracingBus`、`Transport`/`LinkTx` trait,不依赖堆分配,可直接用于 MCU。
- **可选 `std`**:进程内回环管道 `MockTransport`(联调与单元测试),以及 TCP 字节流 `TcpTransport`。
- **可选 `serial`**:串口实现 `SerialTransport`(依赖 `serialport`)。

> 主机侧高级驱动 `HostLink` 在 [`rseq`](../rseq) crate;进程内模拟 MCU `mcu_loop` 在 [`rseq-mcu-sim`](../rseq-mcu-sim) crate。本 crate 只负责"线缆约定 + 可移植原语",不包含业务编排。

---

## 1. 帧协议

所有多字节整数**小端(LE)**。本节是跨语言对接的权威规范——MCU 侧用 C/微码实现也必须遵守。

### 1.1 帧布局

| 偏移 | 字段 | 类型 | 说明 |
|----|------|------|------|
| 0 | sync0 | u8 | 固定 `0x55` |
| 1 | sync1 | u8 | 固定 `0xAA` |
| 2 | type | u8 | 帧类型(见 1.3) |
| 3 | len | u16 LE | payload 字节数(`0..=65535`) |
| 5 | payload | `[u8; len]` | 载荷(可为空) |
| 5+len | crc32 | u32 LE | CRC32,覆盖 `type‖len‖payload`(偏移 `2..5+len`) |

- 帧头 5 字节(sync2 + type1 + len2),CRC 4 字节,固定开销 `OVERHEAD = 9`。
- 接收端见到非 `0x55 0xAA` 的字节流时,逐字节重新寻找 sync(**自动重同步**);CRC 错或 type 未知的帧**静默丢弃**并继续寻找下一帧。
- `len` 上限 65535。Trace 单帧最大载荷覆盖 read/write、log、bus select 和 report v2；当前 `MAX_TRACE_PAYLOAD = 4153`,整帧最大 `MAX_TRACE_FRAME = 4162`。

### 1.2 CRC32

标准 **CRC-32(zlib / PNG)**:多项式 `0x04C11DB7`(反射形式 `0xEDB88320`),初值 `0xFFFFFFFF`,输入输出反射,最终异或 `0xFFFFFFFF`。与 `zlib.crc32` / Python `binascii.crc32` 结果一致。

校验范围:偏移 `2..5+len`(`type‖len‖payload`),**不含** sync 与 crc 本身。

测试向量(实现内置):
- `crc32(b"123456789") = 0xCBF43926`
- `crc32(b"") = 0x00000000`

### 1.3 帧类型

| 值 | 名称 | 方向 | 载荷 | 可靠性 |
|----|------|------|------|--------|
| `0x01` | Load | H→M | LOAD 段(见 2.1) | MCU 回 Ack |
| `0x02` | Exec | H→M | 空 | MCU 回 Ack,随后 Trace 流 + Result |
| `0x03` | Reset | H→M | 空 | MCU 回 Ack |
| `0x04` | Ping | H→M | 空 | MCU 回 Pong |
| `0x05` | Stop | H→M | 空 | MCU 回 Ack,清后台 IRQ/report handler |
| `0x06` | Control | H→M | 直接控制请求(见 2.4) | MCU 回 ControlResult |
| `0x81` | Ack | M→H | 空 | 确认收到 Load/Exec/Reset/Stop |
| `0x82` | Trace | M→H | Trace 记录(见 2.2) | 尽力而为,不重传 |
| `0x83` | Result | M→H | `[status u8]`(见 2.3) | Exec 终止状态 |
| `0x84` | Pong | M→H | 空 | 响应 Ping |
| `0x85` | ControlResult | M→H | 直接控制响应(见 2.4) | 与 Control 的 request_id 匹配 |

> 方向:H=主机,M=MCU。`type` 的 bit7 置位表示 M→H 方向。

### 1.4 示例帧(可直接用于对接自测)

```
Exec(空载荷):
55 aa 02 00 00 7c 0d c5 fc

Trace Delay(500 µs):  payload = [0x03][500 LE = f4 01 00 00]
55 aa 82 05 00 03 f4 01 00 00 7e e3 62 69

Trace Write(addr=0x40, data=[01,02,03]):  payload = [0x02][0x40 LE][3 LE][01 02 03]
55 aa 82 0a 00 02 40 00 00 00 03 00 01 02 03 5a ce 4f 12
```

---

## 2. 载荷规范

### 2.1 LOAD 载荷(段式,前向兼容)

| 偏移 | 字段 | 类型 | 说明 |
|----|------|------|------|
| 0 | version | u8 | `=1` |
| 1 | seg_count | u8 | 段数 |
| 2.. | 段 ×seg_count | | 见下 |

每段:`[kind u8][seg_len u16 LE][bytes seg_len]`。

段 `kind`:

| 值 | 含义 | 当前处理 |
|----|------|----------|
| `0x00` | 主程序字节码(以 Return 结尾) | **执行** |
| `0x01` | 中断派发表 | 预留,MCU 忽略 |
| `0x02` | 中断处理段 | 预留,MCU 忽略 |

主机用 `encode_load_main_into` 打包单段 main;MCU 用 `load_segments` 零分配迭代。未来支持多段 / irq 下沉**无需改格式**。

### 2.2 Trace 载荷

读 / 写:

| 偏移 | 字段 | 类型 | 说明 |
|----|------|------|------|
| 0 | op | u8 | Read=`0x01` / Write=`0x02` |
| 1 | addr | u32 LE | 寄存器 / 总线地址 |
| 5 | dlen | u16 LE | data 字节数 |
| 7 | data | `[u8; dlen]` | 读出 / 写入的字节 |

延时:

| 偏移 | 字段 | 类型 | 说明 |
|----|------|------|------|
| 0 | op | u8 | `=0x03` |
| 1 | us | u32 LE | 延时微秒 |

> Trace 在 Exec 期间**逐条**发出,顺序与 MCU 执行顺序一致。

### 2.3 Result 载荷

| 偏移 | 字段 | 类型 |
|----|------|------|
| 0 | status | u8 |

| 值 | 状态 | 来源 |
|----|------|------|
| `0` | Ok | 字节码正常 Return |
| `1` | InvalidOpcode | 未知操作码 |
| `2` | ProgramTooShort | 无 LOAD 或程序为空 |
| `3` | InvalidLength | 操作长度非法 |
| `4` | DivideByZero | 除零 |
| `5` | BusError | 总线读写失败 |

### 2.4 Control / ControlResult 载荷

Control 是 EXEC 之外的调试控制面，用于不替换当前 rseq 程序地做一次通用总线访问。
当前仅定义直接读:

Control BusRead 请求:

| 偏移 | 字段 | 类型 | 说明 |
|----|------|------|------|
| 0 | op | u8 | BusRead=`0x01` |
| 1 | request_id | u16 LE | 主机分配,用于匹配响应 |
| 3 | addr | u32 LE | 当前 MCU bus 上的寄存器 / 总线地址 |
| 7 | len | u16 LE | 读取字节数,当前上限 `64` |

ControlResult BusRead 响应:

| 偏移 | 字段 | 类型 | 说明 |
|----|------|------|------|
| 0 | op | u8 | BusRead=`0x01` |
| 1 | request_id | u16 LE | 对应请求 |
| 3 | status | u8 | `0` 为 Ok,非 0 为错误 |
| 4 | addr | u32 LE | 实际读取地址 |
| 8 | dlen | u16 LE | data 字节数 |
| 10 | data | `[u8; dlen]` | 读取结果 |

Control BusRead 复用 MCU 当前的物理 bus 状态。例如脚本执行过 `bus!(i2c, 0x6a)` 后,
后续 Control BusRead 就从该 I2C 设备读取；固件不包含任何芯片型号或寄存器语义。

---

## 3. 协议时序

**锁步**:主机一次只发一个请求,收到响应后再发下一个。

```
主机                        MCU
  │  Load(seg)            ──→│
  │            ←──  Ack     │
  │  Exec                 ──→│
  │            ←──  Ack     │
  │            ←──  Trace*  │  (执行期间,0..N 条)
  │            ←──  Result  │  (终止状态)
  │  Reset                ──→│
  │            ←──  Ack     │
  │  Stop                 ──→│
  │            ←──  Ack     │
  │  Ping                 ──→│
  │            ←──  Pong    │
```

---

## 4. Rust API 速览

```rust
use rseq_link::{Transport, TracingBus, LinkError};
use rseq_link::frame::{FrameType, FrameDecoder, HOST_FRAME_BUF, encode_into, OVERHEAD};
use rseq_link::wire::{encode_load_main_into, encode_trace_rw, encode_trace_delay,
                     decode_trace, load_segments, ExecStatus, TraceRef, SEG_KIND_MAIN};
use rseq_link::crc32::crc32;
```

| 项 | 说明 |
|----|------|
| `Transport` trait | `read(&mut [u8]) -> Result<usize, LinkError>` / `write(&[u8]) -> Result<(), LinkError>`;`&mut T: Transport` 有 blanket impl,便于临时借用 |
| `LinkTx` trait | 只写出口(`write`);`impl<T: Transport> LinkTx for T` |
| `TracingBus<B, L, const BUF = MAX_TRACE_FRAME>` | 包裹 `B: Bus`,每次 read/write/delay 经 `L: LinkTx` 发一帧 Trace;`new`/`with_buf`/`inner_mut`/`into_inner` |
| `FrameDecoder<const N>` | 流式解码,`feed(chunk, |ty, payload| ...)`;`N` 按可用 RAM 选,主机用 `HOST_FRAME_BUF` |
| `encode_into` / `encode_trace_*` / `encode_load_main_into` | 编码端 |
| `decode_trace` / `load_segments` | 解码端 |
| `ExecStatus` | `from_u8` / `from_vm_error(VmError)` |
| `LinkError` | Io/Crc/Timeout/Nak/TooLarge/UnknownType/Closed |

---

## 5. Feature flags

| feature | 启用 | 典型场景 |
|---------|------|----------|
| (默认,空) | no_std 核心(帧/CRC/Trace/TracingBus/Trait) | MCU |
| `std` | + `MockTransport` + `TcpTransport` + `LinkError: std::error::Error` | 主机 / 仿真 / TCP 转发 |
| `serial` | + `SerialTransport`(依赖 `serialport`) | 主机串口下发 |

---

## 6. 用法

### 6.1 传输端

本地串口:

```bash
cargo run -p rseq-cli -- --serial /dev/cu.usbmodem314201 --baud 115200 -f examples/qmi8660_fifo.rseq
```

远端 CDC/UART 透明转发为 TCP 时,远端机器负责真实串口参数,本机只连 TCP 字节流:

```bash
# 远端机器示例
python3 skills/serial/scripts/serial_tcp_forward.py --serial /dev/ttyACM0 --baud 115200 --listen 0.0.0.0:5657

# 或使用 socat
socat -d -d TCP-LISTEN:5657,reuseaddr,fork FILE:/dev/ttyACM0,raw,b115200,cs8,-parenb,-cstopb

# 本机
cargo run -p rseq-cli -- --tcp 10.2.8.42:5657 -f examples/qmi8660_fifo.rseq
cargo run -p rseq-cli -- --watch --tcp 10.2.8.42:5657 -f examples/qmi8660_fifo.rseq
```

TCP 只是透明字节流,帧协议仍然是本 crate 定义的 rseq-link 协议。

### 6.2 主机端(经 `rseq::link::HostLink`)

`HostLink` 封装了第 3 节的锁步时序,见 [`rseq`](../rseq) crate:

```rust
use rseq::link::HostLink;
let mut host = HostLink::new(transport);
host.load(&bytecode)?;            // → Ack
let res = host.exec()?;           // → Ack, 收 Trace 流, → Result
// res.status: ExecStatus, res.traces: Vec<BusOp>
host.ping()?;                     // → Pong
host.stop_reports()?;             // → Ack, clear background report stream
```

### 6.3 MCU 端(经 `rseq-mcu-sim::mcu_loop`)

参考实现见 [`rseq-mcu-sim`](../rseq-mcu-sim);核心就是把 `Bus` 包进 `TracingBus` 跑 `Vm`:

```rust
let mut tracing = TracingBus::new(bus, &mut transport);  // transport 作 LinkTx
let res = Vm::new(&bytecode, &mut tracing).run();         // 每次 Bus 调用自动发 Trace
let (bus, _) = tracing.into_inner();                     // 回收总线、释放借用
```

### 6.3 直接用本 crate(自定义收发循环)

```rust
let mut dec: FrameDecoder<{ HOST_FRAME_BUF }> = FrameDecoder::new();
let mut buf = [0u8; 256];
loop {
    let n = transport.read(&mut buf)?;
    dec.feed(&buf[..n], |ty, payload| match ty {
        FrameType::Trace => {
            if let Some(t) = decode_trace(payload) { /* 记录 */ }
        }
        FrameType::Result => {
            let s = ExecStatus::from_u8(payload[0]);
        }
        _ => {}
    });
}
```

---

## 7. MCU 移植指南

把本 crate 用到真实芯片,只需实现两件东西:

1. **`Bus`**(在 `rseq-vm` 里定义):`read(addr, &mut [u8])` / `write(addr, &[u8])` / `delay_us(us)`——对接你的 I²C/SPI/寄存器 HAL。
2. **`Transport`**:`read` / `write`——对接 UART DMA(或 USB CDC 等)。

随后即可:
- 用 `FrameDecoder<{ 你的缓冲大小 }>` 解帧;
- 用 `TracingBus<你的Bus, &mut 你的Transport>` 包裹,跑 `Vm`;
- 或直接复用 `rseq-mcu-sim::mcu_loop`(它就是上述逻辑的参考实现,把 `SimBus` 换成你的 `Bus` 即可)。

**内存预算**:位运算 CRC32 无表(省 1KB 表);`FrameDecoder<N` 的 `N` 按可容纳最大 Trace 帧选取(`MAX_TRACE_FRAME = 4112`,若不需要 4096 字节单次读可按实际调小并用 `TracingBus::with_buf`)。

---

## 8. 测试

```bash
# 三档编译验证
cargo check -p rseq-link
cargo check -p rseq-link --features std
cargo check -p rseq-link --features serial

# 单测(默认 no_std 构建也能跑:测试模块自带 extern crate std)
cargo test -p rseq-link
cargo test -p rseq-link --features std
```

覆盖:CRC 往返与增量等价、帧往返、垃圾后重同步、CRC 不匹配丢弃、分块/单字节 feed、单块多帧、Trace 往返、LOAD 段迭代、ExecStatus 往返、TracingBus 发读/写/延迟且 TX 失败不中断总线操作。
