# Codex adapter

Use this reference only when the current host is Codex.

## Admission

- Accept only an allowlisted LinkStart-owned app-server that has owned the Origin thread since creation or explicit resume, with any TUI joined through `--remote`.
- The initial allowlist is Codex 0.149.1 only. Prove initialize/handshake, required methods and events, live subscription, nonce round trip, and same-thread ownership. Unknown versions or schema drift fail closed.
- Label Unix remote app-server `Experimental Preview`; label native Windows loopback WebSocket `Experimental, not supported for production`.
- A standalone embedded TUI cannot be hot-taken over. Return `codex_origin_mode_unsupported`; do not silently create another thread.

## Delivery and wait rhythm

- When a turn is active, deliver with `turn/steer(expectedTurnId)`.
- When the owned thread is idle, deliver with `turn/start`.
- Reconcile turn races; if acceptance is unknown, keep the Event `received` or fail explicitly. Never redirect it to another thread.
- Keep a bounded foreground `monitor wait` against the verified bundled Runtime so the current Codex turn remains available for App input. A 300-second wait is the default interaction window; after each Event or timeout boundary, re-arm the next wait while the user still wants the connection open.
- Run `monitor ack` only after the app-server accepted the Event into this Origin thread. Send Agent Feedback through LinkStart without treating it as model-completion proof.

Never use `codex -p`, an SDK subprocess, cold resume, or a replacement Codex process. If the LinkStart-owned app-server or Origin process is offline, return `origin_offline`.
