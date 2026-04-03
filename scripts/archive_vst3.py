#!/usr/bin/env python3
from __future__ import annotations

import argparse
import shutil
import sys
from pathlib import Path


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Archive an existing .vst3 bundle.")
    parser.add_argument("--bundle-root", required=True, help="Path to the .vst3 bundle root.")
    parser.add_argument("--archive", required=True, help="Output .zip archive path.")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    bundle_root = Path(args.bundle_root).resolve()
    archive = Path(args.archive).resolve()

    if not bundle_root.exists():
        print(f"Missing bundle root: {bundle_root}", file=sys.stderr)
        return 1

    archive.parent.mkdir(parents=True, exist_ok=True)
    if archive.exists():
        archive.unlink()

    shutil.make_archive(
        str(archive.with_suffix("")),
        "zip",
        root_dir=bundle_root.parent,
        base_dir=bundle_root.name,
    )
    print(f"Archived {bundle_root} -> {archive}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
