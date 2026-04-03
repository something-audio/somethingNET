#!/usr/bin/env python3
from __future__ import annotations

import argparse
import os
import platform
import shutil
import stat
import subprocess
import sys
from pathlib import Path

try:
    import tomllib
except ModuleNotFoundError:  # pragma: no cover
    import tomli as tomllib


PLUGIN_NAME = "SomethingNet"
CRATE_NAME = "somethingnet_vst3"
BUNDLE_ID = "com.somethingaudio.somethingnet"
MIN_MACOS_VERSION = "11.0"


def repo_root() -> Path:
    return Path(__file__).resolve().parents[1]


def package_version(root: Path) -> str:
    cargo_toml = root / "Cargo.toml"
    data = tomllib.loads(cargo_toml.read_text())
    return str(data["package"]["version"])


def target_binary(platform_name: str, target_dir: Path) -> Path:
    if platform_name == "macos":
        return target_dir / f"lib{CRATE_NAME}.a"
    if platform_name == "windows":
        return target_dir / f"{CRATE_NAME}.dll"
    if platform_name == "linux":
        return target_dir / f"lib{CRATE_NAME}.so"
    raise ValueError(f"unsupported platform: {platform_name}")


def ensure_clean_dir(path: Path) -> None:
    if path.exists():
        shutil.rmtree(path)
    path.mkdir(parents=True, exist_ok=True)


def write_text(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)


def package_macos(bundle_root: Path, artifact: Path, version: str) -> None:
    macos_dir = bundle_root / "Contents" / "MacOS"
    resources_dir = bundle_root / "Contents" / "Resources"
    plist_path = bundle_root / "Contents" / "Info.plist"
    pkginfo_path = bundle_root / "Contents" / "PkgInfo"
    binary_path = macos_dir / PLUGIN_NAME

    ensure_clean_dir(bundle_root)
    macos_dir.mkdir(parents=True, exist_ok=True)
    resources_dir.mkdir(parents=True, exist_ok=True)

    subprocess.run(
        [
            "clang",
            "-bundle",
            f"-Wl,-force_load,{artifact}",
            "-Wl,-exported_symbol,_GetPluginFactory",
            "-Wl,-exported_symbol,_bundleEntry",
            "-Wl,-exported_symbol,_bundleExit",
            "-Wl,-undefined,dynamic_lookup",
            "-o",
            str(binary_path),
        ],
        check=True,
    )

    write_text(
        plist_path,
        f"""<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key>
  <string>en</string>
  <key>CFBundleExecutable</key>
  <string>{PLUGIN_NAME}</string>
  <key>CFBundleIdentifier</key>
  <string>{BUNDLE_ID}</string>
  <key>CFBundleInfoDictionaryVersion</key>
  <string>6.0</string>
  <key>CFBundleName</key>
  <string>{PLUGIN_NAME}</string>
  <key>CFBundlePackageType</key>
  <string>BNDL</string>
  <key>CFBundleSignature</key>
  <string>????</string>
  <key>CFBundleShortVersionString</key>
  <string>{version}</string>
  <key>CFBundleSupportedPlatforms</key>
  <array>
    <string>MacOSX</string>
  </array>
  <key>CFBundleVersion</key>
  <string>{version}</string>
  <key>LSMinimumSystemVersion</key>
  <string>{MIN_MACOS_VERSION}</string>
</dict>
</plist>
""",
    )
    pkginfo_path.write_bytes(b"BNDL????")
    binary_path.chmod(binary_path.stat().st_mode | stat.S_IEXEC)

    subprocess.run(
        ["codesign", "--force", "--sign", "-", str(bundle_root)],
        check=False,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )


def package_windows(bundle_root: Path, artifact: Path) -> None:
    binary_dir = bundle_root / "Contents" / "x86_64-win"
    resources_dir = bundle_root / "Contents" / "Resources"
    ensure_clean_dir(bundle_root)
    binary_dir.mkdir(parents=True, exist_ok=True)
    resources_dir.mkdir(parents=True, exist_ok=True)
    shutil.copy2(artifact, binary_dir / f"{PLUGIN_NAME}.vst3")


def linux_arch_dir() -> str:
    machine = platform.machine().lower()
    if machine in {"x86_64", "amd64"}:
        return "x86_64-linux"
    if machine in {"aarch64", "arm64"}:
        return "aarch64-linux"
    return f"{machine}-linux"


def package_linux(bundle_root: Path, artifact: Path) -> None:
    binary_dir = bundle_root / "Contents" / linux_arch_dir()
    resources_dir = bundle_root / "Contents" / "Resources"
    ensure_clean_dir(bundle_root)
    binary_dir.mkdir(parents=True, exist_ok=True)
    resources_dir.mkdir(parents=True, exist_ok=True)
    shutil.copy2(artifact, binary_dir / f"{PLUGIN_NAME}.so")


def create_archive(bundle_root: Path, archive_path: Path) -> None:
    archive_path.parent.mkdir(parents=True, exist_ok=True)
    base_name = archive_path.with_suffix("")
    if archive_path.exists():
        archive_path.unlink()
    shutil.make_archive(
        str(base_name),
        "zip",
        root_dir=bundle_root.parent,
        base_dir=bundle_root.name,
    )


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Package SomethingNet as a VST3 bundle.")
    parser.add_argument("--platform", choices=["macos", "windows", "linux"], required=True)
    parser.add_argument(
        "--target-dir",
        default=str(repo_root() / "target" / "release"),
        help="Cargo target profile directory containing compiled artifacts.",
    )
    parser.add_argument(
        "--bundle-root",
        required=True,
        help="Output path for the .vst3 bundle root.",
    )
    parser.add_argument(
        "--archive",
        help="Optional zip archive path to create from the packaged bundle.",
    )
    parser.add_argument(
        "--version",
        help="Optional version string override for bundle metadata and archive naming.",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    root = repo_root()
    version = (args.version or package_version(root)).lstrip("v")
    target_dir = Path(args.target_dir).resolve()
    bundle_root = Path(args.bundle_root).resolve()

    artifact = target_binary(args.platform, target_dir)
    if not artifact.exists():
        print(f"Missing build artifact: {artifact}", file=sys.stderr)
        return 1

    if args.platform == "macos":
        package_macos(bundle_root, artifact, version)
    elif args.platform == "windows":
        package_windows(bundle_root, artifact)
    else:
        package_linux(bundle_root, artifact)

    if args.archive:
        create_archive(bundle_root, Path(args.archive).resolve())

    print(f"Packaged {bundle_root}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
