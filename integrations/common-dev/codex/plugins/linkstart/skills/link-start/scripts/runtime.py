#!/usr/bin/env python3
"""Resolve, verify, and start the exact skill-local LinkStart Runtime."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import stat
import subprocess
import sys
import time
from urllib.error import URLError
from urllib.request import urlopen


PROTOCOL_MAJOR = "v1"
RUNTIME_VERSION = "0.1.1"
TARGET_FILES = {
    "linux-x64-musl": "linkstart",
    "windows-x64": "linkstart.exe",
    "macos-universal": "linkstart",
}


class RuntimeErrorCode(Exception):
    def __init__(self, code: str, detail: str):
        super().__init__(detail)
        self.code = code
        self.detail = detail


def emit(payload: dict, as_json: bool, exit_code: int = 0) -> None:
    if as_json:
        print(json.dumps(payload, ensure_ascii=False, sort_keys=True))
    else:
        print(payload.get("message") or payload)
    raise SystemExit(exit_code)


def target_for(system: str | None = None, machine: str | None = None) -> str:
    system = (system or platform.system()).lower()
    machine = (machine or platform.machine()).lower()
    x64 = machine in {"x86_64", "amd64"}
    if system == "linux" and x64:
        return "linux-x64-musl"
    if system == "windows" and x64:
        return "windows-x64"
    if system == "darwin" and machine in {"x86_64", "amd64", "arm64", "aarch64"}:
        return "macos-universal"
    raise RuntimeErrorCode(
        "runtime_target_unsupported", f"unsupported execution target: {system}/{machine}"
    )


def assets_dir() -> Path:
    return Path(__file__).resolve().parent.parent / "assets"


def load_manifest(root: Path) -> dict:
    path = root / "checksums.json"
    if not path.is_file():
        raise RuntimeErrorCode("runtime_binary_missing", f"missing {path}")
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        raise RuntimeErrorCode("runtime_binary_invalid", f"invalid {path}: {exc}") from exc
    required = {"schemaVersion", "runtimeVersion", "protocolMajor", "releaseTag", "artifacts"}
    if set(data) != required or data.get("schemaVersion") != 1:
        raise RuntimeErrorCode("runtime_binary_invalid", "unsupported checksums schema")
    if data.get("protocolMajor") != PROTOCOL_MAJOR:
        raise RuntimeErrorCode("runtime_binary_invalid", "protocol major mismatch")
    if data.get("runtimeVersion") != RUNTIME_VERSION:
        raise RuntimeErrorCode("runtime_binary_invalid", "Runtime version mismatch")
    if data.get("releaseTag") != f"v{data.get('runtimeVersion')}":
        raise RuntimeErrorCode("runtime_binary_invalid", "release tag/version mismatch")
    if not isinstance(data.get("artifacts"), list):
        raise RuntimeErrorCode("runtime_binary_invalid", "artifacts must be an array")
    return data


def artifact_record(manifest: dict, target: str) -> dict:
    matches = [item for item in manifest["artifacts"] if item.get("target") == target]
    if len(matches) != 1:
        raise RuntimeErrorCode(
            "runtime_binary_missing", f"expected one checksum record for {target}"
        )
    item = matches[0]
    expected = {
        "target",
        "path",
        "size",
        "sha256",
        "sourceRepository",
        "sourceTag",
        "sourceCommit",
        "workflowRun",
    }
    if set(item) != expected:
        raise RuntimeErrorCode("runtime_binary_invalid", f"invalid artifact record for {target}")
    expected_path = f"bin/{target}/{TARGET_FILES[target]}"
    if item["path"] != expected_path:
        raise RuntimeErrorCode("runtime_binary_invalid", f"unexpected artifact path for {target}")
    if item["sourceRepository"] != "https://github.com/TLOGBen/LinkStart":
        raise RuntimeErrorCode("runtime_binary_invalid", "unexpected build source repository")
    if item["sourceTag"] != manifest["releaseTag"]:
        raise RuntimeErrorCode("runtime_binary_invalid", "artifact source tag mismatch")
    if not isinstance(item["size"], int) or item["size"] <= 0:
        raise RuntimeErrorCode("runtime_binary_invalid", "artifact size must be positive")
    digest = item["sha256"]
    if not isinstance(digest, str) or len(digest) != 64 or any(c not in "0123456789abcdef" for c in digest):
        raise RuntimeErrorCode("runtime_binary_invalid", "artifact SHA-256 is invalid")
    return item


def verify(root: Path | None = None) -> dict:
    root = root or assets_dir()
    target = target_for()
    manifest = load_manifest(root)
    item = artifact_record(manifest, target)
    binary = root / item["path"]
    if not binary.is_file():
        raise RuntimeErrorCode("runtime_binary_missing", f"missing {binary}")
    size = binary.stat().st_size
    digest = hashlib.sha256(binary.read_bytes()).hexdigest()
    if size != item["size"] or digest != item["sha256"]:
        raise RuntimeErrorCode("runtime_binary_invalid", "artifact size or SHA-256 mismatch")
    if os.name != "nt" and not (binary.stat().st_mode & stat.S_IXUSR):
        raise RuntimeErrorCode("runtime_binary_invalid", "bundled Unix artifact is not executable")
    try:
        proc = subprocess.run(
            [str(binary), "version", "--json"],
            check=False,
            capture_output=True,
            text=True,
            timeout=10,
        )
        version = json.loads(proc.stdout)
    except (OSError, subprocess.TimeoutExpired, json.JSONDecodeError) as exc:
        raise RuntimeErrorCode("runtime_binary_invalid", f"version probe failed: {exc}") from exc
    if proc.returncode != 0:
        raise RuntimeErrorCode("runtime_binary_invalid", "version probe returned non-zero")
    if version.get("version") != manifest["runtimeVersion"] or version.get("protocolMajor") != PROTOCOL_MAJOR:
        raise RuntimeErrorCode("runtime_binary_invalid", "binary version/protocol mismatch")
    return {
        "ok": True,
        "target": target,
        "path": str(binary),
        "runtimeVersion": manifest["runtimeVersion"],
        "protocolMajor": PROTOCOL_MAJOR,
        "releaseTag": manifest["releaseTag"],
        "sha256": digest,
        "size": size,
    }


def default_state_dir() -> Path:
    override = os.environ.get("LINKSTART_STATE_DIR")
    if override:
        return Path(override).expanduser()
    if os.name == "nt":
        base = os.environ.get("LOCALAPPDATA")
        if not base:
            raise RuntimeErrorCode("runtime_state_unavailable", "LOCALAPPDATA is not set")
        return Path(base) / "LinkStart"
    if platform.system().lower() == "darwin":
        return Path.home() / "Library" / "Application Support" / "LinkStart"
    return Path(os.environ.get("XDG_STATE_HOME", Path.home() / ".local" / "state")) / "linkstart"


def health(port: int, expected: dict) -> dict:
    try:
        with urlopen(f"http://127.0.0.1:{port}/v1/health", timeout=1) as response:
            data = json.load(response)
    except (OSError, URLError, json.JSONDecodeError) as exc:
        raise RuntimeErrorCode("runtime_health_failed", str(exc)) from exc
    if data.get("status") != "ready":
        raise RuntimeErrorCode("runtime_health_failed", "daemon is not ready")
    if data.get("version") != expected["runtimeVersion"] or data.get("protocolMajor") != PROTOCOL_MAJOR:
        raise RuntimeErrorCode("runtime_version_conflict", "daemon version/protocol mismatch")
    return data


def start(state_dir: Path, port: int) -> dict:
    info = verify()
    binary = info["path"]
    state_dir.mkdir(parents=True, exist_ok=True)
    if os.name != "nt":
        state_dir.chmod(0o700)
    command = [binary, "daemon", "start", "--state-dir", str(state_dir), "--port", str(port), "--json"]
    log_path = state_dir / "launcher.log"
    log = log_path.open("ab")
    flags = 0
    kwargs: dict = {"stdin": subprocess.DEVNULL, "stdout": log, "stderr": log}
    if os.name == "nt":
        flags = subprocess.CREATE_NEW_PROCESS_GROUP | subprocess.DETACHED_PROCESS  # type: ignore[attr-defined]
        kwargs["creationflags"] = flags
    else:
        kwargs["start_new_session"] = True
    try:
        child = subprocess.Popen(command, **kwargs)
    except OSError as exc:
        log.close()
        raise RuntimeErrorCode("runtime_start_failed", str(exc)) from exc
    finally:
        log.close()
    deadline = time.monotonic() + 10
    last_error = "daemon did not become ready"
    while time.monotonic() < deadline:
        try:
            result = health(port, info)
            return {
                **info,
                "daemon": "reused" if child.poll() == 0 else "started",
                "address": f"127.0.0.1:{port}",
                "stateDir": str(state_dir),
                "health": result,
            }
        except RuntimeErrorCode as exc:
            last_error = exc.detail
        if child.poll() not in (None, 0):
            raise RuntimeErrorCode("runtime_start_failed", f"daemon exited {child.returncode}")
        time.sleep(0.1)
    raise RuntimeErrorCode("runtime_health_failed", last_error)


def parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(description=__doc__)
    sub = p.add_subparsers(dest="command", required=True)
    for name in ("resolve", "verify"):
        c = sub.add_parser(name)
        c.add_argument("--json", action="store_true")
    c = sub.add_parser("start")
    c.add_argument("--state-dir", type=Path)
    c.add_argument("--port", type=int, default=45831)
    c.add_argument("--json", action="store_true")
    c = sub.add_parser("run")
    c.add_argument("--json", action="store_true")
    c.add_argument("args", nargs=argparse.REMAINDER)
    return p


def main() -> None:
    args = parser().parse_args()
    try:
        if args.command == "resolve":
            target = target_for()
            payload = {"target": target, "path": str(assets_dir() / "bin" / target / TARGET_FILES[target])}
        elif args.command == "verify":
            payload = verify()
        elif args.command == "start":
            payload = start(args.state_dir or default_state_dir(), args.port)
        else:
            info = verify()
            command = list(args.args)
            if command[:1] == ["--"]:
                command = command[1:]
            if not command:
                raise RuntimeErrorCode("runtime_command_missing", "no Runtime command supplied")
            os.execv(info["path"], [info["path"], *command])
            return
        emit(payload, args.json)
    except RuntimeErrorCode as exc:
        emit({"ok": False, "error": exc.code, "detail": exc.detail}, getattr(args, "json", False), 2)


if __name__ == "__main__":
    main()
