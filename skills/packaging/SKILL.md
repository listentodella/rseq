---
name: packaging
description: >-
  rseq GPUI desktop packaging skill. Use when building or distributing rseq-gpui
  as a double-clickable macOS .app bundle or Windows 11 distribution folder/zip,
  when updating release packaging scripts, or when checking that bundled
  qmi8660.yaml/examples resources are included.
---

# Packaging — rseq-gpui Desktop Distribution

Use this skill to package `rseq-gpui` for machines that do not have Rust, Cargo,
GPUI, or the repository checkout installed.

## Scripts

- macOS: `scripts/package_macos_app.py`
- Windows 11: `scripts/package_windows_app.py`

Run scripts from the repository root unless there is a strong reason not to.
The scripts detect the repository root from their own location, so they also
work when invoked directly from inside this skill directory.

## macOS Workflow

```sh
python3 skills/packaging/scripts/package_macos_app.py
open dist/Rseq.app
```

Output:

- `dist/Rseq.app/Contents/MacOS/rseq-gpui`
- `dist/Rseq.app/Contents/Resources/qmi8660.yaml`
- `dist/Rseq.app/Contents/Resources/examples/*.rseq`

Use `--debug --no-build` only for local structure checks with an existing
`target/debug/rseq-gpui`. Release packages should use the default release build.

## Windows 11 Workflow

Run from a Developer PowerShell with Visual Studio Build Tools and Windows SDK:

```powershell
python skills\packaging\scripts\package_windows_app.py
.\dist\rseq-gpui-windows-x86_64-pc-windows-msvc\rseq-gpui.exe
```

Output:

- `dist\rseq-gpui-windows-x86_64-pc-windows-msvc\rseq-gpui.exe`
- `dist\rseq-gpui-windows-x86_64-pc-windows-msvc\qmi8660.yaml`
- `dist\rseq-gpui-windows-x86_64-pc-windows-msvc\examples\*.rseq`
- `dist\rseq-gpui-windows-x86_64-pc-windows-msvc\run-demo.cmd`
- `dist\rseq-gpui-windows-x86_64-pc-windows-msvc.zip`

Do not claim Windows packaging is verified from macOS alone. GPUI and native
dependencies require a real Windows/MSVC environment for final validation.

## Validation

Always run:

```sh
python3 -m py_compile skills/packaging/scripts/package_macos_app.py \
  skills/packaging/scripts/package_windows_app.py
cargo fmt --check
```

For macOS, also verify bundle structure:

```sh
python3 skills/packaging/scripts/package_macos_app.py --debug --no-build
find dist/Rseq.app -maxdepth 4 -type f | sort | sed -n '1,80p'
plutil -p dist/Rseq.app/Contents/Info.plist
```

## Runtime Resource Rule

Packaged apps must include the resource files expected by the UI and examples:

- `qmi8660.yaml`
- `examples/*.rseq`
- useful repository docs such as `README.md`, `README-zh.md`, and `BUILD.md`

`rseq-gpui` contains startup logic that switches the working directory to the
packaged resource directory on macOS and to the executable folder on Windows.
Keep that behavior intact when changing packaging layout.
