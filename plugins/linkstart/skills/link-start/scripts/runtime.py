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
import uuid
from urllib.error import URLError
from urllib.request import urlopen

import claude_adapter
import codex_adapter


PROTOCOL_MAJOR = "v1"
RUNTIME_VERSION = "0.1.6"
CONTEXT_SCHEMA_VERSION = 1
DEFAULT_MONITOR_TIMEOUT = 300
TARGET_FILES = {
    "linux-x64-musl": "linkstart",
    "windows-x64": "linkstart.exe",
    "macos-universal": "linkstart",
}


# Windows 主控台/管線預設 cp950，會把 Runtime 的 UTF-8 中文輸出解成亂碼或 UnicodeDecodeError。
for _stream in (sys.stdout, sys.stderr):
    _reconfigure = getattr(_stream, "reconfigure", None)
    if _reconfigure is not None:
        try:
            _reconfigure(encoding="utf-8")
        except OSError:
            pass


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
            encoding="utf-8",
            errors="replace",
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


def default_context_path() -> Path:
    override = os.environ.get("LINKSTART_CONTEXT")
    if override:
        return Path(override).expanduser()
    return default_state_dir() / "active-session.json"


def write_private_context(path: Path, data: dict) -> None:
    path = path.expanduser().resolve()
    path.parent.mkdir(parents=True, exist_ok=True)
    if os.name != "nt":
        path.parent.chmod(0o700)
    temporary = path.with_name(f".{path.name}.{os.getpid()}.tmp")
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
    descriptor = os.open(temporary, flags, 0o600)
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8") as handle:
            json.dump(data, handle, ensure_ascii=False, sort_keys=True)
            handle.write("\n")
            handle.flush()
            os.fsync(handle.fileno())
        os.chmod(temporary, 0o600)
        os.replace(temporary, path)
        os.chmod(path, 0o600)
    finally:
        if temporary.exists():
            temporary.unlink()


def load_context(path: Path) -> tuple[Path, dict]:
    path = path.expanduser()
    if path.is_symlink():
        raise RuntimeErrorCode("session_context_invalid", f"context cannot be a symlink: {path}")
    path = path.resolve()
    if not path.is_file():
        raise RuntimeErrorCode("session_context_missing", f"missing private context: {path}")
    if os.name != "nt" and stat.S_IMODE(path.stat().st_mode) != 0o600:
        raise RuntimeErrorCode("session_context_not_private", f"context mode must be 0600: {path}")
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        raise RuntimeErrorCode("session_context_invalid", str(exc)) from exc
    required = {
        "schemaVersion",
        "runtimeVersion",
        "protocolMajor",
        "contextId",
        "stateDir",
        "connectionId",
        "connectionCapability",
        "pendingEvent",
    }
    if set(data) != required:
        raise RuntimeErrorCode("session_context_invalid", "unexpected context fields")
    if (
        data.get("schemaVersion") != CONTEXT_SCHEMA_VERSION
        or data.get("runtimeVersion") != RUNTIME_VERSION
        or data.get("protocolMajor") != PROTOCOL_MAJOR
    ):
        raise RuntimeErrorCode("context_identity_mismatch", "context Runtime/protocol identity mismatch")
    for key in ("contextId", "stateDir", "connectionId", "connectionCapability"):
        if not isinstance(data.get(key), str) or not data[key]:
            raise RuntimeErrorCode("session_context_invalid", f"invalid {key}")
    return path, data


def context_summary(path: Path, data: dict, *, reused: bool = False) -> dict:
    pending = data.get("pendingEvent")
    return {
        "ok": True,
        "operation": "context",
        "contextId": data["contextId"],
        "contextPath": str(path),
        "stateDir": data["stateDir"],
        "connectionId": data["connectionId"],
        "capability": "redacted",
        "pendingEventId": pending.get("eventId") if isinstance(pending, dict) else None,
        "pendingAppInstanceId": pending.get("appInstanceId") if isinstance(pending, dict) else None,
        "reused": reused,
    }


def create_context(path: Path, state_dir: Path, connection_id: str, capability: str) -> dict:
    if not connection_id.strip() or len(capability) < 32:
        raise RuntimeErrorCode("session_context_invalid", "connection identity or capability is invalid")
    path = path.expanduser()
    if path.is_symlink():
        raise RuntimeErrorCode("session_context_invalid", f"context cannot be a symlink: {path}")
    path = path.resolve()
    state_dir = state_dir.expanduser().resolve()
    state_dir.mkdir(parents=True, exist_ok=True)
    if os.name != "nt":
        state_dir.chmod(0o700)
    if path.exists():
        current_path, current = load_context(path)
        same_identity = (
            Path(current["stateDir"]).resolve() == state_dir
            and current["connectionId"] == connection_id
            and current["connectionCapability"] == capability
        )
        if not same_identity:
            raise RuntimeErrorCode("context_identity_mismatch", "existing context belongs to another connection")
        return context_summary(current_path, current, reused=True)
    data = {
        "schemaVersion": CONTEXT_SCHEMA_VERSION,
        "runtimeVersion": RUNTIME_VERSION,
        "protocolMajor": PROTOCOL_MAJOR,
        "contextId": str(uuid.uuid4()),
        "stateDir": str(state_dir),
        "connectionId": connection_id,
        "connectionCapability": capability,
        "pendingEvent": None,
    }
    write_private_context(path, data)
    return context_summary(path, data)


def run_runtime_json(context: dict, arguments: list[str], *, timeout: int) -> dict:
    info = verify()
    capability = context["connectionCapability"]
    try:
        proc = subprocess.run(
            [info["path"], *arguments],
            check=False,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=timeout,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        raise RuntimeErrorCode("runtime_operation_failed", str(exc)) from exc
    if proc.returncode != 0:
        detail = (proc.stderr.strip() or proc.stdout.strip() or f"exit {proc.returncode}")
        detail = detail.replace(capability, "<redacted>")
        raise RuntimeErrorCode("runtime_operation_failed", detail[:1000])
    try:
        result = json.loads(proc.stdout)
    except json.JSONDecodeError as exc:
        raise RuntimeErrorCode("runtime_operation_failed", "Runtime did not return JSON") from exc
    serialized = json.dumps(result, ensure_ascii=False)
    if capability in serialized:
        raise RuntimeErrorCode("runtime_secret_exposure", "Runtime output contained a capability")
    return result


def wait_for_event(path: Path, context: dict, timeout_seconds: int) -> dict:
    if context.get("pendingEvent") is not None:
        raise RuntimeErrorCode("event_pending_response", "respond to the pending Event before re-arming")
    result = run_runtime_json(
        context,
        [
            "monitor",
            "wait",
            "--json",
            "--state-dir",
            context["stateDir"],
            "--connection-id",
            context["connectionId"],
            "--capability",
            context["connectionCapability"],
            "--timeout-seconds",
            str(timeout_seconds),
        ],
        timeout=timeout_seconds + 10,
    )
    if result.get("status") == "timeout":
        return {
            "ok": True,
            "operation": "arm",
            "contextId": context["contextId"],
            "status": "timeout",
            "rearmRequired": True,
        }
    required = {"eventId", "appInstanceId", "sequence", "payload", "receiptId", "status"}
    if not required <= set(result) or result.get("status") != "received":
        raise RuntimeErrorCode("monitor_event_invalid", "monitor output did not match help --json schema")
    event = {key: result[key] for key in required}
    event["untrustedInput"] = True
    context["pendingEvent"] = event
    write_private_context(path, context)
    return {
        "ok": True,
        "operation": "arm",
        "contextId": context["contextId"],
        "status": "event",
        **event,
    }


def respond_and_wait(
    path: Path,
    context: dict,
    payload_text: str,
    event_id: str | None,
    timeout_seconds: int,
) -> dict:
    pending = context.get("pendingEvent")
    if not isinstance(pending, dict):
        raise RuntimeErrorCode("event_not_armed", "arm must receive an Event before respond")
    if event_id is not None and event_id != pending.get("eventId"):
        raise RuntimeErrorCode("context_identity_mismatch", "event-id does not match the pending Event")
    try:
        payload = json.loads(payload_text)
    except json.JSONDecodeError as exc:
        raise RuntimeErrorCode("feedback_payload_invalid", "--payload must be valid JSON") from exc
    canonical_payload = json.dumps(payload, ensure_ascii=False, sort_keys=True, separators=(",", ":"))
    response_digest = hashlib.sha256(canonical_payload.encode("utf-8")).hexdigest()
    existing_digest = pending.get("responseDigest")
    if existing_digest is not None and existing_digest != response_digest:
        raise RuntimeErrorCode("context_identity_mismatch", "pending Event already has another response payload")
    if existing_digest is None:
        pending["responseDigest"] = response_digest
        pending["feedbackId"] = "fb-" + hashlib.sha256(
            f"{context['contextId']}\0{pending['eventId']}\0{response_digest}".encode("utf-8")
        ).hexdigest()[:32]
        pending["acked"] = False
        pending["feedbackSent"] = False
        write_private_context(path, context)
    ack_result: dict | None = None
    if not pending["acked"]:
        ack_result = run_runtime_json(
            context,
            [
                "monitor",
                "ack",
                "--json",
                "--state-dir",
                context["stateDir"],
                "--connection-id",
                context["connectionId"],
                "--capability",
                context["connectionCapability"],
                "--event-id",
                pending["eventId"],
            ],
            timeout=20,
        )
        pending["acked"] = True
        write_private_context(path, context)
    if not pending["feedbackSent"]:
        feedback_result = run_runtime_json(
            context,
            [
                "feedback",
                "send",
                "--json",
                "--state-dir",
                context["stateDir"],
                "--connection-id",
                context["connectionId"],
                "--capability",
                context["connectionCapability"],
                "--app-instance-id",
                pending["appInstanceId"],
                "--feedback-id",
                pending["feedbackId"],
                "--payload",
                canonical_payload,
                "--in-reply-to-event-id",
                pending["eventId"],
            ],
            timeout=20,
        )
        pending["feedbackSent"] = True
        write_private_context(path, context)
    else:
        feedback_result = {
            "feedbackId": pending["feedbackId"],
            "inReplyToEventId": pending["eventId"],
            "status": "already-sent",
        }
    completed_event_id = pending["eventId"]
    context["pendingEvent"] = None
    write_private_context(path, context)
    next_result = wait_for_event(path, context, timeout_seconds)
    return {
        "ok": True,
        "operation": "respond",
        "contextId": context["contextId"],
        "eventId": completed_event_id,
        "deliveryAck": ack_result or {"status": "already-acked"},
        "feedback": feedback_result,
        "next": next_result,
    }


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
    c = sub.add_parser("context")
    context_sub = c.add_subparsers(dest="context_command", required=True)
    create = context_sub.add_parser("create")
    create.add_argument("--context", type=Path, default=default_context_path())
    create.add_argument("--state-dir", type=Path, default=default_state_dir())
    create.add_argument("--connection-id", required=True)
    create.add_argument("--capability-stdin", action="store_true", required=True)
    create.add_argument("--json", action="store_true")
    show = context_sub.add_parser("show")
    show.add_argument("--context", type=Path, default=default_context_path())
    show.add_argument("--json", action="store_true")
    c = sub.add_parser("arm")
    c.add_argument("--context", type=Path, default=default_context_path())
    c.add_argument("--timeout-seconds", type=int, default=DEFAULT_MONITOR_TIMEOUT)
    c.add_argument("--json", action="store_true")
    c = sub.add_parser("respond")
    c.add_argument("--context", type=Path, default=default_context_path())
    c.add_argument("--payload", required=True)
    c.add_argument("--event-id")
    c.add_argument("--timeout-seconds", type=int, default=DEFAULT_MONITOR_TIMEOUT)
    c.add_argument("--json", action="store_true")
    c = sub.add_parser("adapter")
    adapter_sub = c.add_subparsers(dest="adapter_command", required=True)
    start_adapter = adapter_sub.add_parser("start")
    start_adapter.add_argument("--context", type=Path, default=default_context_path())
    start_adapter.add_argument("--host", choices=("codex", "claude"), default="codex")
    start_adapter.add_argument("--mode", choices=("channel", "monitor"))
    start_adapter.add_argument("--thread-id")
    start_adapter.add_argument("--app-server-url")
    start_adapter.add_argument("--json", action="store_true")
    status_adapter = adapter_sub.add_parser("status")
    status_adapter.add_argument("--context", type=Path, default=default_context_path())
    status_adapter.add_argument("--json", action="store_true")
    probe_adapter = adapter_sub.add_parser("probe")
    probe_adapter.add_argument("--context", type=Path, default=default_context_path())
    probe_adapter.add_argument("--json", action="store_true")
    pull_adapter = adapter_sub.add_parser("pull")
    pull_adapter.add_argument("--context", type=Path, default=default_context_path())
    pull_adapter.add_argument("--json", action="store_true")
    feedback_adapter = adapter_sub.add_parser("feedback")
    feedback_adapter.add_argument("--context", type=Path, default=default_context_path())
    feedback_adapter.add_argument("--event-id", required=True)
    feedback_adapter.add_argument("--payload", required=True)
    feedback_adapter.add_argument("--json", action="store_true")
    close_adapter = adapter_sub.add_parser("close")
    close_adapter.add_argument("--context", type=Path, default=default_context_path())
    close_adapter.add_argument("--json", action="store_true")
    c = sub.add_parser("close")
    c.add_argument("--context", type=Path, default=default_context_path())
    c.add_argument("--json", action="store_true")
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
        elif args.command == "run":
            info = verify()
            command = list(args.args)
            if command[:1] == ["--"]:
                command = command[1:]
            if not command:
                raise RuntimeErrorCode("runtime_command_missing", "no Runtime command supplied")
            os.execv(info["path"], [info["path"], *command])
            return
        elif args.command == "context":
            if args.context_command == "create":
                if not args.capability_stdin or sys.stdin.isatty():
                    raise RuntimeErrorCode(
                        "session_context_invalid", "pipe the connection capability to --capability-stdin"
                    )
                capability = sys.stdin.read().strip()
                payload = create_context(args.context, args.state_dir, args.connection_id, capability)
            else:
                path, context = load_context(args.context)
                payload = context_summary(path, context)
        elif args.command == "arm":
            if args.timeout_seconds < 1 or args.timeout_seconds > 3600:
                raise RuntimeErrorCode("monitor_timeout_invalid", "timeout must be 1..3600 seconds")
            path, context = load_context(args.context)
            payload = wait_for_event(path, context, args.timeout_seconds)
        elif args.command == "respond":
            if args.timeout_seconds < 1 or args.timeout_seconds > 3600:
                raise RuntimeErrorCode("monitor_timeout_invalid", "timeout must be 1..3600 seconds")
            path, context = load_context(args.context)
            payload = respond_and_wait(
                path, context, args.payload, args.event_id, args.timeout_seconds
            )
        elif args.command == "adapter":
            claude_lease = claude_adapter.default_lease_path(args.context)
            codex_lease = codex_adapter.default_lease_path(args.context)
            if claude_lease.exists() and codex_lease.exists():
                raise RuntimeErrorCode(
                    "monitor_owner_conflict",
                    "both claude and codex adapter leases exist for this context",
                )
            if args.adapter_command == "start":
                if args.host == "claude":
                    if codex_lease.exists():
                        raise RuntimeErrorCode(
                            "monitor_owner_conflict",
                            "a codex adapter lease already owns this context",
                        )
                    if not args.mode:
                        raise RuntimeErrorCode(
                            "adapter_mode_missing",
                            "--mode channel|monitor is required for --host claude",
                        )
                    adapter_name = (
                        claude_adapter.ADAPTER_CHANNEL
                        if args.mode == "channel"
                        else claude_adapter.ADAPTER_MONITOR
                    )
                    payload = claude_adapter.arm_lease(
                        context_path=args.context,
                        adapter=adapter_name,
                        lease_path=claude_lease,
                    )
                else:
                    if claude_lease.exists():
                        raise RuntimeErrorCode(
                            "monitor_owner_conflict",
                            "a claude adapter lease already owns this context",
                        )
                    thread_id = codex_adapter.require_thread_id(
                        args.thread_id or os.environ.get("CODEX_THREAD_ID")
                    )
                    app_server_command = (
                        codex_adapter.websocket_bridge_command(args.app_server_url)
                        if args.app_server_url
                        else None
                    )
                    payload = codex_adapter.start_background(
                        context_path=args.context,
                        thread_id=thread_id,
                        lease_path=codex_lease,
                        app_server_command=app_server_command,
                    )
            elif args.adapter_command == "status":
                if claude_lease.exists():
                    payload = claude_adapter.lease_status(claude_lease)
                else:
                    payload = codex_adapter.background_status(codex_lease)
            elif args.adapter_command == "probe":
                payload = claude_adapter.request_probe(claude_lease)
            elif args.adapter_command == "pull":
                payload = claude_adapter.pull_event(args.context, lease_path=claude_lease)
            elif args.adapter_command == "feedback":
                parsed_payload = json.loads(args.payload)
                if not isinstance(parsed_payload, dict):
                    raise RuntimeErrorCode(
                        "feedback_payload_invalid", "--payload must be a JSON object"
                    )
                if claude_lease.exists():
                    payload = claude_adapter.send_event_feedback(
                        args.context,
                        args.event_id,
                        parsed_payload,
                        lease_path=claude_lease,
                    )
                else:
                    load_context(args.context)
                    ledger = codex_adapter.SqliteLedger(codex_lease)
                    try:
                        payload = codex_adapter.queue_feedback(
                            ledger, args.event_id, parsed_payload
                        )
                    finally:
                        ledger.close()
            else:
                if claude_lease.exists():
                    payload = claude_adapter.stop_lease(
                        claude_lease, context_path=args.context
                    )
                else:
                    payload = codex_adapter.stop_background(
                        codex_lease, context_path=args.context
                    )
        else:
            path, context = load_context(args.context)
            claude_lease = claude_adapter.default_lease_path(path)
            if claude_lease.exists():
                stopped = claude_adapter.stop_lease(claude_lease, context_path=path)
                payload = {
                    "ok": True,
                    "operation": "close",
                    "contextId": context["contextId"],
                    "connectionId": context["connectionId"],
                    "credentialDeleted": stopped["credentialDeleted"],
                    "connectionRevoked": False,
                    "leaseStatus": stopped["leaseStatus"],
                }
                emit(payload, args.json)
                return
            lease_path = codex_adapter.default_lease_path(path)
            if lease_path.exists():
                stopped = codex_adapter.stop_background(lease_path, context_path=path)
                payload = {
                    "ok": True,
                    "operation": "close",
                    "contextId": context["contextId"],
                    "connectionId": context["connectionId"],
                    "credentialDeleted": stopped["credentialDeleted"],
                    "connectionRevoked": False,
                    "leaseStatus": stopped["leaseStatus"],
                }
            else:
                path.unlink()
                payload = {
                    "ok": True,
                    "operation": "close",
                    "contextId": context["contextId"],
                    "connectionId": context["connectionId"],
                    "credentialDeleted": True,
                    "connectionRevoked": False,
                }
        emit(payload, args.json)
    except (
        codex_adapter.AdapterAdmissionError,
        codex_adapter.FeedbackConflict,
        codex_adapter.RuntimeBoundaryError,
        codex_adapter.AppServerError,
    ) as exc:
        code = getattr(exc, "code", "feedback_payload_conflict")
        emit({"ok": False, "error": code}, getattr(args, "json", False), 2)
    except RuntimeErrorCode as exc:
        emit({"ok": False, "error": exc.code, "detail": exc.detail}, getattr(args, "json", False), 2)


if __name__ == "__main__":
    main()
