from __future__ import annotations

import importlib.util
import hashlib
import json
import os
from pathlib import Path
import sqlite3
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
FAKE_APP_SERVER = Path(__file__).with_name("fixtures") / "fake_codex_app_server.py"
SPEC = importlib.util.spec_from_file_location("linkstart_codex_adapter", MODULE_PATH)
assert SPEC and SPEC.loader
adapter = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(adapter)


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
                        "version": "0.1.5",
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
                        "version": "0.1.5",
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
                        "version": "0.1.5",
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
                        "runtimeVersion": "0.1.5",
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
                        "version": "0.1.5",
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
                        "runtimeVersion": "0.1.5",
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
