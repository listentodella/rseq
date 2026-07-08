#!/usr/bin/env python3
"""Build a double-clickable Windows distribution folder for rseq-gpui."""

from __future__ import annotations

import argparse
import shutil
import subprocess
from pathlib import Path


DEFAULT_TARGET = "x86_64-pc-windows-msvc"


def find_repo_root() -> Path:
    path = Path(__file__).resolve()
    for parent in [path.parent, *path.parents]:
        if (parent / "Cargo.toml").is_file() and (parent / "crates" / "rseq-gpui").is_dir():
            return parent
    raise SystemExit("could not find rseq repository root")


ROOT = find_repo_root()


def run(cmd: list[str]) -> None:
    print("+", " ".join(cmd), flush=True)
    subprocess.run(cmd, cwd=ROOT, check=True)


def copy_tree(src: Path, dst: Path) -> None:
    if dst.exists():
        shutil.rmtree(dst)
    shutil.copytree(
        src,
        dst,
        ignore=shutil.ignore_patterns(".DS_Store", "*.tmp", "target"),
    )


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--release", action="store_true", default=True, help="build release binary")
    parser.add_argument("--debug", action="store_true", help="build debug binary instead of release")
    parser.add_argument(
        "--target",
        default=DEFAULT_TARGET,
        help=f"Rust target triple, default: {DEFAULT_TARGET}",
    )
    parser.add_argument("--no-build", action="store_true", help="reuse an existing target binary")
    parser.add_argument("--out", default="dist", help="output directory, default: dist")
    parser.add_argument("--no-zip", action="store_true", help="do not create a zip archive")
    args = parser.parse_args()

    profile = "debug" if args.debug else "release"
    cargo_cmd = [
        "cargo",
        "build",
        "-p",
        "rseq-gpui",
        "--features",
        "serial",
        "--target",
        args.target,
    ]
    if profile == "release":
        cargo_cmd.append("--release")
    if not args.no_build:
        run(cargo_cmd)

    binary_dir = ROOT / "target" / args.target / profile
    binary = binary_dir / "rseq-gpui.exe"
    if not binary.exists():
        raise SystemExit(f"missing built binary: {binary}")

    out_root = ROOT / args.out
    package = out_root / f"rseq-gpui-windows-{args.target}"
    if package.exists():
        shutil.rmtree(package)
    package.mkdir(parents=True)

    shutil.copy2(binary, package / "rseq-gpui.exe")
    for dll in sorted(binary_dir.glob("*.dll")):
        shutil.copy2(dll, package / dll.name)

    for file_name in ["qmi8660.yaml", "README.md", "README-zh.md", "BUILD.md"]:
        src = ROOT / file_name
        if src.exists():
            shutil.copy2(src, package / file_name)
    examples = ROOT / "examples"
    if examples.exists():
        copy_tree(examples, package / "examples")

    (package / "run-demo.cmd").write_text(
        "@echo off\r\n"
        "cd /d %~dp0\r\n"
        "start \"Rseq GPUI\" \"%~dp0rseq-gpui.exe\" --demo --chip qmi8660.yaml\r\n",
        encoding="utf-8",
    )
    windows_readme = (
        "Rseq GPUI Windows package\r\n"
        "\r\n"
        "Double-click rseq-gpui.exe to launch the workstation.\r\n"
        "Double-click run-demo.cmd to launch demo mode with bundled qmi8660.yaml.\r\n"
        "\r\n"
        "Example serial command from PowerShell:\r\n"
        ".\\rseq-gpui.exe --serial COM3 --baud 115200 --chip qmi8660.yaml -f examples\\qmi8660_fifo.rseq\r\n"
        "\r\n"
        "Example TCP command from PowerShell:\r\n"
        ".\\rseq-gpui.exe --tcp 10.2.8.42:5657 --chip qmi8660.yaml -f examples\\qmi8660_fifo.rseq\r\n"
    )
    (package / "README-WINDOWS.txt").write_text(windows_readme, encoding="utf-8")

    print()
    print(f"Created {package}")
    if not args.no_zip:
        archive_base = out_root / package.name
        archive = shutil.make_archive(str(archive_base), "zip", root_dir=out_root, base_dir=package.name)
        print(f"Created {archive}")


if __name__ == "__main__":
    main()
