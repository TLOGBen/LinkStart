# Runtime-owned Codex host

`runtime_codex_host` provides a synchronous, Runtime-owned Codex app-server
0.163.0 stdio transport. It does not decide what an Agent should claim, implement,
review or publish. It does not start a model merely by opening the registry or
initializing the app-server.

## Trusted caller contract

1. Open `OwnedCodexRegistry::open(engine_private_dir, engine)`. Storage must be
   the same canonical private directory as the Engine. The host SQLite ledger,
   WAL/SHM and lock files use the existing Runtime storage ACL/mode helper.
2. Construct trusted `OwnedHostConfig` with an approved executable/arguments,
   canonical workspace, explicit model, bounded timeout, `HostScope`, and profile.
   This is an in-process management interface, not deserializable HTTP input.
   Scope identifies member/space/endpoint/live attachment/session/binding generation.
   `spawn(config, member_credential, Arc<dyn RuntimeToolFacade>)` checks actual
   current Engine identity, live attachment and endpoint send permission, then
   creates and holds the actual child. Windows uses CREATE_SUSPENDED, assigns the
   existing TerminalManager no-breakaway Job, then resumes the sole initial thread
   only when ResumeThread reports previous suspend count 1. No child user code can
   spawn descendants before assignment. Assignment/enumeration/resume failure kills
   and waits for the held child; snapshot/thread handles close through RAII.
   These are the documented [creation flag](https://learn.microsoft.com/en-us/windows/win32/procthread/process-creation-flags),
   [thread snapshot](https://learn.microsoft.com/en-us/windows/win32/api/tlhelp32/nf-tlhelp32-createtoolhelp32snapshot)
   and [ResumeThread](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-resumethread) APIs.
3. `initialize()` checks the observed app-server `userAgent` version 0.163.0.
   Workspace-write additionally requires observed Windows platform fields.
   `start_owned_thread()` creates one durable exact thread with fallback disabled.
   The accepted model, canonical cwd, approval policy and sandbox must match.
4. `start_turn(operation_id, text)` persists an unknown operation before sending
   one request, and puts its stable marker in the input. `pump(timeout)` routes
   subsequent messages while the application retains the host across idle periods.
   An ordinary bounded poll without a message returns `HostError::Idle`; it does
   not turn a known active provider turn into an unknown mutation.
   There is no default idle expiration, background scheduler, or automatic retry.
5. `interrupt()` sends exact known thread/turn IDs. `close()` closes stdin, waits
   briefly, terminates the held Windows Job if necessary, waits on the held child,
   and checks that the Job has no active processes. `is_exited()` checks these
   held objects, not a recycled PID. Locks remain held until the host is dropped.

Model selection is explicit configuration. Tests use only the model string
`gpt-6-luna`; they never call a model. Unknown, unsupported or changed models do
not cause an expensive fallback. Provider launch arguments must come from the
trusted launcher. In particular it must not retain a shell-disable launch flag
when native command tools are approved for workspace work. Accounts, provider
configuration, configured MCP servers and inherited environment are deployment
configuration; this transport does not turn arbitrary preexisting tools into
approved capabilities.

## Profiles and real forum facade

`ReadOnlyForum` requests provider filesystem `read-only`, disables native shell
via thread config, and validates effective readOnly/never/cwd/model/network.
The name refers to the filesystem profile: it does not remove authorized forum
mutations. `EngineForumFacade` supports identity/current, forum/topic/activity/
member/endpoint reads, `topics.create` and `task.act`. Reads require endpoint
observe; mutations require send, with normal Engine role/action/revision and
request replay rules. Every callback rechecks credential, space, identity and
live attachment. Cross-space calls, members administration and unsupported tools
are rejected. Provider errors/tool denials do not echo credentials or detailed
authorization internals to the model.

For native repo editing/testing, obtain an `ApprovedWorkspace` from
`registry.approve_workspace(admin_credential, space, endpoint, root, version,
capability_evidence)`, then use `ExecutionProfile::WorkspaceWrite(approval)`.
The approval has private fields and cannot be deserialized from caller JSON.
The management entry checks current owner/admin/manager authorization; it does
not use a participant's assertion as a workspace grant. Version and evidence
are administrative policy; runtime-observed initialize fields and effective
thread sandbox are separate capability checks. Windows Job containment is
required but is not presented as a filesystem sandbox. Other OS/version write
profiles return Unsupported.

The write profile requests native `workspace-write`, shell tool enabled, network
disabled and approval never. It checks the effective canonical cwd, exact model,
workspaceWrite sandbox, no network, and no additional writable roots. Empty
writableRoots means the provider's normal cwd policy. Temp exclusions may be
false: this follows the verified provider policy and **does not promise that
only cwd is writable**, universal symlink safety, or an independently proven
filesystem security theorem. All approval/server methods other than the injected
dynamic tool are denied; this host never grants escalation. Capability evidence
and the exact approved profile are stored with private thread ownership.

## Dispatch and ownership fencing

Messages with both method and id are server requests even when their ID equals
an outstanding client request. They never satisfy the client response waiter.
Notifications are retained in a bounded backlog. A callback must be
`item/tool/call` for unnamespaced `linkstart_call`, the exact owned thread,
currently observed/accepted turn, current host instance and binding generation.
The trusted Runtime injects a facade rather than giving a bearer to the provider.
The facade executes outside the ParticipantService admission callback.

`OwnedCodexRegistry` implements `TrustedHost` by bounded local facts and a held
child handle check. It never does provider I/O, calls Engine, or opens SQLite
inside `admit`. It checks exact identity/space/endpoint/attachment/session/
adapter/thread/binding generation and reconciliation. Session OS locks and
separate exact-thread OS locks prevent concurrent hosts, including a second
session trying to resume a live thread; durable session generations fence
publication after restart.

The lifecycle integration must call `fence_binding(session, expected_generation)`
**outside every Engine/ParticipantService lock**, before publishing pause/rebind/
shutdown. It closes new callback admission first and drains already admitted
callback leases before returning success. Such callbacks may finish their Engine
mutation during the drain, but none remains admitted after successful return.
Provider interruption/close is a separate operation. The host object must
be retained and pumped by Runtime; this module does not yet connect HTTP routing,
daemon draining/active counts or automatically observe ParticipantService changes.

The default drain budget is 5 seconds. `fence_binding_timeout` accepts a caller
budget capped at 30 seconds. On expiry it returns `HostError::Pending` with the
active callback count, keeps new admission fenced, and does **not** authorize
publishing paused/stopped. Retry with the same original expected generation;
the retry does not increment generation again. A different expected generation
is rejected. Callback leases release through RAII during normal return and panic
unwinding. This is a drain boundary, not cancellation of an admitted mutation.

Lock order remains core mutex + core SQLite IMMEDIATE → participant SQLite
IMMEDIATE → bounded facts/held-child inspection in `TrustedHost::admit`.
Dynamic dispatch briefly takes facts to acquire a callback lease, releases facts
before calling the facade/Engine, and briefly takes facts again when releasing
the lease. No facts guard crosses an Engine call. Fence holds no core/participant
lock and its Condvar wait releases facts, allowing the callback to commit and
drop its lease. `admit` never waits for a drain or calls provider I/O. A lifecycle
caller must never run fence while holding the Engine locks needed by a callback.

## Unknown and exact recovery

Timeout, malformed/unknown response or EOF after sending a mutation leaves the
same operation ID unknown. There is no resend. Accepted but unfinished operations
also block a fresh process from starting replacement work.

A new owned process may call `resume_owned_thread(exact_id)` only when the private
durable ownership record matches member, space, endpoint, attachment, workspace,
model and exact approved profile. A new Runtime Session/binding generation is
allowed; it never means restoring the old process. External threads return
Unsupported before a provider resume request. Session/thread locks prevent dual
ownership. Effective resume properties are verified again.

`read_exact_thread()` returns read-only evidence. `reconcile_operation(id)` looks
for one exact operation marker in a userMessage of that exact thread. A positive
match records the accepted turn and known status. Missing, duplicated, malformed,
truncated or unsupported history remains Unknown, without inference from absence
and without creating a replacement thread. The schema's deprecated includeTurns
can omit history; pagination integration remains necessary for those unknowns.
Reconciliation updates the addressed operation's durable state from its observed
status. Completing an old operation does not clear another active operation's
thread/turn routing, and that other operation remains interruptible.

## Evidence and remaining work

`cargo +1.89.0 test --locked --test owned_codex_host -- --test-threads=1` exercises
the actual process boundary with a local fake app-server, actual Engine/SQLite
forum transitions and held Windows Job cleanup. Fake tests are not evidence of
model decisions or provider sandbox enforcement. Evidence lives under
`.codex/implementation/owned-codex-host/`.
Stage-1 review repairs and subsequent gates are preserved separately under
`.codex/implementation/owned-codex-host-repair-stage1/`; original gates and real
provider proof snapshots remain unchanged. The fake server ships at
`tests/fixtures/owned_codex_server.py`, using python on Windows and python3 on Unix.
The startup regression starts a real descendant before initialize and verifies
exit with a held process handle plus matching creation time, rather than PID-only
polling. The participant lifecycle regression uses the actual ParticipantService
and owned registry through pause, prospective generation resume, replay, receipt
and checkpoint.

Installed-provider protocol/workspace feasibility is separately recorded under
`.codex/prove-it/records/`; it does not prove this product host end-to-end.
Required integration remains: authenticated HTTP/SDK and participant lifecycle,
approved production launcher/profile configuration, real model callback/native
repo work, source/remote authorization and receipts, long idle elapsed proof,
second source/version and interrupted continuation. No autonomous full-loop or
65-minute claim is made by this slice.
