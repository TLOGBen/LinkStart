from __future__ import annotations

import importlib.util
import base64
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
import unittest


MODULE_PATH = (
    Path(__file__).parents[1]
    / "plugins"
    / "linkstart"
    / "skills"
    / "link-start"
    / "scripts"
    / "codex_adapter.py"
)
RUNTIME_HELPER = MODULE_PATH.with_name("runtime.py")
WEBSOCKET_BRIDGE = MODULE_PATH.with_name("codex_ws_bridge.py")
FAKE_APP_SERVER = Path(__file__).with_name("fixtures") / "fake_codex_app_server.py"
SPEC = importlib.util.spec_from_file_location("linkstart_codex_adapter", MODULE_PATH)
assert SPEC and SPEC.loader
adapter = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(adapter)


def _recv_exact(sock: socket.socket, size: int) -> bytes:
    chunks = []
    remaining = size
    while remaining:
        chunk = sock.recv(remaining)
        if not chunk:
            raise EOFError("socket closed")
        chunks.append(chunk)
        remaining -= len(chunk)
    return b"".join(chunks)


def _recv_client_frame(sock: socket.socket) -> tuple[int, bytes, bool]:
    first, second = _recv_exact(sock, 2)
    masked = bool(second & 0x80)
    length = second & 0x7F
    if length == 126:
        length = struct.unpack("!H", _recv_exact(sock, 2))[0]
    elif length == 127:
        length = struct.unpack("!Q", _recv_exact(sock, 8))[0]
    mask = _recv_exact(sock, 4) if masked else b""
    payload = _recv_exact(sock, length)
    if masked:
        payload = bytes(value ^ mask[index % 4] for index, value in enumerate(payload))
    return first & 0x0F, payload, masked


def _server_frame(opcode: int, payload: bytes, *, final: bool = True) -> bytes:
    first = (0x80 if final else 0) | opcode
    length = len(payload)
    if length < 126:
        return bytes((first, length)) + payload
    if length < 65536:
        return bytes((first, 126)) + struct.pack("!H", length) + payload
    return bytes((first, 127)) + struct.pack("!Q", length) + payload


class FakeAppServer:
    def __init__(self, status: dict, active_turn_id: str | None = None):
        self.status = status
        self.active_turn_id = active_turn_id
        self.inputs: list[dict] = []

    def read_thread(self, thread_id: str) -> dict:
        turns = []
        if self.active_turn_id:
            turns.append({"id": self.active_turn_id, "status": "inProgress", "items": []})
        return {"id": thread_id, "status": self.status, "turns": turns}

    def start_turn(self, thread_id: str, text: str) -> dict:
        self.inputs.append({"method": "turn/start", "threadId": thread_id, "text": text})
        self.status = {"type": "active"}
        return {"turn": {"id": "turn-from-event", "status": "inProgress"}}

    def steer_turn(self, thread_id: str, turn_id: str, text: str) -> dict:
        self.inputs.append(
            {
                "method": "turn/steer",
                "threadId": thread_id,
                "expectedTurnId": turn_id,
                "text": text,
            }
        )
        return {"turnId": turn_id}

    def contains_marker(self, thread_id: str, marker: str) -> bool:
        return any(item["threadId"] == thread_id and marker in item["text"] for item in self.inputs)


class ActiveToIdleRaceAppServer(FakeAppServer):
    def __init__(self):
        super().__init__({"type": "active"}, active_turn_id="turn-ended")
        self.steer_attempts = 0

    def steer_turn(self, thread_id: str, turn_id: str, text: str) -> dict:
        self.steer_attempts += 1
        self.status = {"type": "idle"}
        self.active_turn_id = None
        raise adapter.AppServerError(-32602, "no active turn")


class CompletedTurnAppServer(FakeAppServer):
    def read_thread(self, thread_id: str) -> dict:
        return {
            "id": thread_id,
            "status": {"type": "idle"},
            "turns": [
                {
                    "id": "turn-completed",
                    "status": "completed",
                    "items": [
                        {
                            "type": "agentMessage",
                            "phase": "final_answer",
                            "text": "LINKSTART_FINAL",
                        }
                    ],
                }
            ],
        }


class FakeRuntime:
    def __init__(self):
        self.acked: list[str] = []
        self.feedback: list[dict] = []

    def ack(self, event_id: str) -> dict:
        self.acked.append(event_id)
        return {"eventId": event_id, "status": "delivered", "deliveryAck": True}

    def send_feedback(
        self, app_instance_id: str, feedback_id: str, event_id: str, payload: dict
    ) -> dict:
        sent = {
            "appInstanceId": app_instance_id,
            "feedbackId": feedback_id,
            "inReplyToEventId": event_id,
            "payload": payload,
        }
        self.feedback.append(sent)
        return sent


class TimeoutThenEventRuntime(FakeRuntime):
    def __init__(self, event: dict):
        super().__init__()
        self.event = event
        self.wait_calls = 0

    def wait(self, timeout_seconds: float) -> dict | None:
        self.wait_calls += 1
        return None if self.wait_calls == 1 else self.event


class MemoryLedger:
    def __init__(self):
        self.rows: dict[str, dict] = {}

    def get(self, event_id: str) -> dict | None:
        return self.rows.get(event_id)

    def prepare(self, event: dict, marker: str) -> None:
        self.rows[event["eventId"]] = {"event": event, "marker": marker, "state": "dispatching"}

    def accepted(self, event_id: str, turn_id: str, method: str) -> None:
        self.rows[event_id].update(state="accepted", turnId=turn_id, method=method)

    def acked(self, event_id: str) -> None:
        self.rows[event_id]["state"] = "acked"


class CodexOriginAdapterTest(unittest.TestCase):
    def test_idle_event_starts_same_thread_before_ack(self) -> None:
        app_server = FakeAppServer({"type": "idle"})
        runtime = FakeRuntime()
        ledger = MemoryLedger()
        origin = adapter.CodexOriginAdapter(
            thread_id="thread-origin",
            app_server=app_server,
            runtime=runtime,
            ledger=ledger,
        )

        result = origin.deliver(
            {
                "eventId": "evt-1",
                "appInstanceId": "app-1",
                "sequence": 1,
                "payload": {"message": "使用者互動"},
                "receiptId": "rcpt-1",
            }
        )

        self.assertEqual(result["method"], "turn/start")
        self.assertEqual(result["threadId"], "thread-origin")
        self.assertEqual(runtime.acked, ["evt-1"])
        self.assertEqual(len(app_server.inputs), 1)
        self.assertIn("linkstart:event:evt-1", app_server.inputs[0]["text"])
        self.assertIn('"untrustedInput":true', app_server.inputs[0]["text"])
        self.assertEqual(ledger.rows["evt-1"]["state"], "acked")

    def test_active_event_steers_expected_turn_before_ack(self) -> None:
        app_server = FakeAppServer({"type": "active"}, active_turn_id="turn-active")
        runtime = FakeRuntime()
        ledger = MemoryLedger()
        origin = adapter.CodexOriginAdapter(
            thread_id="thread-origin",
            app_server=app_server,
            runtime=runtime,
            ledger=ledger,
        )

        result = origin.deliver(
            {
                "eventId": "evt-active",
                "appInstanceId": "app-1",
                "sequence": 2,
                "payload": {"selection": "A"},
                "receiptId": "rcpt-active",
            }
        )

        self.assertEqual(result["method"], "turn/steer")
        self.assertEqual(result["turnId"], "turn-active")
        self.assertEqual(app_server.inputs[0]["expectedTurnId"], "turn-active")
        self.assertEqual(runtime.acked, ["evt-active"])

    def test_active_to_idle_race_rechecks_and_starts_once(self) -> None:
        app_server = ActiveToIdleRaceAppServer()
        runtime = FakeRuntime()
        ledger = MemoryLedger()
        origin = adapter.CodexOriginAdapter(
            thread_id="thread-origin",
            app_server=app_server,
            runtime=runtime,
            ledger=ledger,
        )

        result = origin.deliver(
            {
                "eventId": "evt-race",
                "appInstanceId": "app-1",
                "sequence": 3,
                "payload": {"message": "race"},
                "receiptId": "rcpt-race",
            }
        )

        self.assertEqual(app_server.steer_attempts, 1)
        self.assertEqual(result["method"], "turn/start")
        self.assertEqual(len(app_server.inputs), 1)
        self.assertEqual(runtime.acked, ["evt-race"])

    def test_crash_after_host_acceptance_reconciles_marker_without_duplicate_input(self) -> None:
        event = {
            "eventId": "evt-reconcile",
            "appInstanceId": "app-1",
            "sequence": 4,
            "payload": {"message": "resume"},
            "receiptId": "rcpt-reconcile",
        }
        app_server = FakeAppServer({"type": "idle"})
        app_server.inputs.append(
            {
                "method": "turn/start",
                "threadId": "thread-origin",
                "text": adapter.event_input(event),
            }
        )
        runtime = FakeRuntime()
        ledger = MemoryLedger()
        ledger.rows[event["eventId"]] = {
            "event": event,
            "marker": adapter.event_marker(event["eventId"]),
            "state": "dispatching",
            "turnId": "turn-existing",
            "method": "turn/start",
        }
        origin = adapter.CodexOriginAdapter(
            thread_id="thread-origin",
            app_server=app_server,
            runtime=runtime,
            ledger=ledger,
        )

        result = origin.deliver(event)

        self.assertEqual(result["turnId"], "turn-existing")
        self.assertEqual(len(app_server.inputs), 1)
        self.assertEqual(runtime.acked, ["evt-reconcile"])

    def test_sqlite_ledger_survives_restart_and_is_private(self) -> None:
        event = {
            "eventId": "evt-durable",
            "appInstanceId": "app-1",
            "sequence": 5,
            "payload": {"message": "durable"},
            "receiptId": "rcpt-durable",
        }
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "adapter.sqlite3"
            first = adapter.SqliteLedger(path)
            first.prepare(event, adapter.event_marker(event["eventId"]))
            first.accepted(event["eventId"], "turn-durable", "turn/start")
            first.close()

            second = adapter.SqliteLedger(path)
            row = second.get(event["eventId"])
            second.acked(event["eventId"])
            second.close()

            self.assertEqual(row["state"], "accepted")
            self.assertEqual(row["turnId"], "turn-durable")
            self.assertEqual(row["event"]["payload"], {"message": "durable"})
            if os.name != "nt":
                self.assertEqual(path.stat().st_mode & 0o777, 0o600)

    def test_jsonl_client_resumes_origin_and_observes_injected_marker(self) -> None:
        client = adapter.JsonlAppServerClient([sys.executable, str(FAKE_APP_SERVER)])
        try:
            resumed = client.connect("thread-origin")
            started = client.start_turn("thread-origin", "linkstart:event:evt-jsonl")

            self.assertEqual(resumed["id"], "thread-origin")
            self.assertEqual(started["turn"]["id"], "turn-fake")
            self.assertTrue(client.contains_marker("thread-origin", "linkstart:event:evt-jsonl"))
        finally:
            client.close()

    def test_version_label_does_not_replace_live_capability_admission(self) -> None:
        client = adapter.JsonlAppServerClient(
            [sys.executable, str(FAKE_APP_SERVER), "future-build"]
        )
        try:
            resumed = client.connect("thread-origin")
        finally:
            client.close()

        self.assertEqual(resumed["id"], "thread-origin")
        self.assertEqual(client.compatibility_grade, "probed")

    def test_capability_admission_fails_closed_when_thread_read_is_missing(self) -> None:
        client = adapter.JsonlAppServerClient(
            [sys.executable, str(FAKE_APP_SERVER), "future-build", "no-thread-read"]
        )
        try:
            with self.assertRaises(adapter.AppServerError) as caught:
                client.connect("thread-origin")
        finally:
            client.close()

        self.assertEqual(caught.exception.code, -32601)
        self.assertIsNone(client.compatibility_grade)

    def test_websocket_url_gate_is_exact(self) -> None:
        command = adapter.websocket_bridge_command("ws://127.0.0.1:4510")

        self.assertEqual(command[-1], "ws://127.0.0.1:4510")
        self.assertEqual(Path(command[-2]).name, "codex_ws_bridge.py")
        for rejected in (
            "wss://127.0.0.1:4510",
            "ws://localhost:4510",
            "ws://127.0.0.2:4510",
            "ws://user@127.0.0.1:4510",
            "ws://127.0.0.1:4510/path",
            "ws://127.0.0.1:4510?query=1",
            "ws://127.0.0.1:4510#fragment",
            "ws://127.0.0.1",
            "ws://127.0.0.1:0",
        ):
            with self.subTest(url=rejected):
                with self.assertRaises(adapter.AdapterAdmissionError) as caught:
                    adapter.websocket_bridge_command(rejected)
                self.assertEqual(caught.exception.code, "codex_app_server_url_invalid")

    def test_websocket_bridge_roundtrips_jsonl_fragment_ping_close_and_eof(self) -> None:
        listener = socket.socket()
        listener.bind(("127.0.0.1", 0))
        listener.listen(1)
        listener.settimeout(5)
        port = listener.getsockname()[1]
        observed: dict[str, object] = {}

        def serve() -> None:
            connection = None
            try:
                connection, _ = listener.accept()
                connection.settimeout(5)
                request = b""
                while b"\r\n\r\n" not in request:
                    request += connection.recv(4096)
                headers = {}
                for line in request.decode("ascii").split("\r\n")[1:]:
                    if ":" in line:
                        name, value = line.split(":", 1)
                        headers[name.lower()] = value.strip()
                accept = base64.b64encode(
                    hashlib.sha1(
                        (headers["sec-websocket-key"] + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()
                    ).digest()
                ).decode()
                connection.sendall(
                    (
                        "HTTP/1.1 101 Switching Protocols\r\n"
                        "Upgrade: websocket\r\n"
                        "Connection: Upgrade\r\n"
                        f"Sec-WebSocket-Accept: {accept}\r\n\r\n"
                    ).encode("ascii")
                )
                opcode, payload, masked = _recv_client_frame(connection)
                observed.update(opcode=opcode, payload=payload, masked=masked)
                connection.sendall(_server_frame(0x9, b"probe"))
                connection.sendall(_server_frame(0x1, b'{"id":1,', final=False))
                connection.sendall(_server_frame(0x0, b'"result":{}}', final=True))
                while True:
                    reply_opcode, reply_payload, reply_masked = _recv_client_frame(connection)
                    if reply_opcode == 0xA:
                        observed.update(
                            pong=reply_payload,
                            pongMasked=reply_masked,
                        )
                        break
                connection.sendall(_server_frame(0x8, struct.pack("!H", 1000)))
                close_opcode, _, close_masked = _recv_client_frame(connection)
                observed.update(closeOpcode=close_opcode, closeMasked=close_masked)
            finally:
                if connection is not None:
                    connection.close()
                listener.close()

        worker = threading.Thread(target=serve)
        worker.start()
        result = subprocess.run(
            [sys.executable, str(WEBSOCKET_BRIDGE), f"ws://127.0.0.1:{port}"],
            input='{"method":"initialize","id":1,"params":{}}\n',
            capture_output=True,
            text=True,
            encoding="utf-8",
            timeout=10,
        )
        worker.join(timeout=5)

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(worker.is_alive())
        self.assertEqual(observed["opcode"], 0x1)
        self.assertEqual(observed["payload"], b'{"method":"initialize","id":1,"params":{}}')
        self.assertEqual(observed["masked"], True)
        self.assertEqual(observed["pong"], b"probe")
        self.assertEqual(observed["pongMasked"], True)
        self.assertEqual(observed["closeOpcode"], 0x8)
        self.assertEqual(observed["closeMasked"], True)
        self.assertEqual(result.stdout.splitlines(), ['{"id":1,"result":{}}'])

    def test_jsonl_client_times_out_instead_of_leaving_false_armed_lease(self) -> None:
        client = adapter.JsonlAppServerClient(
            [sys.executable, "-c", "import time; time.sleep(5)"], request_timeout=0.1
        )
        try:
            with self.assertRaises(adapter.AppServerError) as caught:
                client.connect("thread-origin")
        finally:
            client.close()

        self.assertEqual(caught.exception.message, "app_server_timeout")

    def test_feedback_queue_is_nonblocking_idempotent_and_host_flushed(self) -> None:
        event = {
            "eventId": "evt-feedback",
            "appInstanceId": "app-feedback",
            "sequence": 6,
            "payload": {"question": "status"},
            "receiptId": "rcpt-feedback",
        }
        with tempfile.TemporaryDirectory() as directory:
            ledger = adapter.SqliteLedger(Path(directory) / "adapter.sqlite3")
            ledger.prepare(event, adapter.event_marker(event["eventId"]))
            ledger.accepted(event["eventId"], "turn-feedback", "turn/start")
            ledger.acked(event["eventId"])
            runtime = FakeRuntime()

            first = adapter.queue_feedback(ledger, event["eventId"], {"message": "已處理"})
            second = adapter.queue_feedback(ledger, event["eventId"], {"message": "已處理"})
            flushed = adapter.flush_feedback(ledger, runtime, event["eventId"])
            repeated = adapter.flush_feedback(ledger, runtime, event["eventId"])
            with self.assertRaises(adapter.FeedbackConflict):
                adapter.queue_feedback(ledger, event["eventId"], {"message": "另一個答案"})
            ledger.close()

        self.assertEqual(first["feedbackId"], second["feedbackId"])
        self.assertEqual(first["status"], "queued")
        self.assertEqual(flushed["payload"], {"message": "已處理"})
        self.assertEqual(repeated["status"], "already-sent")
        self.assertEqual(len(runtime.feedback), 1)

    def test_runtime_store_waits_and_posts_ack_without_secret_in_argv(self) -> None:
        capability = "c" * 64
        with tempfile.TemporaryDirectory() as directory:
            state_dir = Path(directory)
            (state_dir / "daemon.json").write_text(
                json.dumps(
                    {
                        "address": "127.0.0.1:45831",
                        "pid": 123,
                        "version": "0.1.6",
                        "protocolMajor": "v1",
                    }
                ),
                encoding="utf-8",
            )
            db = sqlite3.connect(state_dir / "linkstart.sqlite3")
            db.executescript(
                """
                CREATE TABLE connections(id TEXT PRIMARY KEY, capability_hash TEXT, status TEXT);
                CREATE TABLE apps(id TEXT PRIMARY KEY, connection_id TEXT);
                CREATE TABLE events(id TEXT PRIMARY KEY, app_id TEXT, sequence INTEGER,
                    payload TEXT, status TEXT, receipt_id TEXT, created_at INTEGER);
                """
            )
            db.execute(
                "INSERT INTO connections VALUES(?,?,?)",
                ["conn-1", hashlib.sha256(capability.encode()).hexdigest(), "online"],
            )
            db.execute("INSERT INTO apps VALUES(?,?)", ["app-1", "conn-1"])
            db.execute(
                "INSERT INTO events VALUES(?,?,?,?,?,?,?)",
                ["evt-store", "app-1", 1, '{"message":"queued"}', "received", "rcpt-store", 1],
            )
            db.commit()
            db.close()
            posts: list[dict] = []

            def post(path: str, payload: dict, bearer: str) -> dict:
                posts.append({"path": path, "payload": payload, "bearer": bearer})
                return {"eventId": payload["eventId"], "status": "delivered", "deliveryAck": True}

            runtime = adapter.RuntimeStoreClient(
                state_dir=state_dir,
                connection_id="conn-1",
                capability=capability,
                http_post=post,
            )
            event = runtime.wait(0.2)
            ack = runtime.ack(event["eventId"])

        self.assertEqual(event["payload"], {"message": "queued"})
        self.assertEqual(event["untrustedInput"], True)
        self.assertEqual(ack["deliveryAck"], True)
        self.assertEqual(posts[0]["path"], "/v1/connections/conn-1/ack")
        self.assertEqual(posts[0]["bearer"], capability)

    def test_runtime_conflicts_only_normalize_equivalent_durable_records(self) -> None:
        capability = "f" * 64
        with tempfile.TemporaryDirectory() as directory:
            state_dir = Path(directory)
            (state_dir / "daemon.json").write_text(
                json.dumps(
                    {
                        "address": "127.0.0.1:45831",
                        "pid": 123,
                        "version": "0.1.6",
                        "protocolMajor": "v1",
                    }
                ),
                encoding="utf-8",
            )
            db = sqlite3.connect(state_dir / "linkstart.sqlite3")
            db.executescript(
                """
                CREATE TABLE connections(id TEXT PRIMARY KEY, capability_hash TEXT, status TEXT);
                CREATE TABLE apps(id TEXT PRIMARY KEY, connection_id TEXT);
                CREATE TABLE events(id TEXT PRIMARY KEY, app_id TEXT, sequence INTEGER,
                    payload TEXT, status TEXT, receipt_id TEXT, created_at INTEGER);
                CREATE TABLE feedback(id TEXT PRIMARY KEY, app_id TEXT, in_reply_to TEXT,
                    payload TEXT);
                """
            )
            db.execute(
                "INSERT INTO connections VALUES(?,?,?)",
                ["conn-1", hashlib.sha256(capability.encode()).hexdigest(), "online"],
            )
            db.execute("INSERT INTO apps VALUES(?,?)", ["app-1", "conn-1"])
            db.execute(
                "INSERT INTO events VALUES(?,?,?,?,?,?,?)",
                ["evt-1", "app-1", 1, "{}", "delivered", "rcpt-1", 1],
            )
            db.execute(
                "INSERT INTO feedback VALUES(?,?,?,?)",
                ["fb-1", "app-1", "evt-1", '{"message":"same"}'],
            )
            db.commit()
            db.close()

            def conflict(path: str, payload: dict, bearer: str) -> dict:
                if path.endswith("/ack"):
                    raise adapter.RuntimeBoundaryError(
                        "event_not_received_or_not_inflight", "409"
                    )
                raise adapter.RuntimeBoundaryError("storage_error", "403")

            runtime = adapter.RuntimeStoreClient(
                state_dir=state_dir,
                connection_id="conn-1",
                capability=capability,
                http_post=conflict,
            )
            ack = runtime.ack("evt-1")
            feedback = runtime.send_feedback(
                "app-1", "fb-1", "evt-1", {"message": "same"}
            )
            with self.assertRaises(adapter.RuntimeBoundaryError):
                runtime.send_feedback("app-1", "fb-1", "evt-1", {"message": "different"})
            db = sqlite3.connect(state_dir / "linkstart.sqlite3")
            db.execute("UPDATE events SET status='failed' WHERE id='evt-1'")
            db.commit()
            db.close()
            with self.assertRaises(adapter.RuntimeBoundaryError):
                runtime.ack("evt-1")

        self.assertEqual(ack["status"], "already-delivered")
        self.assertEqual(feedback["status"], "already-sent")

    def test_timeout_rearms_without_false_failure(self) -> None:
        event = {
            "eventId": "evt-rearm",
            "appInstanceId": "app-1",
            "sequence": 7,
            "payload": {"message": "after-timeout"},
            "receiptId": "rcpt-rearm",
        }
        with tempfile.TemporaryDirectory() as directory:
            ledger = adapter.SqliteLedger(Path(directory) / "adapter.sqlite3")
            runtime = TimeoutThenEventRuntime(event)
            app_server = FakeAppServer({"type": "idle"})
            origin = adapter.CodexOriginAdapter(
                thread_id="thread-origin",
                app_server=app_server,
                runtime=runtime,
                ledger=ledger,
            )

            adapter.run_monitor(
                origin=origin,
                runtime=runtime,
                ledger=ledger,
                should_stop=lambda: bool(runtime.acked),
                wait_seconds=0.01,
            )
            lease_status = ledger.get_meta("leaseStatus")
            ledger.close()

        self.assertEqual(runtime.wait_calls, 2)
        self.assertEqual(runtime.acked, ["evt-rearm"])
        self.assertEqual(lease_status, "armed")

    def test_adapter_start_receipt_is_redacted_and_exact(self) -> None:
        receipt = adapter.lease_receipt("thread-origin", "armed")

        self.assertEqual(
            receipt,
            {
                "ok": True,
                "operation": "adapter",
                "adapter": "codex-app-server",
                "monitorMode": "host-lease",
                "threadId": "thread-origin",
                "leaseStatus": "armed",
            },
        )
        self.assertNotIn("capability", json.dumps(receipt))
        self.assertNotIn("credential", json.dumps(receipt))

    def test_unsupported_origin_fails_closed(self) -> None:
        with self.assertRaises(adapter.AdapterAdmissionError) as caught:
            adapter.require_thread_id(None)

        self.assertEqual(caught.exception.code, "codex_origin_mode_unsupported")
        self.assertEqual(
            adapter.error_payload(caught.exception),
            {"ok": False, "error": "codex_origin_mode_unsupported"},
        )

    def test_supervisor_arms_and_closes_without_agent_turn(self) -> None:
        capability = "d" * 64
        with tempfile.TemporaryDirectory() as directory:
            state_dir = Path(directory)
            (state_dir / "daemon.json").write_text(
                json.dumps(
                    {
                        "address": "127.0.0.1:45831",
                        "pid": 123,
                        "version": "0.1.6",
                        "protocolMajor": "v1",
                    }
                ),
                encoding="utf-8",
            )
            db = sqlite3.connect(state_dir / "linkstart.sqlite3")
            db.executescript(
                """
                CREATE TABLE connections(id TEXT PRIMARY KEY, capability_hash TEXT, status TEXT);
                CREATE TABLE apps(id TEXT PRIMARY KEY, connection_id TEXT);
                CREATE TABLE events(id TEXT PRIMARY KEY, app_id TEXT, sequence INTEGER,
                    payload TEXT, status TEXT, receipt_id TEXT, created_at INTEGER);
                """
            )
            db.execute(
                "INSERT INTO connections VALUES(?,?,?)",
                ["conn-1", hashlib.sha256(capability.encode()).hexdigest(), "online"],
            )
            db.commit()
            db.close()
            context_path = state_dir / "context.json"
            context_path.write_text(
                json.dumps(
                    {
                        "schemaVersion": 1,
                        "runtimeVersion": "0.1.6",
                        "protocolMajor": "v1",
                        "contextId": "ctx-1",
                        "stateDir": str(state_dir),
                        "connectionId": "conn-1",
                        "connectionCapability": capability,
                        "pendingEvent": None,
                    }
                ),
                encoding="utf-8",
            )
            if os.name != "nt":
                context_path.chmod(0o600)
            lease_path = state_dir / "lease.sqlite3"
            worker = threading.Thread(
                target=adapter.supervise,
                kwargs={
                    "context_path": context_path,
                    "thread_id": "thread-origin",
                    "lease_path": lease_path,
                    "app_server_command": [sys.executable, str(FAKE_APP_SERVER)],
                    "wait_seconds": 0.01,
                },
            )
            worker.start()
            deadline = time.monotonic() + 2
            status = None
            while time.monotonic() < deadline:
                if lease_path.exists():
                    ledger = adapter.SqliteLedger(lease_path)
                    status = ledger.get_meta("leaseStatus")
                    if status == "armed":
                        ledger.set_meta("desiredState", "closed")
                        ledger.close()
                        break
                    ledger.close()
                time.sleep(0.01)
            worker.join(timeout=2)
            ledger = adapter.SqliteLedger(lease_path)
            final_status = ledger.get_meta("leaseStatus")
            ledger.close()

        self.assertEqual(status, "armed")
        self.assertFalse(worker.is_alive())
        self.assertEqual(final_status, "closed")

    def test_background_start_status_and_close_report_exact_lifecycle(self) -> None:
        capability = "e" * 64
        with tempfile.TemporaryDirectory() as directory:
            state_dir = Path(directory)
            (state_dir / "daemon.json").write_text(
                json.dumps(
                    {
                        "address": "127.0.0.1:45831",
                        "pid": 123,
                        "version": "0.1.6",
                        "protocolMajor": "v1",
                    }
                ),
                encoding="utf-8",
            )
            db = sqlite3.connect(state_dir / "linkstart.sqlite3")
            db.executescript(
                """
                CREATE TABLE connections(id TEXT PRIMARY KEY, capability_hash TEXT, status TEXT);
                CREATE TABLE apps(id TEXT PRIMARY KEY, connection_id TEXT);
                CREATE TABLE events(id TEXT PRIMARY KEY, app_id TEXT, sequence INTEGER,
                    payload TEXT, status TEXT, receipt_id TEXT, created_at INTEGER);
                """
            )
            db.execute(
                "INSERT INTO connections VALUES(?,?,?)",
                ["conn-bg", hashlib.sha256(capability.encode()).hexdigest(), "online"],
            )
            db.commit()
            db.close()
            context_path = state_dir / "context.json"
            context_path.write_text(
                json.dumps(
                    {
                        "schemaVersion": 1,
                        "runtimeVersion": "0.1.6",
                        "protocolMajor": "v1",
                        "contextId": "ctx-bg",
                        "stateDir": str(state_dir),
                        "connectionId": "conn-bg",
                        "connectionCapability": capability,
                        "pendingEvent": None,
                    }
                ),
                encoding="utf-8",
            )
            if os.name != "nt":
                context_path.chmod(0o600)
            lease_path = state_dir / "background.sqlite3"

            started = adapter.start_background(
                context_path=context_path,
                thread_id="thread-origin",
                lease_path=lease_path,
                app_server_command=[sys.executable, str(FAKE_APP_SERVER)],
            )
            status = adapter.background_status(lease_path)
            closed = adapter.stop_background(lease_path, context_path=context_path)
            context_deleted = not context_path.exists()

        self.assertEqual(started, adapter.lease_receipt("thread-origin", "armed"))
        self.assertEqual(status, adapter.lease_receipt("thread-origin", "armed"))
        self.assertEqual(
            closed,
            {
                **adapter.lease_receipt("thread-origin", "closed"),
                "credentialDeleted": True,
                "connectionRevoked": False,
            },
        )
        self.assertTrue(context_deleted)

    def test_adapter_cli_parent_exit_keeps_lease_armed_until_explicit_close(self) -> None:
        capability = "a" * 64
        with tempfile.TemporaryDirectory() as directory:
            state_dir = Path(directory)
            (state_dir / "daemon.json").write_text(
                json.dumps(
                    {
                        "address": "127.0.0.1:45831",
                        "pid": 123,
                        "version": "0.1.6",
                        "protocolMajor": "v1",
                    }
                ),
                encoding="utf-8",
            )
            db = sqlite3.connect(state_dir / "linkstart.sqlite3")
            db.executescript(
                """
                CREATE TABLE connections(id TEXT PRIMARY KEY, capability_hash TEXT, status TEXT);
                CREATE TABLE apps(id TEXT PRIMARY KEY, connection_id TEXT);
                CREATE TABLE events(id TEXT PRIMARY KEY, app_id TEXT, sequence INTEGER,
                    payload TEXT, status TEXT, receipt_id TEXT, created_at INTEGER);
                """
            )
            db.execute(
                "INSERT INTO connections VALUES(?,?,?)",
                ["conn-cli", hashlib.sha256(capability.encode()).hexdigest(), "online"],
            )
            db.commit()
            db.close()
            context_path = state_dir / "context.json"
            context_path.write_text(
                json.dumps(
                    {
                        "schemaVersion": 1,
                        "runtimeVersion": "0.1.6",
                        "protocolMajor": "v1",
                        "contextId": "ctx-cli",
                        "stateDir": str(state_dir),
                        "connectionId": "conn-cli",
                        "connectionCapability": capability,
                        "pendingEvent": None,
                    }
                ),
                encoding="utf-8",
            )
            if os.name != "nt":
                context_path.chmod(0o600)
            environment = os.environ.copy()
            environment["LINKSTART_CODEX_COMMAND_JSON"] = json.dumps(
                [sys.executable, str(FAKE_APP_SERVER)]
            )
            start = subprocess.run(
                [
                    sys.executable,
                    str(MODULE_PATH),
                    "start",
                    "--context",
                    str(context_path),
                    "--thread-id",
                    "thread-origin",
                    "--json",
                ],
                check=True,
                capture_output=True,
                text=True,
                encoding="utf-8",
                env=environment,
                timeout=10,
            )
            status = subprocess.run(
                [
                    sys.executable,
                    str(MODULE_PATH),
                    "status",
                    "--context",
                    str(context_path),
                    "--json",
                ],
                check=True,
                capture_output=True,
                text=True,
                encoding="utf-8",
                timeout=10,
            )
            closed = subprocess.run(
                [
                    sys.executable,
                    str(MODULE_PATH),
                    "close",
                    "--context",
                    str(context_path),
                    "--json",
                ],
                check=True,
                capture_output=True,
                text=True,
                encoding="utf-8",
                timeout=10,
            )

        self.assertEqual(json.loads(start.stdout)["leaseStatus"], "armed")
        self.assertEqual(json.loads(status.stdout)["leaseStatus"], "armed")
        self.assertEqual(json.loads(closed.stdout)["leaseStatus"], "closed")

    def test_runtime_helper_exposes_adapter_without_removing_foreground_commands(self) -> None:
        top = subprocess.run(
            [sys.executable, str(RUNTIME_HELPER), "--help"],
            check=True,
            capture_output=True,
            text=True,
            encoding="utf-8",
        ).stdout
        adapter_help = subprocess.run(
            [sys.executable, str(RUNTIME_HELPER), "adapter", "--help"],
            check=True,
            capture_output=True,
            text=True,
            encoding="utf-8",
        ).stdout

        self.assertIn("arm", top)
        self.assertIn("respond", top)
        self.assertIn("adapter", top)
        self.assertIn("start", adapter_help)
        self.assertIn("status", adapter_help)
        self.assertIn("feedback", adapter_help)
        self.assertIn("close", adapter_help)

    def test_runtime_adapter_start_exposes_explicit_app_server_url(self) -> None:
        start_help = subprocess.run(
            [sys.executable, str(RUNTIME_HELPER), "adapter", "start", "--help"],
            check=True,
            capture_output=True,
            text=True,
            encoding="utf-8",
        ).stdout

        self.assertIn("--app-server-url", start_help)

    def test_event_input_carries_nonblocking_feedback_action_without_secret(self) -> None:
        event = {
            "eventId": "evt-action",
            "appInstanceId": "app-1",
            "sequence": 8,
            "payload": {"message": "answer me"},
            "receiptId": "rcpt-action",
        }
        text = adapter.event_input(
            event,
            feedback_command=[
                "python3",
                "/plugin/runtime.py",
                "adapter",
                "feedback",
                "--context",
                "/private/context.json",
                "--event-id",
                "evt-action",
                "--payload",
                "<JSON_OBJECT>",
                "--json",
            ],
        )

        self.assertIn('"feedbackMode":"nonblocking"', text)
        self.assertIn('"feedbackCommand":["python3","/plugin/runtime.py"', text)
        self.assertIn("After handling the App payload, execute feedbackCommand exactly once", text)
        self.assertNotIn("capability", text.lower())
        self.assertNotIn("bearer", text.lower())

    def test_completed_turn_is_flushed_as_feedback_by_host_adapter(self) -> None:
        event = {
            "eventId": "evt-completed",
            "appInstanceId": "app-completed",
            "sequence": 9,
            "payload": {"message": "finish"},
            "receiptId": "rcpt-completed",
        }
        with tempfile.TemporaryDirectory() as directory:
            ledger = adapter.SqliteLedger(Path(directory) / "adapter.sqlite3")
            ledger.prepare(event, adapter.event_marker(event["eventId"]))
            ledger.accepted(event["eventId"], "turn-completed", "turn/start")
            ledger.acked(event["eventId"])
            runtime = FakeRuntime()
            origin = adapter.CodexOriginAdapter(
                thread_id="thread-origin",
                app_server=CompletedTurnAppServer({"type": "idle"}),
                runtime=runtime,
                ledger=ledger,
            )

            origin.collect_feedback()
            row = ledger.get(event["eventId"])
            ledger.close()

        self.assertEqual(runtime.feedback[0]["payload"], {"message": "LINKSTART_FINAL"})
        self.assertEqual(row["feedbackState"], "sent")


if __name__ == "__main__":
    unittest.main()
