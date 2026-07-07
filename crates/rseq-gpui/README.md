# rseq-gpui

`rseq-gpui` is a GPUI workstation for the `rseq` MCU link. It uses the same `.rseq`
DSL and `rseq-link` frame protocol as the CLI/TUI tools.

## Run

Load a script, execute it on the MCU, and keep observing reports:

```bash
cargo run -p rseq-gpui --features serial -- \
  --serial /dev/cu.usbmodem314201 \
  --baud 115200 \
  --chip qmi8660.yaml \
  -f examples/qmi8660_fifo.rseq
```

Attach to an MCU that is already running without sending LOAD/EXEC frames:

```bash
cargo run -p rseq-gpui --features serial -- \
  --watch \
  --serial /dev/cu.usbmodem314201 \
  --baud 115200 \
  --chip qmi8660.yaml \
  -f examples/qmi8660_fifo.rseq
```

Multiple `-f/--file` arguments are accepted. They are parsed as separate source
units, keep their own relative `chip!(...)` base directories and diagnostics, and
are compiled into one LOAD/EXEC program in the order supplied.

Run without hardware:

```bash
cargo run -p rseq-gpui -- --demo --chip qmi8660.yaml
```

## UI

- Motion: live accelerometer (`m/s^2`) and gyroscope (`rad/s`) charts.
- Reports: decoded report stream with frame/timestamp/drop statistics.
- Registers: page-aware Matrix and Register Map views. Hover decodes YAML
  bitfields with live-value highlighting, `??` means `no_dump`, and double-click
  edits a writable register in place through observing control frames.
- Sequences: open, edit, save, compile, watch, and load/run `.rseq` as the active
  command sequence. It has two views:
  - Text: the native `.rseq` editor for full DSL features such as `irq!`,
    `report_format!`, `if`, `repeat!`, and custom expressions.
  - Blocks: a graphical register-sequence editor with left-side categories and
    row-based Read/Write steps. Rows expose address, read length, write data, and
    delay-us inputs, then generate ordinary `.rseq` source. The Blocks view uses
    chip YAML metadata for basic read/write safety checks, but does not replace
    the native DSL for advanced interrupt/report logic.
  The sidebar also selects chip YAML metadata when it is not provided by
  `chip!(...)`. The compiler path is the same as CLI/TUI, including `chip!` and
  `report_format!` metadata collection.
- Logs: link, LOAD/EXEC, control, and trace messages.

The `Connect` button starts the default mode derived from the command line:
`--watch` or missing startup bytecode opens an observing session; otherwise it
loads and runs the compiled `.rseq`. If the Sequences editor is active or dirty,
the toolbar actions use the editor buffer; otherwise they use the startup file
from the command line. `Watch` always opens an observing session, and `Load & Run`
always sends LOAD/EXEC when a compiled program is available.

The Sequences tab has `Open` buttons backed by the system file picker. You can
launch the app without `-f`, choose a local `.rseq` file and chip YAML in the UI,
press `Compile`, then `Load & Run`. In Blocks view, `Apply To Text` materializes
the generated `.rseq` in the Text view, while `Run Active` compiles and sends only
the selected category.

## Report Decoding

Report decoding metadata comes from `report_format!` in the loaded `.rseq` file,
not from hardcoded chip logic. For FIFO samples:

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

`output: physical_f32` drives Motion charts. `output: raw_i16` is accepted by the
host decoder for textual/report workflows, but Motion still needs gyro and accel
field groups to produce physical samples.

If the FIFO sample also carries temperature, add a temperature field and scaling
metadata. The Motion page will show the temperature panel only after it receives
samples with `temp_c`, and the panel can be hidden from the UI:

```rseq
report_format!(FIFO_RAW, i16_le, {
    fields: [gx, gy, gz, ax, ay, az, temp],
    gyro_fields: [gx, gy, gz],
    accel_fields: [ax, ay, az],
    temp_field: temp,
    accel_fs_g: 16,
    gyro_fs_dps: 4096,
    temp_lsb_per_c: 256,
    output: physical_f32,
});
```
