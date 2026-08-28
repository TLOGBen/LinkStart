from __future__ import annotations

import importlib.util
import io
import json
import os
from pathlib import Path
import sys
import tempfile
import threading
import unittest


SCRIPTS_DIR = (
    Path(__file__).parents[1]
    / "plugins"
    / "linkstart"
    / "skills"
    / "link-start"
    / "scripts"
)
sys.path.insert(0, os.fspath(SCRIPTS_DIR))
SPEC = importlib.util.spec_from_file_location(
    "linkstart_claude_adapter", SCRIPTS_DIR / "claude_adapter.py"
)
assert SPEC and SPEC.loader
adapter = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(adapter)


EVENT = {
    "eventId": "evt-1",
    "appInstanceId": "app-1",
    "sequence": 1,
    "payload": {"choice": "A"},
    "receiptId": "rcpt-1",
    "status": "received",
    "untrustedInput": True,
}


class FakeRuntime:
    def __init__(self, events: list[dict] | None = None):
        self.events = list(events or [])
        self.acked: list[str] = []
        self.feedback: list[dict] = []

    def wait(self, timeout_seconds: float) -> dict | None:
        if self.events:
            return dict(self.events[0])
        return None

    def ack(self, event_id: str) -> dict:
        self.acked.append(event_id)
        self.events = [event for event in self.events if event["eventId"] != event_id]
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


def write_context(directory: Path) -> Path:
    path = directory / "active-session.json"
    path.write_text(
        json.dumps(
            {
                "schemaVersion": 1,
                "runtimeVersion": "0.1.5",
                "protocolMajor": "v1",
                "contextId": "ctx-1",
                "stateDir": os.fspath(directory),
                "connectionId": "conn-1",
                "connectionCapability": "secret-capability",
                "pendingEvent": None,
            }
        ),
        encoding="utf-8",
    )
    if os.name != "nt":
        path.chmod(0o600)
    return path


class ClaudeAdapterTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.dir = Path(self.tmp.name)
        self.context_path = write_context(self.dir)
        self.lease_path = adapter.default_lease_path(self.context_path)

    def make_supervisor(self, *, mode: str, runtime: FakeRuntime, emitted: list, probes: list):
        return adapter.WakeSupervisor(
            adapter=mode,
            context_path=self.context_path,
            emit=emitted.append,
            emit_probe=probes.append,
            lease_path=self.lease_path,
            runtime_factory=lambda context: runtime,
            poll_seconds=0.01,
        )

    def arm(self, mode: str) -> None:
        lease = adapter.WakeLease(self.lease_path)
        lease.arm(mode, "ctx-1")

    def test_adapter_start_receipt_is_redacted_and_exact(self) -> None:
        self.arm(adapter.ADAPTER_CHANNEL)
        runtime = FakeRuntime()
        emitted: list = []
        supervisor = self.make_supervisor(
            mode=adapter.ADAPTER_CHANNEL, runtime=runtime, emitted=emitted, probes=[]
        )
        stop = threading.Event()
        thread = threading.Thread(
            target=supervisor.run, args=(stop.is_set,), daemon=True
        )
        thread.start()
        try:
            receipt = adapter.arm_lease(
                context_path=self.context_path,
                adapter=adapter.ADAPTER_CHANNEL,
                lease_path=self.lease_path,
                wait_seconds=5,
            )
        finally:
            stop.set()
            thread.join(timeout=2)
        self.assertEqual(
            receipt,
            {
                "ok": True,
                "operation": "adapter",
                "adapter": "claude-channel",
                "monitorMode": "host-lease",
                "contextId": "ctx-1",
                "leaseStatus": "armed",
            },
        )
        self.assertNotIn("secret-capability", json.dumps(receipt))

    def test_arm_without_wake_owner_fails_closed(self) -> None:
        with self.assertRaises(adapter.AdapterAdmissionError) as caught:
            adapter.arm_lease(
                context_path=self.context_path,
                adapter=adapter.ADAPTER_CHANNEL,
                lease_path=self.lease_path,
                wait_seconds=0.3,
            )
        self.assertEqual(caught.exception.code, "claude_adapter_unavailable")

    def test_status_without_lease_fails_closed(self) -> None:
        with self.assertRaises(adapter.AdapterAdmissionError) as caught:
            adapter.lease_status(self.lease_path)
        self.assertEqual(caught.exception.code, "claude_adapter_unavailable")

    def test_wake_announces_event_once_and_never_acks(self) -> None:
        self.arm(adapter.ADAPTER_CHANNEL)
        runtime = FakeRuntime([EVENT])
        emitted: list = []
        supervisor = self.make_supervisor(
            mode=adapter.ADAPTER_CHANNEL, runtime=runtime, emitted=emitted, probes=[]
        )
        self.assertTrue(supervisor.tick())
        self.assertTrue(supervisor.tick())
        self.assertEqual(len(emitted), 1)
        self.assertEqual(emitted[0]["eventId"], "evt-1")
        self.assertEqual(runtime.acked, [])
        ledger = adapter.SqliteLedger(self.lease_path)
        try:
            row = ledger.get("evt-1")
        finally:
            ledger.close()
        self.assertIsNotNone(row)
        self.assertEqual(row["state"], "accepted")

    def test_wake_ignores_other_owner_and_closed_lease(self) -> None:
        self.arm(adapter.ADAPTER_MONITOR)
        runtime = FakeRuntime([EVENT])
        emitted: list = []
        supervisor = self.make_supervisor(
            mode=adapter.ADAPTER_CHANNEL, runtime=runtime, emitted=emitted, probes=[]
        )
        self.assertFalse(supervisor.tick())
        self.assertEqual(emitted, [])
        self.arm(adapter.ADAPTER_CHANNEL)
        adapter.WakeLease(self.lease_path).set_meta("desiredState", "closed")
        self.assertFalse(supervisor.tick())
        self.assertEqual(emitted, [])

    def test_probe_round_trip(self) -> None:
        self.arm(adapter.ADAPTER_CHANNEL)
        runtime = FakeRuntime()
        probes: list = []
        supervisor = self.make_supervisor(
            mode=adapter.ADAPTER_CHANNEL, runtime=runtime, emitted=[], probes=probes
        )
        supervisor.tick()
        lease = adapter.WakeLease(self.lease_path)
        lease.set_meta("leaseStatus", "armed")
        stop = threading.Event()
        thread = threading.Thread(target=supervisor.run, args=(stop.is_set,), daemon=True)
        thread.start()
        try:
            result = adapter.request_probe(self.lease_path, wait_seconds=5)
        finally:
            stop.set()
            thread.join(timeout=2)
        self.assertEqual(result["probe"], "emitted")
        self.assertEqual(len(probes), 1)
        self.assertEqual(probes[0], result["nonce"])

    def test_pull_event_acks_and_returns_untrusted_envelope(self) -> None:
        self.arm(adapter.ADAPTER_CHANNEL)
        runtime = FakeRuntime([EVENT])
        result = adapter.pull_event(
            self.context_path, lease_path=self.lease_path, runtime=runtime
        )
        self.assertEqual(result["status"], "event")
        self.assertEqual(result["eventId"], "evt-1")
        self.assertTrue(result["untrustedInput"])
        self.assertEqual(runtime.acked, ["evt-1"])
        self.assertNotIn("secret-capability", json.dumps(result))
        empty = adapter.pull_event(
            self.context_path, lease_path=self.lease_path, runtime=runtime
        )
        self.assertEqual(empty["status"], "no-event")

    def test_feedback_acks_flushes_and_is_idempotent(self) -> None:
        self.arm(adapter.ADAPTER_CHANNEL)
        runtime = FakeRuntime([EVENT])
        emitted: list = []
        supervisor = self.make_supervisor(
            mode=adapter.ADAPTER_CHANNEL, runtime=runtime, emitted=emitted, probes=[]
        )
        supervisor.tick()
        first = adapter.send_event_feedback(
            self.context_path,
            "evt-1",
            {"message": "done"},
            lease_path=self.lease_path,
            runtime=runtime,
        )
        self.assertEqual(runtime.acked, ["evt-1"])
        self.assertEqual(len(runtime.feedback), 1)
        again = adapter.send_event_feedback(
            self.context_path,
            "evt-1",
            {"message": "done"},
            lease_path=self.lease_path,
            runtime=runtime,
        )
        self.assertEqual(again["feedbackId"], first["feedbackId"])
        self.assertEqual(again["status"], "already-sent")
        self.assertEqual(len(runtime.feedback), 1)
        with self.assertRaises(adapter.FeedbackConflict):
            adapter.send_event_feedback(
                self.context_path,
                "evt-1",
                {"message": "different"},
                lease_path=self.lease_path,
                runtime=runtime,
            )

    def test_feedback_unknown_event_fails_closed(self) -> None:
        self.arm(adapter.ADAPTER_CHANNEL)
        runtime = FakeRuntime()
        with self.assertRaises(adapter.RuntimeBoundaryError) as caught:
            adapter.send_event_feedback(
                self.context_path,
                "evt-unknown",
                {"message": "x"},
                lease_path=self.lease_path,
                runtime=runtime,
            )
        self.assertEqual(caught.exception.code, "feedback_unknown_event")

    def test_close_reports_exact_lifecycle_truth(self) -> None:
        self.arm(adapter.ADAPTER_CHANNEL)
        result = adapter.stop_lease(self.lease_path, context_path=self.context_path)
        self.assertTrue(result["credentialDeleted"])
        self.assertFalse(result["connectionRevoked"])
        self.assertEqual(result["leaseStatus"], "closed")
        self.assertFalse(self.context_path.exists())

    def test_monitor_wake_line_has_no_payload_or_secrets(self) -> None:
        line = adapter.monitor_wake_line(EVENT, self.context_path)
        self.assertIn("linkstart:event:evt-1", line)
        self.assertIn("pull_event", line)
        self.assertNotIn("choice", line)
        self.assertNotIn("secret-capability", line)
        self.assertNotIn("\n", line)

    def test_channel_content_marks_untrusted_and_carries_marker(self) -> None:
        content = adapter.channel_content(EVENT)
        self.assertIn("linkstart:event:evt-1", content)
        self.assertIn("untrusted", content.lower())
        self.assertIn('"untrustedInput":true', content)
        self.assertNotIn("secret-capability", content)


class ChannelServerProtocolTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.dir = Path(self.tmp.name)
        self.context_path = write_context(self.dir)
        self.out = io.StringIO()
        self.server = adapter.ChannelServer(
            context_path=self.context_path,
            lease_path=adapter.default_lease_path(self.context_path),
            stdin=io.StringIO(""),
            stdout=self.out,
        )

    def replies(self) -> list[dict]:
        return [json.loads(line) for line in self.out.getvalue().splitlines()]

    def test_initialize_declares_channel_capability(self) -> None:
        self.server.handle_message(
            {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                "protocolVersion": "2025-06-18"
            }}
        )
        reply = self.replies()[0]
        result = reply["result"]
        self.assertEqual(result["protocolVersion"], "2025-06-18")
        self.assertEqual(result["capabilities"]["experimental"], {"claude/channel": {}})
        self.assertIn("tools", result["capabilities"])
        self.assertIn("untrusted", result["instructions"].lower())

    def test_tools_list_exposes_send_feedback_and_pull_event(self) -> None:
        self.server.handle_message({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})
        tools = self.replies()[0]["result"]["tools"]
        names = {tool["name"] for tool in tools}
        self.assertEqual(names, {"send_feedback", "pull_event"})

    def test_unknown_method_returns_error(self) -> None:
        self.server.handle_message({"jsonrpc": "2.0", "id": 3, "method": "nope"})
        self.assertEqual(self.replies()[0]["error"]["code"], -32601)

    def test_invalid_feedback_arguments_fail_closed(self) -> None:
        self.server.handle_message(
            {
                "jsonrpc": "2.0",
                "id": 4,
                "method": "tools/call",
                "params": {"name": "send_feedback", "arguments": {"event_id": "x"}},
            }
        )
        reply = self.replies()[0]["result"]
        self.assertTrue(reply["isError"])
        body = json.loads(reply["content"][0]["text"])
        self.assertEqual(body["error"], "feedback_payload_invalid")

    def test_event_notification_shape(self) -> None:
        self.server._emit_event(EVENT)
        message = self.replies()[0]
        self.assertEqual(message["method"], "notifications/claude/channel")
        params = message["params"]
        self.assertEqual(params["meta"]["event_id"], "evt-1")
        self.assertEqual(params["meta"]["untrusted"], "true")
        for key in params["meta"]:
            self.assertRegex(key, r"^[A-Za-z0-9_]+$")
        self.assertIn("linkstart:event:evt-1", params["content"])


if __name__ == "__main__":
    unittest.main()
