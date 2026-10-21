# Native terminals and persisted history (library)

This stacked change depends on `codex/runtime-core`. It exports
`runtime_terminal` and `runtime_history` together; their recording integration
requires both modules. It adds no HTTP adapter, UI, provider agent or model call.

Run the public consumer on Windows with an unused state-directory path:

Rust 1.89 or newer is required (`std::fs::File::lock`).

```powershell
cargo run --example native_history --locked -- C:\scratch\linkstart-native-example
```

The example starts two real `cmd.exe` sessions through ConPTY, checks exclusive
control and stale-generation rejection after release/rejoin, observes computed
answers absent from input, exits both owned trees, flushes history, independently
reads the JSONL/checkpoint files, then reopens the store and reads both histories.
It retains its newly created state directory for inspection; do not point it at
an existing runtime state. On failure, dropping its manager cleans up its owned
Windows Job Objects. No unrelated processes are selected by name or PID.

## Trust and ownership

These are trusted in-process library APIs. An `identity` string is a controller
label, **not authentication**. A lease fences cooperative writers but does not
authorize endpoint access. `TerminalManager` does not consult `Engine` scopes.
Its create, observe, stop and force methods grant the host process native command
execution/control. A host must authenticate callers, authorize each endpoint and
operation, derive identity from trusted context, and serialize revocation against
mutations before exposing any operation to untrusted callers. HTTP permissions,
credentials, shutdown tickets and transport integration are follow-up work.

Windows uses a gated PowerShell launcher and a Job Object to own descendants;
`process_id` identifies that launcher/root. `stop_owned` queues Ctrl-C and may
return `stop_pending`; `shutdown` returns sessions still pending/failed after a
shared graceful budget. Force is explicit. Check actual session exit and then
`HistoryStore::shutdown_flush` before reporting complete shutdown. Input timeout
means delivery unknown and must not trigger automatic retry. This is not an OS
sandbox. Unix process-tree force cleanup is unsupported; the public example is
Windows-only.

Release and lease expiry reject future input admission. They do not cancel or
recall an input request already accepted into the bounded native writer queue.
An accepted request can complete after its caller times out or releases control,
including after another controller acquires a new lease. The host must treat a
timeout as unknown delivery and must not promise that release cancels pending
native input. Force/shutdown likewise cannot retract bytes already delivered.

## Persistence and limits

Use `create_recorded` to enable history; `create` deliberately remains unrecorded.
Raw output bytes are authoritative; text is lossy UTF-8 convenience. Output and
history retention are bounded and disclose gaps. History event cursors differ
from raw byte cursors. Live `flush` covers already queued entries; shutdown flush
rejects new recording and requires reader EOF. Checkpoint failures remain errors;
after repairing a checkpoint target, a new successful barrier may recover it.
Journal append/rotation failures require storage repair and runtime restart.
Opening a new store fences older writers using a root writer-instance record.
Every root handoff/recovery, session registration/retirement, journal append,
rotation and checkpoint replacement uses the same OS file lock while checking
current writer authority. This also serializes cooperating processes sharing the
root; it assumes all writers use this library and the filesystem honors OS locks.
The lock is scoped to the canonical connection root, including in-process reads
and writes. Waiting for an unrelated root's OS lock does not hold a shared
process-wide gate. There is no static registry of roots to retain abandoned
entries; canonical path aliases still reach the same underlying OS lock file.
Registration and retention run on the history writer thread and synchronously
return their result; no queue reply is awaited while holding mutation authority.
The lock file is persistent and must not be removed/replaced while stores exist.
Filesystem/lock I/O can block; this does not add an unconditional wall-clock
deadline to registration or OS I/O. Reader output enqueue remains nonblocking.
Retention requires a closed lifecycle, reader EOF and zero queued/in-flight output
chunks for current sessions, then flushes before removing a journal. An admission
queued before the reader finished can return capacity occupied until those later
chunks drain; the host may retry admission. Process exit alone is not reader EOF.
A public sink producer must finish its output before close_output.

Persisted history is recovery evidence. `process_resume` remains `unsupported`;
neither native process state nor provider conversations are resumed. State uses
the current OS-user trust boundary, and raw output can include secrets printed
by the child. A host must choose and protect its storage directory accordingly.

The extracted modules retain their original TDD tests, including the later
checkpoint-recovery corrections. A fresh build, locked all-target checks/tests
and this example provide author verification for this branch. Prior HTTP proof
belongs to a different dirty checkout/binary; it is not proof of this library PR
head. Independent review/proof, long idle reliability, power loss, hostile
same-user tampering and all interleavings remain outside this delivery's claims.
