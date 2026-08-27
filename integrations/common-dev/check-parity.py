#!/usr/bin/env python3
"""Verify the LinkStart integration mirror against canonical common-dev."""

from __future__ import annotations

import argparse
from pathlib import Path
import stat


HERE = Path(__file__).resolve().parent


def files(root: Path) -> dict[Path, Path]:
    return {path.relative_to(root): path for path in root.rglob("*") if path.is_file()}


def compare(canonical: Path, mirror: Path) -> list[str]:
    errors: list[str] = []
    left = files(canonical)
    right = files(mirror)
    if set(left) != set(right):
        for rel in sorted(set(left) - set(right)):
            errors.append(f"mirror_missing: {mirror.name}/{rel}")
        for rel in sorted(set(right) - set(left)):
            errors.append(f"mirror_extra: {mirror.name}/{rel}")
    for rel in sorted(set(left) & set(right)):
        if left[rel].read_bytes() != right[rel].read_bytes():
            errors.append(f"content_mismatch: {mirror.name}/{rel}")
        if stat.S_IMODE(left[rel].stat().st_mode) != stat.S_IMODE(right[rel].stat().st_mode):
            errors.append(f"mode_mismatch: {mirror.name}/{rel}")
    return errors


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--canonical",
        type=Path,
        default=(HERE / "../../../common-dev-plugin").resolve(),
        help="common-dev-plugin repository root",
    )
    args = parser.parse_args()
    errors = compare(args.canonical / "plugins/linkstart", HERE / "plugins/linkstart")
    errors += compare(args.canonical / "codex/plugins/linkstart", HERE / "codex/plugins/linkstart")
    if errors:
        print("LINKSTART_COMMON_DEV_MIRROR_INVALID")
        for error in errors:
            print(f"ERROR {error}")
        return 2
    print("LINKSTART_COMMON_DEV_MIRROR_VALID")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
