#!/usr/bin/env python3
"""Build a double-clickable macOS .app bundle for rseq-gpui."""

from __future__ import annotations

import argparse
import os
import plistlib
import shutil
import subprocess
from pathlib import Path


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
    shutil.copytree(src, dst, ignore=shutil.ignore_patterns(".DS_Store"))


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--release", action="store_true", default=True, help="build release binary")
    parser.add_argument("--debug", action="store_true", help="build debug binary instead of release")
    parser.add_argument("--target", help="optional Rust target triple, for example aarch64-apple-darwin")
    parser.add_argument("--no-build", action="store_true", help="reuse an existing target binary")
    parser.add_argument("--out", default="dist", help="output directory, default: dist")
    args = parser.parse_args()

    profile = "debug" if args.debug else "release"
    cargo_cmd = ["cargo", "build", "-p", "rseq-gpui", "--features", "serial"]
    if profile == "release":
        cargo_cmd.append("--release")
    if args.target:
        cargo_cmd.extend(["--target", args.target])
    if not args.no_build:
        run(cargo_cmd)

    binary_dir = ROOT / "target"
    if args.target:
        binary_dir = binary_dir / args.target
    binary = binary_dir / profile / "rseq-gpui"
    if not binary.exists():
        raise SystemExit(f"missing built binary: {binary}")

    app = ROOT / args.out / "Rseq.app"
    contents = app / "Contents"
    macos = contents / "MacOS"
    resources = contents / "Resources"
    if app.exists():
        shutil.rmtree(app)
    macos.mkdir(parents=True)
    resources.mkdir(parents=True)

    bundled_binary = macos / "rseq-gpui"
    shutil.copy2(binary, bundled_binary)
    bundled_binary.chmod(bundled_binary.stat().st_mode | 0o111)

    for file_name in ["qmi8660.yaml", "README.md", "README-zh.md", "BUILD.md"]:
        src = ROOT / file_name
        if src.exists():
            shutil.copy2(src, resources / file_name)
    examples = ROOT / "examples"
    if examples.exists():
        copy_tree(examples, resources / "examples")

    plist = {
        "CFBundleDevelopmentRegion": "en",
        "CFBundleDisplayName": "Rseq",
        "CFBundleExecutable": "rseq-gpui",
        "CFBundleIdentifier": "dev.rseq.gpui",
        "CFBundleInfoDictionaryVersion": "6.0",
        "CFBundleName": "Rseq",
        "CFBundlePackageType": "APPL",
        "CFBundleShortVersionString": "0.1.0",
        "CFBundleVersion": "0.1.0",
        "LSMinimumSystemVersion": "11.0",
        "NSHighResolutionCapable": True,
        "NSPrincipalClass": "NSApplication",
    }
    with (contents / "Info.plist").open("wb") as f:
        plistlib.dump(plist, f, sort_keys=False)
    (contents / "PkgInfo").write_text("APPL????", encoding="ascii")

    print()
    print(f"Created {app}")
    print(f"Open it with: open {app}")


if __name__ == "__main__":
    os.environ.setdefault("MACOSX_DEPLOYMENT_TARGET", "11.0")
    main()
