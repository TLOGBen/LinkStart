#!/usr/bin/env python3
"""Deterministic LinkStart release bundle assembly and validation.

The script deliberately validates cross-platform binaries from their file
formats. It never manufactures a missing target and never trusts a checksum
record without recomputing it.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import stat
import struct
import sys
import zipfile
from pathlib import Path
from typing import Any


TARGETS = {
    "linux-x64-musl": {
        "binary": "linkstart",
        "rust_targets": ["x86_64-unknown-linux-musl"],
        "runner_os": "Linux",
    },
    "windows-x64": {
        "binary": "linkstart.exe",
        "rust_targets": ["x86_64-pc-windows-msvc"],
        "runner_os": "Windows",
    },
    "macos-universal": {
        "binary": "linkstart",
        "rust_targets": ["x86_64-apple-darwin", "aarch64-apple-darwin"],
        "runner_os": "macOS",
    },
}
SOURCE_REPOSITORY = "https://github.com/TLOGBen/LinkStart"
MANIFEST_KEYS = {
    "schemaVersion",
    "runtimeVersion",
    "protocolMajor",
    "releaseTag",
    "artifacts",
}
ARTIFACT_KEYS = {
    "target",
    "path",
    "size",
    "sha256",
    "sourceRepository",
    "sourceTag",
    "sourceCommit",
    "workflowRun",
}
SEMVER_RE = re.compile(r"^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:[-+][0-9A-Za-z.-]+)?$")
SHA_RE = re.compile(r"^[0-9a-f]{40}$")
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")


class ReleaseError(RuntimeError):
    pass


def fail(message: str) -> None:
    raise ReleaseError(message)


def read_json(path: Path) -> Any:
    try:
        with path.open("r", encoding="utf-8") as handle:
            return json.load(handle)
    except (OSError, json.JSONDecodeError) as exc:
        fail(f"cannot read valid JSON {path}: {exc}")


def write_json(path: Path, value: Any) -> None:
    path.write_text(
        json.dumps(value, ensure_ascii=False, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )


def digest(path: Path) -> str:
    value = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            value.update(chunk)
    return value.hexdigest()


def validate_version(version: str, protocol_major: str, source_tag: str | None) -> None:
    if not SEMVER_RE.fullmatch(version):
        fail(f"invalid runtime semver: {version!r}")
    if not re.fullmatch(r"v[1-9]\d*", protocol_major):
        fail(f"invalid protocol major: {protocol_major!r}")
    if source_tag is not None and source_tag != f"v{version}":
        fail(f"tag/version mismatch: tag={source_tag!r}, version={version!r}")


def validate_version_json(path: Path, version: str, protocol_major: str) -> dict[str, Any]:
    value = read_json(path)
    if not isinstance(value, dict):
        fail(f"version output must be a JSON object: {path}")
    if value.get("version") != version or value.get("protocolMajor") != protocol_major:
        fail(
            "wrong version --json output: "
            f"expected {version}/{protocol_major}, got "
            f"{value.get('version')!r}/{value.get('protocolMajor')!r}"
        )
    return value


def validate_elf_static(path: Path) -> str:
    data = path.read_bytes()
    if len(data) < 64 or data[:4] != b"\x7fELF":
        fail(f"linux artifact is not ELF: {path}")
    elf_class, byte_order = data[4], data[5]
    if elf_class != 2 or byte_order not in (1, 2):
        fail(f"linux artifact must be 64-bit ELF: {path}")
    endian = "<" if byte_order == 1 else ">"
    machine = struct.unpack_from(endian + "H", data, 18)[0]
    if machine != 62:
        fail(f"linux artifact is not x86_64 ELF: machine={machine}")
    program_offset = struct.unpack_from(endian + "Q", data, 32)[0]
    entry_size = struct.unpack_from(endian + "H", data, 54)[0]
    entry_count = struct.unpack_from(endian + "H", data, 56)[0]
    if not entry_size or program_offset + entry_size * entry_count > len(data):
        fail(f"invalid ELF program header table: {path}")
    for index in range(entry_count):
        p_type = struct.unpack_from(endian + "I", data, program_offset + index * entry_size)[0]
        if p_type == 3:  # PT_INTERP means a dynamic loader is required.
            fail(f"linux artifact is dynamically linked (PT_INTERP present): {path}")
    return "elf64-x86_64-no-pt-interp"


def validate_pe_x64(path: Path) -> str:
    data = path.read_bytes()
    if len(data) < 0x40 or data[:2] != b"MZ":
        fail(f"windows artifact is not PE: {path}")
    pe_offset = struct.unpack_from("<I", data, 0x3C)[0]
    if pe_offset + 6 > len(data) or data[pe_offset : pe_offset + 4] != b"PE\0\0":
        fail(f"windows artifact has invalid PE header: {path}")
    machine = struct.unpack_from("<H", data, pe_offset + 4)[0]
    if machine != 0x8664:
        fail(f"windows artifact is not x86_64 MSVC-compatible PE: machine={machine:#x}")
    return "pe32plus-x86_64"


def validate_macos_universal(path: Path) -> str:
    data = path.read_bytes()
    if len(data) < 8:
        fail(f"macOS artifact is too small: {path}")
    magic = data[:4]
    if magic == b"\xca\xfe\xba\xbe":
        endian, is_64 = ">", False
    elif magic == b"\xbe\xba\xfe\xca":
        endian, is_64 = "<", False
    elif magic == b"\xca\xfe\xba\xbf":
        endian, is_64 = ">", True
    elif magic == b"\xbf\xba\xfe\xca":
        endian, is_64 = "<", True
    else:
        fail(f"macOS artifact is not a universal Mach-O binary: {path}")
    count = struct.unpack_from(endian + "I", data, 4)[0]
    entry_size = 32 if is_64 else 20
    if count < 2 or 8 + count * entry_size > len(data):
        fail(f"invalid universal Mach-O architecture table: {path}")
    cpu_types = {
        struct.unpack_from(endian + "I", data, 8 + index * entry_size)[0]
        for index in range(count)
    }
    required = {0x01000007, 0x0100000C}  # CPU_TYPE_X86_64, CPU_TYPE_ARM64
    if not required.issubset(cpu_types):
        fail(f"macOS artifact lacks x86_64 and arm64 slices: {path}")
    return "mach-o-universal-x86_64-arm64"


def validate_binary(path: Path, target: str) -> str:
    if not path.is_file() or path.is_symlink():
        fail(f"missing or unsafe binary for {target}: {path}")
    if path.stat().st_size == 0:
        fail(f"empty binary for {target}: {path}")
    if target == "linux-x64-musl":
        return validate_elf_static(path)
    if target == "windows-x64":
        return validate_pe_x64(path)
    if target == "macos-universal":
        return validate_macos_universal(path)
    fail(f"unknown target: {target}")
    raise AssertionError("unreachable")


def validate_commit(value: str) -> None:
    if not SHA_RE.fullmatch(value):
        fail(f"source commit must be a full lowercase Git SHA: {value!r}")


def validate_workflow_run(value: str) -> None:
    pattern = (
        r"^https://github\.com/TLOGBen/LinkStart/actions/runs/"
        r"[1-9]\d*/attempts/[1-9]\d*$"
    )
    if not isinstance(value, str) or not re.fullmatch(pattern, value):
        fail(f"invalid workflow run provenance: {value!r}")


def bundle(args: argparse.Namespace) -> None:
    target = args.target
    if target not in TARGETS:
        fail(f"unsupported target: {target}")
    source_tag = args.source_tag or None
    validate_version(args.version, args.protocol_major, source_tag)
    validate_commit(args.source_commit)
    if args.source_repository != SOURCE_REPOSITORY:
        fail(f"unexpected source repository: {args.source_repository}")
    validate_workflow_run(args.workflow_run)
    version_json = validate_version_json(Path(args.version_json), args.version, args.protocol_major)
    binary = Path(args.binary).resolve()
    format_check = validate_binary(binary, target)
    expected = TARGETS[target]
    rust_targets = sorted(set(args.rust_target))
    if rust_targets != sorted(expected["rust_targets"]):
        fail(f"wrong Rust targets for {target}: {rust_targets}")
    if args.runner_os != expected["runner_os"]:
        fail(f"wrong runner OS for {target}: {args.runner_os}")
    output = Path(args.output)
    if output.exists():
        fail(f"bundle output already exists: {output}")
    output.mkdir(parents=True)
    binary_name = expected["binary"]
    copied = output / binary_name
    shutil.copyfile(binary, copied)
    if target != "windows-x64":
        copied.chmod(copied.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
    provenance = {
        "schemaVersion": 1,
        "target": target,
        "binaryName": binary_name,
        "binarySize": copied.stat().st_size,
        "binarySha256": digest(copied),
        "binaryFormatCheck": format_check,
        "runtimeVersion": args.version,
        "protocolMajor": args.protocol_major,
        "versionJson": version_json,
        "sourceCommit": args.source_commit,
        "sourceTag": source_tag,
        "sourceRepository": args.source_repository,
        "workflowRun": args.workflow_run,
        "runner": {"os": args.runner_os, "arch": args.runner_arch},
        "rustcVersion": args.rustc_version,
        "rustTargets": rust_targets,
    }
    write_json(output / "provenance.json", provenance)
    print(json.dumps(provenance, ensure_ascii=False, sort_keys=True))


def validate_bundle(path: Path, target: str, args: argparse.Namespace) -> tuple[Path, dict[str, Any]]:
    expected = TARGETS[target]
    allowed = {expected["binary"], "provenance.json"}
    names = {item.name for item in path.iterdir()} if path.is_dir() else set()
    if names != allowed:
        fail(f"bundle {target} must contain exactly {sorted(allowed)}, got {sorted(names)}")
    binary = path / expected["binary"]
    provenance = read_json(path / "provenance.json")
    if not isinstance(provenance, dict) or provenance.get("target") != target:
        fail(f"wrong or missing provenance target in {path}")
    actual_format = validate_binary(binary, target)
    if provenance.get("binaryName") != expected["binary"]:
        fail(f"wrong binary name in provenance for {target}")
    if provenance.get("binarySize") != binary.stat().st_size:
        fail(f"binary size mismatch for {target}")
    actual_sha = digest(binary)
    if provenance.get("binarySha256") != actual_sha:
        fail(f"checksum mismatch for {target}")
    if provenance.get("binaryFormatCheck") != actual_format:
        fail(f"binary format provenance mismatch for {target}")
    if not SHA256_RE.fullmatch(actual_sha):
        fail(f"invalid computed SHA-256 for {target}")
    if provenance.get("sourceCommit") != args.source_commit:
        fail(f"source commit mismatch for {target}")
    if provenance.get("sourceTag") != (args.source_tag or None):
        fail(f"source tag mismatch for {target}")
    if provenance.get("sourceRepository") != SOURCE_REPOSITORY:
        fail(f"source repository mismatch for {target}")
    if provenance.get("sourceRepository") != args.source_repository:
        fail(f"bundle source repository does not match assembly input for {target}")
    if provenance.get("workflowRun") != args.workflow_run:
        fail(f"workflow run provenance mismatch for {target}")
    validate_workflow_run(provenance.get("workflowRun"))
    if provenance.get("runtimeVersion") != args.version:
        fail(f"runtime version mismatch for {target}")
    if provenance.get("protocolMajor") != args.protocol_major:
        fail(f"protocol major mismatch for {target}")
    version_value = provenance.get("versionJson")
    if not isinstance(version_value, dict) or version_value.get("version") != args.version:
        fail(f"wrong recorded version --json for {target}")
    if version_value.get("protocolMajor") != args.protocol_major:
        fail(f"wrong recorded protocol major for {target}")
    if provenance.get("rustTargets") != sorted(expected["rust_targets"]):
        fail(f"target provenance mismatch for {target}")
    runner = provenance.get("runner")
    if not isinstance(runner, dict) or runner.get("os") != expected["runner_os"]:
        fail(f"runner provenance mismatch for {target}")
    if not isinstance(runner.get("arch"), str) or not runner["arch"]:
        fail(f"runner architecture provenance is missing for {target}")
    if not isinstance(provenance.get("rustcVersion"), str) or not provenance["rustcVersion"].startswith("rustc "):
        fail(f"rustc provenance is missing for {target}")
    return binary, provenance


def assemble(args: argparse.Namespace) -> None:
    source_tag = args.source_tag or None
    validate_version(args.version, args.protocol_major, source_tag)
    if source_tag is None:
        fail("assembly requires a genuine release source tag")
    if args.source_repository != SOURCE_REPOSITORY:
        fail(f"unexpected source repository: {args.source_repository}")
    validate_workflow_run(args.workflow_run)
    validate_commit(args.source_commit)
    input_dir = Path(args.input)
    if not input_dir.is_dir():
        fail(f"input directory does not exist: {input_dir}")
    target_dirs = [item.name for item in input_dir.iterdir() if item.is_dir()]
    if sorted(target_dirs) != sorted(TARGETS):
        fail(f"missing, extra, or duplicate target bundles: {sorted(target_dirs)}")
    output = Path(args.output)
    if output.exists():
        fail(f"assembly output already exists: {output}")
    assets = output / "assets"
    records: list[dict[str, Any]] = []
    for target in sorted(TARGETS):
        binary, provenance = validate_bundle(input_dir / target, target, args)
        relative = Path("bin") / target / TARGETS[target]["binary"]
        destination = assets / relative
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(binary, destination)
        if target != "windows-x64":
            destination.chmod(0o755)
        records.append(
            {
                "target": target,
                "path": relative.as_posix(),
                "size": destination.stat().st_size,
                "sha256": digest(destination),
                "sourceRepository": args.source_repository,
                "sourceTag": source_tag,
                "sourceCommit": args.source_commit,
                "workflowRun": provenance["workflowRun"],
            }
        )
    manifest = {
        "schemaVersion": 1,
        "runtimeVersion": args.version,
        "protocolMajor": args.protocol_major,
        "releaseTag": source_tag,
        "artifacts": records,
    }
    write_json(assets / "checksums.json", manifest)
    validate_manifest_file(assets / "checksums.json", output)
    print(json.dumps(manifest, ensure_ascii=False, sort_keys=True))


def validate_manifest_file(manifest_path: Path, root: Path) -> None:
    manifest = read_json(manifest_path)
    if (
        not isinstance(manifest, dict)
        or set(manifest) != MANIFEST_KEYS
        or manifest.get("schemaVersion") != 1
    ):
        fail("unsupported checksums.json schema")
    records = manifest.get("artifacts")
    if not isinstance(records, list):
        fail("checksums.json artifacts must be an array")
    validate_version(
        manifest.get("runtimeVersion", ""),
        manifest.get("protocolMajor", ""),
        manifest.get("releaseTag"),
    )
    by_target: dict[str, dict[str, Any]] = {}
    for record in records:
        if not isinstance(record, dict) or record.get("target") not in TARGETS:
            fail("checksums.json contains an invalid target record")
        target = record["target"]
        if set(record) != ARTIFACT_KEYS:
            fail(f"invalid artifact record keys for {target}")
        if target in by_target:
            fail(f"checksums.json contains duplicate target: {target}")
        by_target[target] = record
    if set(by_target) != set(TARGETS):
        fail(f"checksums.json targets mismatch: {sorted(by_target)}")
    for target, record in by_target.items():
        expected_path = (Path("bin") / target / TARGETS[target]["binary"]).as_posix()
        if record.get("path") != expected_path:
            fail(f"wrong relative path for {target}")
        binary = root / "assets" / Path(expected_path)
        if not binary.is_file() or binary.is_symlink():
            fail(f"manifest binary is missing or unsafe: {binary}")
        validate_binary(binary, target)
        if record.get("size") != binary.stat().st_size or record.get("sha256") != digest(binary):
            fail(f"manifest size or checksum mismatch for {target}")
        if record.get("sourceRepository") != SOURCE_REPOSITORY:
            fail(f"manifest source repository mismatch for {target}")
        if record.get("sourceTag") != manifest["releaseTag"]:
            fail(f"manifest source tag mismatch for {target}")
        validate_commit(record.get("sourceCommit", ""))
        validate_workflow_run(record.get("workflowRun"))


def validate_manifest(args: argparse.Namespace) -> None:
    manifest = Path(args.manifest).resolve()
    root = manifest.parent.parent
    validate_manifest_file(manifest, root)
    print(json.dumps({"valid": True, "manifest": str(manifest)}, sort_keys=True))


def archive(args: argparse.Namespace) -> None:
    root = Path(args.root).resolve()
    manifest = root / "assets" / "checksums.json"
    validate_manifest_file(manifest, root)
    output = Path(args.output)
    if output.exists():
        fail(f"archive output already exists: {output}")
    output.parent.mkdir(parents=True, exist_ok=True)
    files = sorted(path for path in root.rglob("*") if path.is_file())
    with zipfile.ZipFile(output, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as bundle_file:
        for path in files:
            relative = path.relative_to(root).as_posix()
            info = zipfile.ZipInfo(relative, date_time=(1980, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = (0o755 if path.name == "linkstart" else 0o644) << 16
            bundle_file.writestr(info, path.read_bytes())
    print(json.dumps({"archive": str(output), "sha256": digest(output)}, sort_keys=True))


def parser() -> argparse.ArgumentParser:
    command = argparse.ArgumentParser(description=__doc__)
    sub = command.add_subparsers(dest="command", required=True)

    bundle_cmd = sub.add_parser("bundle", help="validate and record one native build")
    bundle_cmd.add_argument("--binary", required=True)
    bundle_cmd.add_argument("--target", required=True, choices=sorted(TARGETS))
    bundle_cmd.add_argument("--output", required=True)
    bundle_cmd.add_argument("--version", required=True)
    bundle_cmd.add_argument("--protocol-major", required=True)
    bundle_cmd.add_argument("--version-json", required=True)
    bundle_cmd.add_argument("--source-commit", required=True)
    bundle_cmd.add_argument("--source-tag", default="")
    bundle_cmd.add_argument("--source-repository", required=True)
    bundle_cmd.add_argument("--workflow-run", required=True)
    bundle_cmd.add_argument("--runner-os", required=True)
    bundle_cmd.add_argument("--runner-arch", required=True)
    bundle_cmd.add_argument("--rustc-version", required=True)
    bundle_cmd.add_argument("--rust-target", action="append", required=True)
    bundle_cmd.set_defaults(function=bundle)

    assemble_cmd = sub.add_parser("assemble", help="assemble all exact target bundles")
    assemble_cmd.add_argument("--input", required=True)
    assemble_cmd.add_argument("--output", required=True)
    assemble_cmd.add_argument("--version", required=True)
    assemble_cmd.add_argument("--protocol-major", required=True)
    assemble_cmd.add_argument("--source-commit", required=True)
    assemble_cmd.add_argument("--source-tag", required=True)
    assemble_cmd.add_argument("--source-repository", required=True)
    assemble_cmd.add_argument("--workflow-run", required=True)
    assemble_cmd.set_defaults(function=assemble)

    validate_cmd = sub.add_parser("validate", help="validate an assembled checksums.json")
    validate_cmd.add_argument("--manifest", required=True)
    validate_cmd.set_defaults(function=validate_manifest)

    archive_cmd = sub.add_parser("archive", help="create a deterministic plugin-assets ZIP")
    archive_cmd.add_argument("--root", required=True)
    archive_cmd.add_argument("--output", required=True)
    archive_cmd.set_defaults(function=archive)
    return command


def main() -> int:
    try:
        args = parser().parse_args()
        args.function(args)
        return 0
    except ReleaseError as exc:
        print(f"release validation failed: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
