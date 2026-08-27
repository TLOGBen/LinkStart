#!/usr/bin/env python3
"""Validate the fixed LinkStart App Manifest v1 contract."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import re
from urllib.parse import urlparse


FIELDS = {
    "protocolMajor",
    "appId",
    "displayName",
    "appVersion",
    "entry",
    "originPolicy",
    "requestedCapabilities",
    "structuredInputs",
}
CAPABILITIES = {
    "event:submit",
    "event:cancel-own-received",
    "event:read-own-status",
    "feedback:stream-own",
}
INPUTS = {"single_choice", "free_text"}
APP_ID = re.compile(r"^[a-z0-9](?:[a-z0-9-]*[a-z0-9])?(?:\.[a-z0-9](?:[a-z0-9-]*[a-z0-9])?)+$")


def fail(message: str) -> None:
    raise ValueError(message)


def validate(data: object) -> dict:
    if not isinstance(data, dict) or set(data) != FIELDS:
        fail(f"top-level fields must be exactly {sorted(FIELDS)}")
    if data["protocolMajor"] != "v1":
        fail("protocolMajor must be v1")
    if not isinstance(data["appId"], str) or not APP_ID.fullmatch(data["appId"]):
        fail("appId must be a reverse-DNS identifier")
    for key in ("displayName", "appVersion"):
        if not isinstance(data[key], str) or not data[key].strip():
            fail(f"{key} must be a non-empty string")
    entry = data["entry"]
    origin = data["originPolicy"]
    if not isinstance(entry, dict) or set(entry) != {"kind", "location"}:
        fail("entry must contain exactly kind and location")
    if not isinstance(origin, dict) or set(origin) != {"kind", "exactOrigin"}:
        fail("originPolicy must contain exactly kind and exactOrigin")
    if entry["kind"] == "localhost_app":
        parsed = urlparse(entry["location"])
        if (
            parsed.scheme != "http"
            or parsed.hostname not in {"127.0.0.1", "localhost", "::1"}
            or parsed.port is None
            or parsed.username is not None
            or parsed.password is not None
        ):
            fail("localhost_app entry must be an explicit loopback http URL with a port")
        host = f"[{parsed.hostname}]" if parsed.hostname == "::1" else parsed.hostname
        expected = f"{parsed.scheme}://{host}:{parsed.port}"
        if origin != {"kind": "exact", "exactOrigin": expected}:
            fail("localhost_app originPolicy must exactly match the entry origin")
    elif entry["kind"] == "self_contained_html":
        location = entry["location"]
        if not isinstance(location, str) or not (location.startswith("file://") or Path(location).is_absolute()):
            fail("self_contained_html entry must be an absolute path or file URL")
        if origin != {"kind": "opaque_file", "exactOrigin": "null"}:
            fail("self_contained_html requires opaque_file/null originPolicy")
    else:
        fail("entry.kind must be self_contained_html or localhost_app")
    caps = data["requestedCapabilities"]
    if not isinstance(caps, list) or not caps or len(caps) != len(set(caps)) or not set(caps) <= CAPABILITIES:
        fail(f"requestedCapabilities must be a unique non-empty subset of {sorted(CAPABILITIES)}")
    inputs = data["structuredInputs"]
    if not isinstance(inputs, list) or len(inputs) != len(set(inputs)) or not set(inputs) <= INPUTS:
        fail(f"structuredInputs must be a unique subset of {sorted(INPUTS)}")
    return {"ok": True, "protocolMajor": "v1", "appId": data["appId"], "entryKind": entry["kind"]}


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("manifest", type=Path)
    p.add_argument("--json", action="store_true")
    args = p.parse_args()
    try:
        data = json.loads(args.manifest.read_text(encoding="utf-8"))
        result = validate(data)
        print(json.dumps(result, ensure_ascii=False, sort_keys=True) if args.json else "App Manifest v1 valid")
    except (OSError, json.JSONDecodeError, ValueError) as exc:
        payload = {"ok": False, "error": "app_manifest_invalid", "detail": str(exc)}
        print(json.dumps(payload, ensure_ascii=False, sort_keys=True) if args.json else f"app_manifest_invalid: {exc}")
        raise SystemExit(2)


if __name__ == "__main__":
    main()
