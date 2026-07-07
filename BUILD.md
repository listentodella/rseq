# Cross-Platform Build Notes

The workspace should not require machine-local absolute paths. Host tools build
with normal Cargo commands, while the GPUI workstation pins public git
dependencies for `gpui` and `gpui-component`.

## macOS

```sh
cargo check -p rseq-cli -p rseq-host -p rseq-link -p rseq-lsp --features serial
cargo check -p rseq-gpui --features serial
```

Serial ports are usually named `/dev/cu.usbmodem*` or `/dev/cu.usbserial*`.

## Windows 11

Install:

- Rust stable with the `x86_64-pc-windows-msvc` target.
- Visual Studio 2022 Build Tools with the C++ workload and Windows SDK.
- Git, available from PowerShell or the Developer PowerShell.

Then run:

```powershell
rustup target add x86_64-pc-windows-msvc
cargo check -p rseq-cli -p rseq-host -p rseq-link -p rseq-lsp --features serial
cargo check -p rseq-gpui --features serial
```

Serial ports are usually named `COM3`, `COM4`, etc:

```powershell
cargo run -p rseq-gpui --features serial -- --serial COM3 --baud 115200 --chip qmi8660.yaml -f examples/qmi8660_fifo.rseq
```

Cross-checking the Windows target from macOS is not enough by itself: some
dependencies compile small C/ASM helpers and need MSVC tools such as `lib.exe`
and Windows headers.

## Linux

Install Rust and the native UI/dev packages required by GPUI. Package names vary
by distribution; on Debian/Ubuntu-like systems start with:

```sh
sudo apt install build-essential pkg-config clang libx11-dev libxcb1-dev libxkbcommon-dev libwayland-dev libssl-dev
cargo check -p rseq-cli -p rseq-host -p rseq-link -p rseq-lsp --features serial
cargo check -p rseq-gpui --features serial
```

Serial ports are usually named `/dev/ttyACM*` or `/dev/ttyUSB*`. Add the user to
the appropriate dialout/uucp group if opening the port fails with permission
errors.

## Local GPUI Component Development

The default `crates/rseq-gpui/Cargo.toml` uses:

- `gpui` / `gpui_platform` from `https://github.com/zed-industries/zed`
- `gpui-component` / assets from
  `https://github.com/longbridge/gpui-component.git`

If you need to test local component changes, or keep using component patches
that have not landed upstream yet, copy `.cargo/gpui-local.example.toml` to
`.cargo/config.toml` and adjust the relative paths. `.cargo/config.toml` is
ignored so local overrides do not leak into the shared workspace.
