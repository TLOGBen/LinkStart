# Claude Code adapter

Use this reference only when the current host is Claude Code. Resolve `HELPER` to `${CLAUDE_PLUGIN_ROOT}/skills/link-start/scripts/runtime.py`; if that variable is unavailable, locate the installed plugin containing the active `SKILL.md` and use the same relative path.

## Admission and Runtime schema

- Require Claude Code 2.1.105+ and a live capability self-test. Prefer two-way Channel (`notifications/claude/channel` plus the integration feedback tool), labeled `Research Preview`.
- For an already-open Origin Session without Channel admission, use the Monitor flow below after a live same-session wake/ack/feedback self-test. Label it `Experimental compatibility`.
- If neither passes, return `claude_adapter_unavailable`. Never use `claude -p`, Agent SDK, Remote Control, automatic resume, a file mailbox, or a replacement session.

Before attach/register/launch, run:

```console
HELPER="${CLAUDE_PLUGIN_ROOT}/skills/link-start/scripts/runtime.py"
"$HELPER" verify --json
"$HELPER" run -- help --json
"$HELPER" start --state-dir "$STATE_DIR" --json
```

Require Runtime `0.1.5`, protocol `v1`, and the `operations.attach/register/launch/monitor/ack/feedback` schemas from `help --json`. Do not inspect Rust source or guess missing fields.

## Attach, register, and launch

Attach the current Origin Session:

```http
POST http://127.0.0.1:45831/v1/connections
Content-Type: application/json

{"protocolMajor":"v1","callsign":"<display-only>"}
```

Capture `connectionId`, `callsign`, `capability`, and `status` in memory without echoing capability. Immediately create the private session context:

```console
printf '%s' "$CONNECTION_CAPABILITY" | "$HELPER" context create \
  --context "$CONTEXT" --state-dir "$STATE_DIR" \
  --connection-id "$CONNECTION_ID" --capability-stdin --json
```

Context output fields are `contextId`, `contextPath`, `stateDir`, `connectionId`, `capability:"redacted"`, `pendingEventId`, `pendingAppInstanceId`, and `reused`. The file must remain `0600`; its directory is `0700`. Recreating it with a different state dir, connection ID, or capability fails `context_identity_mismatch`.

For a localhost App, validate Manifest v1 and register exactly as `help --json` specifies:

```http
POST http://127.0.0.1:45831/v1/apps
Authorization: Bearer <connection capability>
Content-Type: application/json

{"protocolMajor":"v1","connectionId":"<id>","manifest":{"appId":"<id>","displayName":"<name>","originPolicy":{"exactOrigin":"http://127.0.0.1:<port>"}}}
```

Capture `instanceId`, App capability, `origin`, and `status` privately and inject the App capability into the page process only.

For self-contained HTML, create a one-time launch grant:

```http
POST http://127.0.0.1:45831/v1/launch-grants
Authorization: Bearer <connection capability>
Content-Type: application/json

{"protocolMajor":"v1","connectionId":"<id>","manifest":{"appId":"<id>","displayName":"<name>","originPolicy":{"exactOrigin":"null"}},"page":{"htmlPath":"<absolute path>"}}
```

Open returned `launchUrl` immediately without logging it; it carries the one-time grant only in the fragment. Then unset temporary capability/grant variables.

## Attached Monitor flow

Arm one attached blocking background tool call in this Claude session:

```console
"$HELPER" arm --context "$CONTEXT" --timeout-seconds 300 --json
```

Completion re-enters this same session. Event output fields are `operation:"arm"`, `contextId`, `status:"event"`, `eventId`, `appInstanceId`, `sequence`, `payload`, `receiptId`, and `untrustedInput:true`. Timeout returns `status:"timeout"` and `rearmRequired:true`.

After accepting the pending Event into this Origin Session, answer and re-arm with one attached background tool call:

```console
"$HELPER" respond --context "$CONTEXT" \
  --payload '{"message":"<agent feedback>"}' \
  --timeout-seconds 300 --json
```

Do not manually call ack, feedback, and wait. `respond` infers `eventId` and `appInstanceId`, verifies identity, generates a stable `feedbackId`, records Delivery Ack, sends Feedback, then enters the next wait. Its output contains `deliveryAck`, `feedback`, and `next`; capability never appears. Supply `--event-id` only as an extra equality guard.

On every Event or timeout, keep the attached background arm/respond rhythm while the connection remains open. On explicit close:

```console
"$HELPER" close --context "$CONTEXT" --json
```

`close` deletes the locally persisted ephemeral capability and reports `connectionRevoked:false`; do not claim server-side revoke. If the Origin process is offline, return `origin_offline`.
