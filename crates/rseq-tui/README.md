# rseq-tui

Terminal dashboard demo for rseq report streams.

It provides five tabs:

- Motion: live acc and gyro charts.
- Reports: recent report frames.
- Registers: latest traced register reads/writes.
- Controls: live output-rate, filter, range, and other YAML-defined settings.
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
  --set-control accel_odr=200Hz \
  --set-control accel_lpf=preset2 \
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

Use a CDC/UART port forwarded by a remote machine as a TCP byte stream:

```bash
# remote machine attached to the board
python3 skills/serial/scripts/serial_tcp_forward.py --serial /dev/ttyACM0 --baud 115200 --listen 0.0.0.0:5657

# local workstation
cargo run -p rseq-tui -- \
  --tcp 10.2.8.42:5657 \
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
the YAML are never actively read and remain `??`. Press `Enter` or `i` to view register metadata,
then press `q` or `Esc` to close the detail view.

Press `w` on a writable register to open the write dialog. Enter hex bytes such as `12`, `0x12`,
`12 34`, or `0x1234`, then press Enter to send a `Control` BusWrite frame. Press `q` or `Esc` to
cancel the dialog without exiting the TUI. Registers marked read-only in YAML are rejected by the TUI
before a control frame is sent.

The Controls tab is populated from the chip YAML top-level `controls` list. Press `r` to read the
selected control's backing register, `[`/`]` to cycle declared options, or `Enter`/`e` to enter an
option label or numeric value. Changes use a live register read-modify-write and do not reload the
running rseq program. Repeated `--set-control NAME=VALUE` arguments apply initial values immediately
after the TUI connects; the same controls remain editable from the tab afterward.
