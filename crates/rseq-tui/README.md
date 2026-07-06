# rseq-tui

Terminal dashboard demo for rseq report streams.

It provides four tabs:

- Motion: live acc and gyro charts.
- Reports: recent report frames.
- Registers: latest traced register reads/writes.
- Logs: link and source messages.

Run the synthetic demo:

```bash
cargo run -p rseq-tui -- --demo
```

Load an rseq file into the MCU, execute it, then watch the report stream:

```bash
cargo run -p rseq-tui --features serial -- \
  --serial /dev/cu.usbmodem314201 \
  --baud 115200 \
  --chip qmi8660.yaml \
  -f examples/qmi8660_fifo.rseq
```

Watch an already running MCU without sending LOAD/EXEC frames:

```bash
cargo run -p rseq-tui --features serial -- \
  --watch \
  --serial /dev/cu.usbmodem314201 \
  --baud 115200 \
  --chip qmi8660.yaml \
  -f examples/qmi8660_fifo.rseq
```

The TUI reads `report_format!` metadata from `-f` files and currently supports `i16_le` plus the
legacy `qmi8660_fifo6` alias. `FIFO_RAW` samples are converted to acc `m/s^2` and gyro `rad/s` for
the chart tab.

The Registers tab reads chip metadata from `--chip` or from `chip!(...)` statements in the `-f`
files. It renders a 16-column register map, marks `no_dump` registers as `??`, and shows YAML field
details for the selected register.

In the Registers tab, move the selected cell with the arrow keys and press `r` to actively dump the
selected register. This sends a `Control` BusRead frame to the MCU, so it does not replace the
currently loaded rseq program or clear background IRQ/report handlers. Registers marked `no_dump` in
the YAML are never actively read and remain `??`.
