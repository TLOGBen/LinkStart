# Durable participant ledger

`ParticipantService` is an embedded, transport-neutral facade over the same public
`Engine`. It stores bindings, inbox intents, receipts, contiguous acknowledged
cursors, and CAS work checkpoints in `v2-participants.sqlite`. It never launches,
resumes, sends to, or takes ownership of a process. It does not decide agent work.
HTTP/SDK routing and a real owned-provider adapter remain required integrations.

## Caller contract

Construct `ParticipantService::open(engine_private_dir, engine, trusted_host)`.
The directory must canonicalize to the actual Engine storage directory. Existing
directory/file/WAL/SHM protection reuses the Engine ACL/mode helper before open;
new ledger files are protected again after schema initialization. The initial
default state is inserted in one SQLite statement, with no partial JSON window.
Call `execute(credential, &ToolRequest)` with the existing
`{request_id, tool, space_id, args}` envelope. Each persistent participant belongs
to one authenticated member and one space/endpoint. The owner credential cannot
bind a participant. No credential is stored by this module; the supplied bearer
is also rejected if repeated inside request data.

| Tool | Arguments and behavior |
| --- | --- |
| `participants.bind` | New: `endpoint_id`, `session_id`, `attachment_id`, `adapter_kind`, `provider_thread_id?`, `initial_cursor: beginning\|snapshot`, `expected_generation: 0`. Returns a member-owned binding at generation 1. |
| `participants.bind` | Replacement: additionally supply `participant_id` and its current `expected_generation`; omit `initial_cursor`. Same member/space/endpoint, a newly admitted owned Session and live self attachment. Preserves participant/cursor/intents/checkpoints, increments generation. |
| `participants.read` | `participant_id`, optional explicit `cursor`, `limit: 1..100`. Omitted cursor uses durable contiguous acknowledged position; `null` is invalid. Returns envelopes, non-null `next_cursor`, `durable_cursor`, `retained_from`, `gap`, `paused`, `binding_generation`. Reading saves inbox intent but does not acknowledge delivery. |
| `participants.receipt` | `participant_id`, `event_id`, `binding_generation`, `state: delivered\|handled\|blocked`, `provider_turn_id?`, `evidence_ref?`. Requires a known durable inbox intent and current exact owned Session; saves receipt and advances contiguous acknowledged position. No forum task state changes. |
| `participants.pause` | `participant_id`, `expected_generation`. Increments generation, stops new delivery admission, reports `provider_turn_cancelled: false`. Does not cancel a provider turn or stop a process. |
| `participants.resume` | `participant_id`, `expected_generation`. Trusted host must attest reconciliation and current ownership, then increments generation. Does not perform provider dispatch. |
| `work.checkpoint.get` | `participant_id`, `work_id`; missing checkpoint returns revision 0. |
| `work.checkpoint.set` | `participant_id`, `work_id`, `binding_generation`, `expected_revision`, `topic_id`, `assignment_revision` (current topic revision), `submission_id?`, arrays `completed`, `pending`, `unknown`, `artifacts`, `provider_resume_ref?`. Validates same-space topic and current revision/submission. Updates by SQLite CAS; no task transition. |
| `operations.get` | `participant_id`, `operation_request_id`. Reads this participant's durable inbox intents or confirmed facade receipts. No dispatch/replay mutation. Foreign Engine/SourceJobs request IDs return `Unsupported`. |

`pending` and `unknown` contain string operation IDs already admitted in this
participant's durable inbox ledger (`inbox:<stable-core-event-id>`). Arbitrary
IDs and source/forum IDs with no integrated read-only lookup return `Unsupported`.
These are bookkeeping references, not permission to repeat an external effect.
`completed` is caller evidence and never marks a forum task done. An opaque
`provider_resume_ref` is not provider memory restoration or a process handle.

All mutations support exact request replay. Current credential/space/endpoint
authorization is checked before replay. Receipt/checkpoint replay also requires
current generation/live owned Session. Pause/resume replay returns the original
result only while its resulting generation is still current. Reusing an ID with
different arguments conflicts. Old generations cannot publish new receipts.

## Trusted owned host boundary

`TrustedHost` is an explicitly trusted **in-process** dependency, not caller JSON
and not an HTTP capability assertion. An implementation must consult the actual
Runtime owned Session registry, verify identity, space, endpoint, attachment,
Session instance/generation, approved adapter/provider thread, and approved
execution profile. A caller-controlled implementation must never be installed.
The library cannot verify a lying trusted implementation; tests use a fake only
at this ownership boundary and do not prove actual provider capabilities.

`BindingContext` supplies the proposed exact binding, generation, previous
binding, and durable pending intents. `OwnedAdmission` supplies the actual owned
Session/instance/generation, adapter, exact provider thread, and host capabilities.
Mismatched Session/adapter/thread, absent live self attachment, zero host
generation, and missing host instance are rejected. Caller `capabilities` is an
unknown argument and cannot grant tools. Host capabilities remain informational;
they do not authorize Engine operations or establish workspace isolation.

Bind/rebind/resume require `reconciled: true`. When provider acceptance or marker
status is unknown, the host must refuse this attestation and keep the binding
paused or unavailable. Before admission, the adapter must prepare marker/turn
reconciliation outside the callback; the callback may only consult bounded local
registry facts. It must not do provider/network I/O, wait for events, or reenter
Engine/ParticipantService. A real adapter must retain unknowns rather than infer
absence from an incomplete marker query. This ledger never performs retries.

After restart the service can read durable inbox/checkpoint/operation history
without an active provider. Receipt/checkpoint mutation rechecks trusted ownership;
a changed host instance/generation requires explicit rebind/resume. The service
cannot recreate ownership from saved IDs. Explicit rebind can use a newly owned
Session while preserving old history; conversation restoration still requires
the adapter's exact-thread admission and reconciliation.

## Atomic authorization and concurrent writers

Every call uses public `Engine::with_endpoint_authorization`. Bind/resume/receipt/
checkpoint.set require current endpoint `send`; observation/pause/checkpoint.get/
operations.get require `observe`. Self identity is checked again in the ledger
transaction. Current role-derived and explicit endpoint grants are authoritative.

Lock order is **core mutex + core SQLite IMMEDIATE transaction → participant
SQLite IMMEDIATE transaction → bounded local host registry read**. No reverse
calls are allowed. The core transaction stays open until the ledger callback
returns, fencing another Engine connection's revocation commit. The participant
transaction commits its binding, receipt, cursor, checkpoint and request receipt
together. Two separately opened services/Engines share SQLite serialization,
not merely a clone-local mutex. Credential expiry is evaluated at admission;
this is a linearized admission fence, not a guarantee that time cannot pass.
This is not a distributed transaction with external provider effects.

## Inbox identity, cursor, retention

Core forum activity remains the authoritative forum. Event identity is its
persisted UUID, not content hashing or a delivery-specific UUID. Envelopes include
space/topic/revision/submission, provenance from that immutable activity event,
causation (original request), correlation (topic), timestamp and original activity.
An updated source event retains its own source version, original author and fetch
time, even when the topic retains its initial source origin. An event with no
provenance (including current ordinary forum comments and legacy events) returns
`{"kind":"unknown"}`; it never inherits external origin from its topic. Reads
repair older cached inbox envelope metadata from the authoritative event;
`operations.get` and the trusted host's pending-intent view normalize returned
envelopes without rewriting the ledger.
Every participant may observe the same event; delivery does not claim its task.

The facade converts current space-filtered append-only activity positions into
space-scoped string cursors and never returns `null` at end-of-page. New events
can be read from the previously returned end cursor. `beginning` includes existing
unclaimed work; `snapshot` explicitly opts into a captured boundary. Rebind never
resets that boundary. Acknowledging the second event first cannot skip the first.
Unacknowledged events remain readable after reopen, with their original IDs.

Core currently retains all activity, so `retained_from` is offset zero. A cursor
beyond available history returns `gap: true` and the available end boundary;
malformed/cross-space/null cursors are rejected. There is no retention pruning
in this slice. Any future pruning must replace offset semantics with a retained
sequence floor before it can remove activity. Paused participants may still read
history/save inbox intents; this is observation and never provider delivery.

## Verification and remaining integration

`cargo +1.89.0 test --locked --test participant_ledger` covers public facade
offline reopen/stable IDs, independent Engine/service concurrent CAS writers,
contiguous receipts/replay, same-space/self attachment isolation, revoke-before-
replay, explicit boundaries/gap, known-intent checkpoints, host generation
change, pause/resume and explicit replacement Session fencing. See
`.codex/implementation/participant-ledger/` for RED/GREEN records. Core cross-
Engine authorization/revocation fencing is separately tested in
`tests/participant_authorization.rs`.

Remaining required work: route this facade through the common HTTP tool entry;
install an actual owned adapter/credential store with provider-marker recovery;
integrate read-only Engine/SourceJobs receipts for foreign operation IDs;
connect lifecycle/draining/active workers to owned hosts; and verify real tools,
cross-identity review/test/writeback, second source/version and long idle time.
No model call, GitHub write, autonomous implementation loop, provider sandbox,
65-minute run or arbitrary-process takeover is claimed here.
