# Register Sequence 

这是一个专注于描述i2c/spi等总线或寄存器读写的DSL仓库, 旨在提供一个简洁、易读的语法来描述和编写总线或寄存器相关的代码, 方便快速迭代与调试.  

跨平台构建说明见 [BUILD.md](BUILD.md)。默认 Cargo 配置不依赖本机绝对
路径；需要调试本地 `gpui-component` 时可使用 `.cargo/gpui-local.example.toml`
生成自己的 `.cargo/config.toml` 覆盖。

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

## 总线选择

DSL 可用 `bus!` 配置后续寄存器读写走 SPI 或 I2C。MCU 固件是通用
SPI/I2C/I3C 桥，不包含任何芯片型号、WHOAMI 或候选地址知识；芯片相关
的地址和探测规则应写在 DSL/host 配置层。

```rseq
chip!("qmi8660.yaml");

bus!(spi);          // 使用默认 SPI 后端
bus!(i2c, 0x6a);    // 使用指定 7-bit I2C 地址
bus!(i3c);          // 预留接口，F429 当前返回 unsupported

bus_probe!(spi, { read: UI.WHOAMI, expect: 0x06 });
bus_probe!(i2c, { addrs: [0x6a, 0x6b], read: UI.WHOAMI, expect: 0x06 });
```

`bus!` 会编译成 VM 字节码并下发到 MCU。`bus!(i2c, 0x6a)` 编译为
`SetBus(I2c, 0x6a)`；裸 `bus!(i2c)` 会在 host 编译阶段报错，因为通用
固件无法知道某颗芯片的默认 I2C 地址。CLI/Trace 会显示 `Select spi bus`
或 `Select i2c bus arg=...`, 便于确认脚本实际切换过总线。

`bus_probe!` 也只是一条通用 VM 指令：DSL 明确给出候选地址、探测寄存器和期望值, MCU 逐个尝试 `set_bus_kind + read`，首个匹配者成为后续总线。固件仍不包含任何芯片型号逻辑。

### 帧协议

主机 ↔ MCU 之间走一条带校验的二进制帧协议, 帧布局(小端):

```
[sync 0x55 0xAA][type: u8][len: u16 LE][payload: len 字节][crc32: u32 LE]
```

- CRC32(IEEE) 覆盖 `type || len || payload`, 不含 sync 与 crc 本身;
- 主机 → MCU: `Load=0x01` / `Exec=0x02` / `Reset=0x03` / `Ping=0x04` / `Stop=0x05` / `Control=0x06`;
- MCU → 主机: `Ack=0x81` / `Trace=0x82` / `Result=0x83` / `Pong=0x84` / `ControlResult=0x85`;
- `Load`/`Exec`/`Reset`/`Stop` 由 MCU 回 `Ack` 确认; `Exec` 期间 MCU 逐条发 `Trace`, 结束后发一条 `Result`(含状态码). `Trace`/`Result` 为尽力而为, 不重传.

`Trace` 载荷: 读/写为 `[op u8][addr u32 LE][dlen u16 LE][data]`(`op`: Read=0x01 / Write=0x02), 延时为 `[0x03][us u32 LE]`.

`Control` 是 EXEC 之外的直接调试控制面，当前支持一次性总线读：
请求载荷 `[op=0x01][request_id u16 LE][addr u32 LE][len u16 LE]`，
响应载荷 `[op=0x01][request_id u16 LE][status u8][addr u32 LE][dlen u16 LE][data]`。
它复用 MCU 当前的 `bus!(...)` 状态，不会 LOAD/EXEC 临时脚本，也不会清除后台 IRQ handler。

### 上位机: --serial

```bash
# 编译 .rseq 并通过串口下发, 收集 MCU 回传的总线轨迹
cargo run --package rseq-cli -- --file examples/qmi8660_init.rseq --serial /dev/ttyUSB0 --baud 115200
```

CLI 会 `Load` 字节码、`Exec`, 然后按执行顺序打印 `Write`/`Read`/`Delay`, 格式与 `--execute` 的 MockBus 回放一致, 便于对照.

## IMU 运行时参数

芯片 YAML 顶层的 `controls` 可以把输出速率、滤波器、量程等用户参数映射到
具体寄存器位域。主机端会先读取整个寄存器，再只替换目标位域并写回，因此同一
寄存器里的其他配置位不会被覆盖。`qmi8660.yaml` 已声明 `accel_odr`、
`gyro_odr`、`accel_lpf`、`gyro_lpf`、量程和温度输出速率。

```yaml
controls:
  - name: accel_odr
    group: Sampling
    target: UI.ACTL0.aodr_ui
    options:
      - { value: 8, label: 100Hz }
      - { value: 9, label: 200Hz }
```

CLI 可列出控制项，并在连接后立即应用一个或多个启动值：

```bash
cargo run -p rseq-cli -- --chip qmi8660.yaml --list-controls

cargo run -p rseq-cli --features serial -- \
  --serial /dev/ttyUSB0 --baud 115200 \
  --chip qmi8660.yaml -f examples/qmi8660_fifo.rseq \
  --set-control accel_odr=200Hz \
  --set-control accel_lpf=preset2
```

MCU 已经运行时，也可以只发送一次实时调整而不重新 LOAD/EXEC：

```bash
cargo run -p rseq-cli --features serial -- \
  --serial /dev/ttyUSB0 --baud 115200 \
  --chip qmi8660.yaml \
  --set-control gyro_odr=400Hz
```

TUI 和 GPUI 同样接受重复的 `--set-control NAME=VALUE` 启动参数。运行时，
TUI 的 `Controls` 页可读取、循环选择或输入值；GPUI 将醒目的 `IMU Tuning`
面板直接放在 `Motion` 页，提供当前值读取、枚举按钮和自定义值输入。所有路径
使用相同的控制元数据和读-改-写逻辑。

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
| `rseq-lsp` | `.rseq` 语言服务: 诊断 / 补全 / hover, 支持 chip YAML metadata |
| `rseq-mcu-sim` | 进程内模拟 MCU(`mcu_loop` + `--self-test`), 供联调与集成测试 |

真实 MCU 移植时, 把 `rseq-link` 的 `SimBus` 换成 HAL 的 `Bus` 实现、`Transport` 换成 UART, `mcu_loop` 的协议逻辑无需改动.

## LSP 编辑器提示

`rseq-lsp` 提供标准 LSP stdio server, 可在编辑 `.rseq` 时获得语法诊断、
内置宏补全、寄存器/字段/中断事件补全以及 hover 文档:

```bash
cargo run -p rseq-lsp -- --chip qmi8660.yaml
```

如果 `.rseq` 文件里已经写了 `chip!("qmi8660.yaml");`, LSP 会自动加载该
YAML；`--chip` 适合让没有显式 `chip!` 的片段也能获得补全。更多接入示例见
`crates/rseq-lsp/README.md`。

VS Code 自动启动和语法高亮由 `editors/vscode-rseq` extension 提供。安装或
用 Extension Development Host 运行该 extension 后，打开 `*.rseq` 文件会自动
激活 `rseq` 语言、加载 TextMate 高亮并启动 `rseq-lsp`。

```bash
cd editors/vscode-rseq
pnpm install
```

在当前仓库里 extension 的默认 `rseq.lsp.command = "auto"` 会启动
`cargo run -q -p rseq-lsp --`；在外部工程中会尝试运行 PATH 里的 `rseq-lsp`。

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

保存 CSV 或可回放二进制捕获:

```bash
cargo run -q -p rseq-cli --features serial -- \
  -f examples/qmi8660_fifo.rseq \
  --watch \
  --serial /dev/cu.usbmodem314201 \
  --save fifo.csv

cargo run -q -p rseq-cli -- \
  -f examples/qmi8660_fifo.rseq \
  --replay fifo.bin
```

抢回正在后台持续上报的 MCU:

```bash
cargo run -q -p rseq-cli --features serial -- \
  --serial /dev/cu.usbmodem314201 \
  --stop
```

更完整的 FIFO report 语法、字段顺序、`physical_f32`/`raw_i16` 输出和 `frame_id`/`timestamp` 掉帧检查说明见 [SPEC-FIFO-REPORT.md](SPEC-FIFO-REPORT.md)。
