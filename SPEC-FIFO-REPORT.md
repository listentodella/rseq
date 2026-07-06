# FIFO report parsing SPEC

本文档描述 `report!` 与 `report_format!` 在 FIFO 数据上报、解析和打印中的约定。

当前设计采用 host/MCU 分工：

- MCU 运行 `report!` 字节码, 通过已有 Trace 帧主动上报结构化事件。
- CLI 读取 `.rseq` 或 manifest 中的 `report_format!` 元数据, 在接收端解析和打印 raw FIFO 数据。
- `report_format!` 只影响 CLI 展示, 不生成 VM 字节码, 不改变 MCU 中断处理路径和总线读写时序。

## Report 类型

`report!(kind, ...)` 的 `kind` 可以是数值, 也可以使用内置名称：

| 名称 | 数值 | 当前用途 |
| --- | ---: | --- |
| `FIFO_RAW` | `0x01` | FIFO 原始字节流 |
| `AMD` | `0x02` | 占位: any motion detected |
| `SMD` | `0x03` | 占位: significant motion detected |
| `DRDY` | `0x04` | 占位: data ready |

单条 `report!` 最多携带 8 个参数, 其中最多 1 个 raw bytes 参数。raw bytes 最大长度为 4096 字节。

## FIFO_RAW 上报写法

FIFO watermark 中断通常需要通过读取 FIFO 状态和 FIFO 数据来撤销条件, 而不是写寄存器清中断。推荐写法：

```rseq
chip!("qmi8660.yaml");
bus_probe!(spi, { read: UI.WHOAMI, expect: 0x06 });

irq!(int1) {
    on(fifo_watermark) {
        let fifo_l = read!(UI.FIFO_STATUSL, 1);
        let fifo_h = read!(UI.FIFO_STATUSH, 1);
        let fifo_len = fifo_l | ((fifo_h & 0x0f) << 8);
        let data = read!(UI.FIFO_DATA, fifo_len);
        report!(FIFO_RAW, fifo_len, data);
    }
}
```

约定：

- 第一个 `u32` 参数推荐放 FIFO 状态寄存器读出的长度, 便于 CLI 做一致性检查。
- raw bytes 参数放实际从 `FIFO_DATA` 读出的数据。
- CLI 会优先把第一个 `u32` 当作 `fifo_len`, 把第一个 bytes 参数当作 FIFO payload。

## 总线选择与探测

固件侧仍然只是通用 SPI/I2C/I3C 桥梁, 不包含任何芯片型号、WHOAMI 或地址候选知识。需要自动选择时, 在 DSL 里显式写出探测条件：

```rseq
bus_probe!(spi, {
    read: UI.WHOAMI,
    expect: 0x06,
});

bus_probe!(i2c, {
    addrs: [0x6a, 0x6b],
    read: UI.WHOAMI,
    expect: 0x06,
});
```

`bus_probe!` 会被编译成 VM 的通用 `ProbeBus` 指令：逐个候选执行 `set_bus_kind + read`, 首个满足 `(value & mask) == (expect & mask)` 的候选成为后续读写总线。

可用选项：

| 选项 | 类型 | 说明 |
| --- | --- | --- |
| `addrs` | number array | I2C 必填。7-bit 地址候选。SPI 可省略。 |
| `read` | number/register | 必填。用于探测的寄存器地址。 |
| `expect` | number | 必填。期望读值。 |
| `len` | number | 可选。读取 1..4 字节, 默认 1。 |
| `mask` | number | 可选。比较掩码, 默认按 `len` 覆盖全部位。 |
| `delay_us` | number | 可选。探测匹配后延时。 |

## FIFO 解析格式

使用 `report_format!` 描述 CLI 如何解释 `FIFO_RAW` 的 raw bytes：

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

`i16_le` 的含义：

- FIFO payload 被拆成连续 sample。
- 每个字段占 2 字节, 小端有符号 `i16`。
- 一个 sample 的字节数为 `fields.len() * 2`。
- `fields` 的顺序必须严格等于芯片 FIFO 中每个 sample 的实际字节顺序。

可用选项：

| 选项 | 类型 | 说明 |
| --- | --- | --- |
| `fields` | identifier array | 必填。每个 sample 的字段顺序。字段名只是解析标签, 不自动绑定芯片寄存器。 |
| `gyro_fields` | identifier array | 可选。哪些字段按 gyro 量程转换。字段必须存在于 `fields`。 |
| `accel_fields` | identifier array | 可选。哪些字段按 accel 量程转换。字段必须存在于 `fields`。 |
| `gyro_fs_dps` | number | 可选。gyro 满量程, 单位 dps, 默认 `4096`。 |
| `accel_fs_g` | number | 可选。accel 满量程, 单位 g, 默认 `16`。 |
| `output` | identifier | 可选。`physical_f32` 或 `raw_i16`, 默认 `physical_f32`。 |

旧的 `qmi8660_fifo6` decoder 仍保留为兼容别名, 等价于：

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

新脚本推荐使用显式 `i16_le`, 这样 FIFO 字段顺序直接体现在 DSL 中。

## Output 模式

`output: physical_f32` 会按 `gyro_fields` 和 `accel_fields` 转换物理量：

```text
gyro_rad_s = raw_i16 * gyro_fs_dps / 32768 * pi / 180
acc_m_s2   = raw_i16 * accel_fs_g * 9.80665 / 32768
```

CLI 打印单位：

- gyro: `rad/s`
- accel: `m/s^2`

示例输出：

```text
FIFO_RAW #12 frame_id=38 ts_us=155400 dt_us=10000: fifo_len=72 data_len=72 samples=6 data=[...]
  decoded(i16_le physical_f32 gyro_rad_s acc_m_s2): [0] gyro=(gx=0.012,gy=-0.004,gz=0.018) acc=(ax=0.030,ay=-0.060,az=9.790); ...
```

`output: raw_i16` 只打印原始 `i16` 计数值, 顺序同 `fields`：

```rseq
report_format!(FIFO_RAW, i16_le, {
    fields: [gx, gy, gz, ax, ay, az],
    gyro_fields: [gx, gy, gz],
    accel_fields: [ax, ay, az],
    accel_fs_g: 16,
    gyro_fs_dps: 4096,
    output: raw_i16,
});
```

示例输出：

```text
FIFO_RAW #12 frame_id=38 ts_us=155400 dt_us=10000: fifo_len=72 data_len=72 samples=6 data=[...]
  decoded(i16_le raw_i16): [0] raw=(gx=12,gy=-4,gz=18,ax=63,ay=-126,az=20442); ...
```

## Watch 模式

如果 MCU 已经在运行, CLI 可以只接收上报数据, 不发送 `LOAD`、`EXEC`、`PING` 或其他控制帧：

```bash
cargo run -q -p rseq-cli --features serial -- \
  -f examples/qmi8660_fifo.rseq \
  --watch \
  --serial /dev/cu.usbmodem314201 \
  --baud 115200
```

`-f` 或 `--manifest` 在 watch 模式下只用于加载 `report_format!` 元数据。即使传入了 `--execute` 等控制选项, watch 模式也不会对 MCU 发控制命令。

### 保存与回放

watch 或普通下发后观察模式可以保存 report：

```bash
cargo run -q -p rseq-cli --features serial -- \
  -f examples/qmi8660_fifo.rseq \
  --watch \
  --serial /dev/cu.usbmodem314201 \
  --save fifo.csv

cargo run -q -p rseq-cli --features serial -- \
  -f examples/qmi8660_fifo.rseq \
  --watch \
  --serial /dev/cu.usbmodem314201 \
  --save fifo.bin
```

- `.csv` 使用固定长表表头：一行一个解码字段, 包含 `seq/kind/frame_id/timestamp_us/dt_us/fifo_len/data_len/sample_index/field/raw_i16/value/unit` 等列。
- `.bin` 保存完整 report 记录, 可离线回放：

```bash
cargo run -q -p rseq-cli -- \
  -f examples/qmi8660_fifo.rseq \
  --replay fifo.bin
```

如果需要抢回正在持续上报的 MCU, 使用 Stop 控制帧：

```bash
cargo run -q -p rseq-cli --features serial -- \
  --serial /dev/cu.usbmodem314201 \
  --stop
```

`--stop` 会清除 MCU 侧后台 IRQ handler 和 pending 标志, 等待 ACK 时会丢弃旧 Trace, 用于 FIFO report 很密集时恢复控制。`--reset-mcu` 则进一步清空已加载主程序。

## 连续性与健康检查

每条 report Trace v2 都会携带链路元信息：

| 字段 | 含义 |
| --- | --- |
| `frame_id` | MCU 侧 report 序号。CLI 用它检测是否丢 report。 |
| `ts_us` | MCU 侧微秒时间戳。底层提供单调时钟时有效。 |
| `dt_us` | 当前 report 与上一条有效时间戳的间隔。 |
| `frame_gap=N` | 上一条到当前条之间缺失了 `N` 个 report。 |
| `frame_id_reset=A->B` | `frame_id` 回退, 通常表示 MCU 重启、计数器回绕或重新连接。 |
| `ts_rewind=A->B` | 时间戳回退, 通常表示 MCU 重启或时钟源异常。 |

FIFO 解析还会打印数据健康提示：

| 字段 | 含义 |
| --- | --- |
| `len_mismatch=status:X,data:Y` | `report!` 上报的 `fifo_len` 与实际 bytes 长度不同。 |
| `partial_bytes=N` | FIFO payload 不能被 sample 字节数整除, 剩余 `N` 字节无法组成完整 sample。 |

如果持续看到 `frame_gap`, 说明上报链路或主机接收端跟不上。若持续看到 `partial_bytes` 或物理量明显不合理, 优先检查 `fields` 顺序、FIFO 配置和量程参数。

CLI 默认每 100 条 report 打印一次累计健康摘要：

```text
report health: total=100 kinds=[FIFO_RAW=100] dropped=0 frame_resets=0 ts_rewinds=0 fifo_bytes=7200 fifo_samples=600 fifo_len_mismatch=0 fifo_partial_reports=0 fifo_partial_bytes=0
```

可以用 `--stats-every N` 调整周期, `--stats-every 0` 关闭周期摘要。

## 示例脚本

当前 QMI8660 FIFO 示例位于：

```text
examples/qmi8660_fifo.rseq
```

调试时可以在 `physical_f32` 和 `raw_i16` 之间切换 `output`：

- 用 `physical_f32` 看人类可读的 `rad/s` 与 `m/s^2`。
- 用 `raw_i16` 对照逻辑分析仪、芯片手册或驱动源码里的原始 FIFO 排列。
