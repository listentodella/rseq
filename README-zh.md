# Register Sequence 

这是一个专注于描述i2c/spi等总线或寄存器读写的DSL仓库, 旨在提供一个简洁、易读的语法来描述和编写总线或寄存器相关的代码, 方便快速迭代与调试.  

## 多段序列与 manifest

可以把一个设备的多个功能段拆到多个 `.rseq` 文件中, 再用 TOML manifest 组合运行:

```toml
chip = "qmi8660.yaml"

[[sequence]]
id = "init"
name = "初始化 QMI8660"
file = "qmi8660_init.rseq"

[[sequence]]
id = "enable_accel"
name = "开启加速度计"
file = "qmi8660_enable_accel.rseq"
```

`chip` 是可选项. 如果提供, CLI 会在所有功能段之前自动插入 `chip!("...");`, 因此各个功能段文件可以只描述寄存器操作.

运行全部段:

```bash
cargo run --package rseq-cli -- --manifest examples/qmi8660_manifest.toml --execute
```

按指定顺序运行部分段:

```bash
cargo run --package rseq-cli -- --manifest examples/qmi8660_manifest.toml --run init --execute
```

## 串口下发到 MCU

除了在主机端用 `--execute` 跑 `MockBus` 回放, 也可以把字节码编译出来后通过串口下发到真实 MCU 执行, MCU 边执行边把每次总线操作流式回传给主机.

### 帧协议

主机 ↔ MCU 之间走一条带校验的二进制帧协议, 帧布局(小端):

```
[sync 0x55 0xAA][type: u8][len: u16 LE][payload: len 字节][crc32: u32 LE]
```

- CRC32(IEEE) 覆盖 `type || len || payload`, 不含 sync 与 crc 本身;
- 主机 → MCU: `Load=0x01` / `Exec=0x02` / `Reset=0x03` / `Ping=0x04`;
- MCU → 主机: `Ack=0x81` / `Trace=0x82` / `Result=0x83` / `Pong=0x84`;
- `Load`/`Exec`/`Reset` 由 MCU 回 `Ack` 确认; `Exec` 期间 MCU 逐条发 `Trace`, 结束后发一条 `Result`(含状态码). `Trace`/`Result` 为尽力而为, 不重传.

`Trace` 载荷: 读/写为 `[op u8][addr u32 LE][dlen u16 LE][data]`(`op`: Read=0x01 / Write=0x02), 延时为 `[0x03][us u32 LE]`.

### 上位机: --serial

```bash
# 编译 .rseq 并通过串口下发, 收集 MCU 回传的总线轨迹
cargo run --package rseq-cli -- --file examples/qmi8660_init.rseq --serial /dev/ttyUSB0 --baud 115200
```

CLI 会 `Load` 字节码、`Exec`, 然后按执行顺序打印 `Write`/`Read`/`Delay`, 格式与 `--execute` 的 MockBus 回放一致, 便于对照.

### 模拟 MCU: rseq-mcu-sim

没有真实硬件时, `rseq-mcu-sim` 在进程内充当 MCU, 跑同一套帧协议:

```bash
# 端到端自检: 编译示例 → 回环管道下发 → 比对轨迹
cargo run --package rseq-mcu-sim -- --self-test

# 占据一个串口当 MCU(对接真实上位机)
cargo run --package rseq-mcu-sim --features serial -- --serial /dev/ttyUSB1 115200
```

### crate 布局

| crate | 作用 |
| --- | --- |
| `rseq-vm` | 字节码解释器, `no_std`, MCU/主机共用 |
| `rseq` | 主机编译器 + 芯片字典 + `link::HostLink`(主机驱动) + `trace::BusOp` |
| `rseq-link` | 帧编解码 / CRC32 / `TracingBus` / `Transport`, `no_std` 核心, 可选 `std`(回环管道) 与 `serial` |
| `rseq-cli` | 命令行: 编译 / `--execute` 回放 / `--serial` 下发 |
| `rseq-mcu-sim` | 进程内模拟 MCU(`mcu_loop` + `--self-test`), 供联调与集成测试 |

真实 MCU 移植时, 把 `rseq-link` 的 `SimBus` 换成 HAL 的 `Bus` 实现、`Transport` 换成 UART, `mcu_loop` 的协议逻辑无需改动.

## 中断命令

`irq!(pin) { on(event) { ... } }` 声明中断事件处理块, `wait!(pin, timeout_ms)` 在字节码中等待一次中断并内联派发:

```rseq
chip!("qmi8660.yaml");

irq!(int1) {
    on(fifo_watermark) {
        write!(UI.ENCTL, 0x03);
    }
}

wait!(int1, 10000);
```

主机模拟可用 `--fire` 预置中断状态快照:

```bash
cargo run -p rseq-cli -- -f examples/qmi8660_irq.rseq --execute --fire int1=0x40
```

F429ZI 固件中 `int1` 映射为 PB8 active-high。真机执行到 `wait!` 时 Zephyr GPIO ISR 只唤醒信号量, VM 在线程上下文继续运行, 然后读取 `0x58` 中断状态快照并执行匹配的 `on(...)` 代码。

## FIFO 上报与解析

中断处理块可以读取 FIFO 长度和 FIFO 数据后, 用 `report!(FIFO_RAW, fifo_len, data)` 主动上报原始 FIFO bytes。CLI 端通过 `report_format!` 解析这些 bytes, 支持按 DSL 声明的字段顺序打印原始 `i16` 或换算后的物理量。

```rseq
report_format!(FIFO_RAW, i16_le, {
    fields: [gx, gy, gz, ax, ay, az],
    gyro_fields: [gx, gy, gz],
    accel_fields: [ax, ay, az],
    accel_fs_g: 16,
    gyro_fs_dps: 4096,
    output: physical_f32,
});
```

只监听已运行 MCU 上报数据:

```bash
cargo run -q -p rseq-cli --features serial -- \
  -f examples/qmi8660_fifo.rseq \
  --watch \
  --serial /dev/cu.usbmodem314201 \
  --baud 115200
```

更完整的 FIFO report 语法、字段顺序、`physical_f32`/`raw_i16` 输出和 `frame_id`/`timestamp` 掉帧检查说明见 [SPEC-FIFO-REPORT.md](SPEC-FIFO-REPORT.md)。
