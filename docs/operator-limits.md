# Operator limits

What the Runtime deliberately does not guarantee, and what an operator does
about it. Each item names the known issue it comes from in
`docs/progress/LEDGER.md`.

## Launch approvals (K21)

- **Approving a shell approves every command.** A terminal whose program is a
  shell (`pwsh.exe`, `cmd.exe`, `/bin/sh`) lets any agent with `send` on it run
  anything inside. Approve a shell only for agents you would let do that;
  otherwise approve the specific program.
- **A leaked credential: revoke first, then rotate.** Revoking a human
  administrator's credential (or removing `human_admin`) advances their trust
  generation, so every launch they approved stops counting, also after a later
  rotation. A rotation alone is routine and keeps their approvals, including
  ones an attacker made with the leaked credential.
- **Breaks before the upgrade are not recorded.** A demotion or revocation that
  happened before trust generations existed did not advance one; if such a
  person is restored, their pre-upgrade approvals count again. Re-approve with
  `endpoints.approve_launch` (or reinstall the plan) after restoring them.
- **Workspace write approvals** (`approve_workspace`) keep their approver
  without rechecking it; the plan's installer is rechecked at every launch.
- **An agent manager can revoke launch plans.** Revoking only removes
  authority, so it is allowed without a human; at worst every plan in the space
  stops launching until a human reinstalls it.
- **Revocation does not stop a running process.** A launch already admitted
  keeps running; the next launch is refused.

## `linkstart tool --address` (K19)

The Runtime proves it knows the caller's credential before the credential is
sent, bound to the Runtime's own address. What that does not cover:

- The proof does not bind the TCP connection that follows. Another program of
  the same OS user that shares the port (Windows `SO_REUSEADDR`) could take the
  connection; that user can already read the private state directory.
- Whoever holds a credential's stored hash can forge the proof for it. The API
  never returns stored hashes; keep `v2.sqlite3` out of logs and shared backups.
- `/v2/prove` tells a caller who already knows a key id whether that credential
  is still valid.
- The Runtime listens on `127.0.0.1` only. `--address [::1]:port` or
  `127.0.0.2:port` always fails the proof.

## Request receipts (K2, K22)

A terminal request_id is answered once: a resend gets the same reply, or
`UnknownEffect` if the first call may have acted. Each identity keeps its
newest 2,000 receipts for an hour; older ones become tombstones that still
refuse a rerun. Past that:

- Tombstones are shared by all identities and capped at 200,000. An identity
  that sends more requests than that can push out another identity's expired
  tombstone; a resend of that request_id afterwards runs again.
- An identity's expired receipts are pruned only when it sends again; an idle
  identity keeps at most 2,000.
- A request rejected for bad arguments before acting still answers
  `UnknownEffect` on a resend. Use a new request_id.
- `native-intents/` keeps one marker per terminal create or restart and is
  never pruned.

## Terminal operations share one gate (K1)

Mutating terminal operations run one at a time. Measured on Windows (debug
build, idle machine): create median 68 ms (max 83), stop 25 ms (max 37), force
14 ms (max 21). The bounds are longer — stop waits up to 300 ms for a program
that ignores Ctrl-C, create up to 5 s for a program that hangs while starting —
and another agent's write waits behind them; it is delayed, never lost.
`terminal.observe` and health do not wait on the gate.
