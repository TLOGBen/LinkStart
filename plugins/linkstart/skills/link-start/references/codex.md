# Codex adapter

Use this reference only when the current host is Codex. Resolve `HELPER` from the installed `link-start` skill directory as `scripts/runtime.py`; do not rely on a user cache/version path.

## Admission and Runtime schema

- Accept only a LinkStart-owned app-server that has owned the Origin thread since creation or explicit resume, with any TUI joined through `--remote`.
- Codex version and `userAgent` are diagnostic only. Admission requires live `initialize`/`initialized`, exact-thread `thread/resume`, and `thread/read`; later `turn/start`, `turn/steer(expectedTurnId)`, live subscription, event-marker reconciliation, and same-thread ownership remain fail-closed runtime capabilities. Do not maintain a version allowlist, denylist, minimum, or `>=` shortcut.
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

## Same-thread host-lease Monitor flow

After context creation, start the persistent Origin Adapter. `CODEX_THREAD_ID` is used when `--thread-id` is omitted. A managed control-socket Origin uses the default command:

```console
"$HELPER" adapter start --context "$CONTEXT" --json
```

For a LinkStart-owned loopback WebSocket app-server that already owns the same Origin thread, pass its exact endpoint:

```console
"$HELPER" adapter start --context "$CONTEXT" \
  --app-server-url "ws://127.0.0.1:<port>" --json
```

Only the exact form `ws://127.0.0.1:<port>` is accepted. The helper directly executes the checked-in `scripts/codex_ws_bridge.py`; never generate, copy, patch, or install a bridge at runtime. The bridge has no third-party Python dependency. An endpoint does not grant ownership and cannot hot-takeover an embedded or unrelated thread.

Require the exact redacted receipt fields `adapter:"codex-app-server"`, `monitorMode:"host-lease"`, the same opaque `threadId`, and `leaseStatus:"armed"`. The default path connects through `codex app-server proxy`; that Origin must belong to the managed app-server/control socket. A hidden or unrelated Host app-server fails closed with `codex_origin_mode_unsupported`.

The adapter continuously re-arms outside Agent turns. It reads the durable Runtime queue, reconciles a stable `linkstart:event:{eventId}` marker, uses `turn/steer(expectedTurnId)` for an active turn or `turn/start` for an idle thread, and records Delivery Ack only after app-server acceptance. Active/idle races are re-read once; never redirect to another thread.

The injected trusted envelope includes a nonblocking feedback command. After handling the untrusted App payload, execute that exact argv with `<JSON_OBJECT>` replaced by the feedback object. The equivalent explicit form is:

```console
"$HELPER" adapter feedback --context "$CONTEXT" \
  --event-id "$EVENT_ID" --payload '{"message":"<agent feedback>"}' --json
```

This queues Feedback in the private durable ledger only; it does not contact loopback Runtime from the Agent sandbox, Ack, or take ownership of the next wait. The Host Adapter flushes the queued payload to Runtime. Same event and payload retries return the same stable `feedbackId`; a different payload for that event fails closed. If the Agent does not queue explicit Feedback, the adapter sends the completed turn's final Agent message as fallback Feedback.

Inspect or close the lease with:

```console
"$HELPER" adapter status --context "$CONTEXT" --json
"$HELPER" adapter close --context "$CONTEXT" --json
```

Close must report `credentialDeleted:true`, `connectionRevoked:false`, and `leaseStatus:"closed"`.

## Foreground compatibility only

`arm` and `respond` remain available for bounded, explicitly foreground operation. Do not claim this mode supports unattended wake-up, and never run it concurrently with a host lease:

```console
"$HELPER" arm --context "$CONTEXT" --timeout-seconds 300 --json
"$HELPER" respond --context "$CONTEXT" --payload '{"message":"<agent feedback>"}' \
  --timeout-seconds 300 --json
```

If the owned app-server or Origin process is offline, return `origin_offline`. Do not claim Runtime-side revoke.

`leaseStatus:"armed"` and capability admission are MOP only. Report `LINKSTART_ACTIVE` only after a real App Event is delivered to this exact `threadId` and real Agent Feedback is observed by the same App Instance.
