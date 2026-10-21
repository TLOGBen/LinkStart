# Runtime owned-session supervisor

`OwnedSessionSupervisor` is the in-process caller boundary for a trusted Runtime
launcher. Construct it with the same `Engine` and `OwnedCodexRegistry`, then call
`start(approved_config, private_member_credential, facade, ThreadAdmission::New)`.
`ExactResume(id)` is explicit and still passes the host's private ownership checks.
Executable, argv, workspace and execution profile must never come directly from
untrusted HTTP JSON. An HTTP adapter must resolve an approved launch policy first.

`start` returns acceptance of the worker, before provider initialization completes.
`get` and `list` return current authorized status. `start_turn` accepts a bounded
in-memory queue item and its caller-supplied operation ID; this is **not durable
acceptance or completion**. Before provider dispatch the host creates its durable
unknown intent. A Runtime crash before dispatch can lose a queue item. Callers
requiring a durable queue must persist admission upstream; they must reconcile
the same operation ID rather than assume a timed-out command was never sent.
The existing host operation ledger prevents resending an existing operation ID.
`operation(credential, scope, operation_id)` exposes the in-memory `Queued`,
`Dispatching`, `Accepted`, `Rejected`, `Unknown` or `Confirmed` snapshot and a
sanitized error. `Confirmed` means an observed provider turn completion, not a
successful forum mutation or completed Agent task. Same-ID/same-payload replay
returns the original queue acceptance without sending again; different payload
is denied. Each session retains at most 1024 operation snapshots. Worker exit
turns unfinished queue snapshots into Unknown. A Runtime restart loses these
in-memory snapshots; consult the durable host operation ledger/exact thread.
`provider_outcome` separately records `in_progress`, `completed`, `interrupted`
or `failed`. Confirmed terminal evidence includes interrupted/failed turns and
must never be displayed as successful forum work. Failed/interrupted observations
carry a provider error; successful exact reconciliation clears the stale unknown
error and updates only the addressed operation. It preserves other active turns.

Both host and supervisor retain the original String-returning reconciliation
methods. `OwnedCodexHost::reconcile_operation_observation` and
`OwnedSessionSupervisor::reconcile_observation` additionally return typed
`ReconciledOperation { operation_id, thread_id, turn_id, outcome }` evidence.

`read_exact_thread`, `reconcile` and `interrupt` accept bounded response waits.
`Pending` means that the accepted command can still run; it is not a retry signal.
History uncertainty remains `HostError::Unknown`; there is no automatic replacement
thread or model-call retry. Management methods check current member identity and
endpoint authority. Workers recheck authorization immediately before every queued
command. The real forum facade retains its own current authorization checks.
Command admission and dispatch use one required-grant mapping:

| Operation | Endpoint grant |
| --- | --- |
| get, list, operation, read_exact_thread, reconcile | observe |
| start, start_turn | send |
| interrupt | control |
| stop | manage |

Stop returns its already-authorized management snapshot without requiring an
additional observe grant. Once the actual worker has returned, repeated and
concurrent stop calls retain a stable terminal result even after its handle has
been joined. Stop changes phase under the same status mutex used by the worker;
an already-published terminal phase, worker-exited flag and failure are preserved
even when the worker returns between stop's JoinHandle check and status update.
The deterministic transition tests pin this latter interleaving invariant for
Exited and Unknown, rather than claiming to reproduce actual OS scheduling.
The prior real-provider v2 proof predates this repair and does not prove this
new source version. Worker and heartbeat creation use `thread::Builder::spawn` Results;
worker-spawn failure removes the reserved session and returns an error. Provider
initialization/resume failure still closes the held host and joins heartbeat.
`worker_stage` and sanitized `worker_failure { stage, kind }` preserve which
startup/run/cleanup boundary failed without exposing provider text or credentials.
An Unknown terminal status is not evidence that initialization ever succeeded.

This slice requires the exact member's credential for management. Human admin,
session-maintainer and appointed-controller cross-member management needs a
separate authorized HTTP management gate; HTTP must never borrow a member bearer
as proof that the requesting administrator is authorized.

Each worker exclusively owns its host and continuously pumps provider messages
even when no caller makes requests. Its separate heartbeat thread uses a bounded
registry liveness/renewal entry, never the host's request mutex. It refreshes only
the exact already-owned member attachment, using the private member bearer. It
does not rotate credentials, extend credential expiry, restore grants or roles,
manufacture a new attachment, or release forum claims. Renew after machine sleep
is allowed only if actual held ownership and root authorization still validate.

`stop` first prevents new queued admission, fences callback admission using the
original binding generation, and waits up to the requested budget (30-second cap).
Existing provider requests are bounded by the trusted host timeout; stop can return
`Pending` while those unwind. The worker stops heartbeat, joins it, fences/drains
callbacks, closes and drops the actual host, then returns. `active_count` inspects
actual worker JoinHandles; a stopped admission or requested kill does not remove
the worker count. Repeating stop uses the same original generation. `request_shutdown`
sets stop requests and attempts immediate registry fencing with a zero drain
budget, without claiming that callback drain or process cleanup already finished;
the daemon must keep this supervisor and poll
actual counts until workers return. Dropping it requests shutdown but does not
claim cleanup already finished.

No Engine/core, session-map, host, or HTTP global lock crosses provider I/O. The
registry heartbeat entry acquires a local liveness lease and releases facts before
Engine authorization; the same callback drain includes heartbeat leases. Shutdown
does not publish completion until the worker has returned. Sessions cannot be
silently restarted or replaced by reusing a session ID in this supervisor.

Tests use a local fake app-server with real Engine/SQLite callbacks, including a
6.2-second blocked request against the real minimum configured five-second TTL.
They do not prove model decisions, HTTP wiring, a 65-minute idle interval, native
workspace sandbox behavior, or autonomous forum/task completion.
