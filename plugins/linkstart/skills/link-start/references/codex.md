# Codex adapter

Use this reference only when the current host is Codex. Resolve `HELPER` from the installed `link-start` skill directory as `scripts/runtime.py`; do not rely on a user cache/version path.

## Admission and Runtime schema

- Accept only an allowlisted LinkStart-owned app-server that has owned the Origin thread since creation or explicit resume, with any TUI joined through `--remote`.
- Initial allowlist: Codex 0.149.1 only. Prove initialize/handshake, required methods/events, live subscription, nonce round trip, and same-thread ownership. Standalone embedded TUI returns `codex_origin_mode_unsupported`.
- Label Unix remote app-server `Experimental Preview`; label native Windows loopback WebSocket `Experimental, not supported for production`.
- Never use `codex -p`, an SDK subprocess, cold resume, or a replacement process/thread.

Before attach/register/launch, run:

```console
"$HELPER" verify --json
"$HELPER" run -- help --json
"$HELPER" start --state-dir "$STATE_DIR" --json
```

Require Runtime `0.1.5`, protocol `v1`, and the `operations.attach/register/launch/monitor/ack/feedback` schemas from `help --json`. Do not inspect Rust source or infer missing fields.

## Attach, register, and launch

Attach the current Origin thread:

```http
POST http://127.0.0.1:45831/v1/connections
Content-Type: application/json

{"protocolMajor":"v1","callsign":"<display-only>"}
```

Keep returned `capability` in memory, then create a private context without echoing it:

```console
printf '%s' "$CONNECTION_CAPABILITY" | "$HELPER" context create \
  --context "$CONTEXT" --state-dir "$STATE_DIR" \
  --connection-id "$CONNECTION_ID" --capability-stdin --json
```

Context output fields are `contextId`, `contextPath`, `stateDir`, `connectionId`, `capability:"redacted"`, `pendingEventId`, `pendingAppInstanceId`, and `reused`. The file is `0600` under a `0700` directory. Identity changes fail closed.

Register a localhost App according to `help --json`:

```http
POST http://127.0.0.1:45831/v1/apps
Authorization: Bearer <connection capability>
Content-Type: application/json

{"protocolMajor":"v1","connectionId":"<id>","manifest":{"appId":"<id>","displayName":"<name>","originPolicy":{"exactOrigin":"http://127.0.0.1:<port>"}}}
```

Keep `instanceId` and App capability private. For self-contained HTML, create and immediately open a one-time launch URL:

```http
POST http://127.0.0.1:45831/v1/launch-grants
Authorization: Bearer <connection capability>
Content-Type: application/json

{"protocolMajor":"v1","connectionId":"<id>","manifest":{"appId":"<id>","displayName":"<name>","originPolicy":{"exactOrigin":"null"}},"page":{"htmlPath":"<absolute path>"}}
```

Never log the returned fragment grant. Unset temporary capability/grant variables after registration/launch.

## Same-thread foreground Monitor flow

Keep the current Codex turn attached to a bounded foreground wait:

```console
"$HELPER" arm --context "$CONTEXT" --timeout-seconds 300 --json
```

Event output fields are `operation:"arm"`, `contextId`, `status:"event"`, `eventId`, `appInstanceId`, `sequence`, `payload`, `receiptId`, and `untrustedInput:true`. Timeout returns `status:"timeout"` and `rearmRequired:true`.

Deliver an active-turn Event with `turn/steer(expectedTurnId)` or an idle-thread Event with `turn/start`. After the app-server accepts it into this Origin thread, respond and enter the next foreground wait with one call:

```console
"$HELPER" respond --context "$CONTEXT" \
  --payload '{"message":"<agent feedback>"}' \
  --timeout-seconds 300 --json
```

Do not manually compose ack, feedback, and wait. `respond` infers the pending identities, generates a stable `feedbackId`, performs Delivery Ack and Feedback, then blocks in the next wait. Output fields include `deliveryAck`, `feedback`, and `next`; capability is always absent. `--event-id` is optional and only checks equality with the pending Event.

Reconcile turn races instead of redirecting to another thread. After every Event or timeout, re-arm while the user wants the connection open. On explicit close, delete the ephemeral secret context:

```console
"$HELPER" close --context "$CONTEXT" --json
```

This reports `credentialDeleted:true` and `connectionRevoked:false`; do not claim Runtime-side revoke. If the owned app-server or Origin process is offline, return `origin_offline`.
