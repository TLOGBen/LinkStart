# Member root credential contract

`members.rotate_credential` accepts `member_id`, `expected_epoch`, and optional
`ttl_seconds` (integer 1..2592000). Omitted TTL means no expiry.
`members.revoke_credential` accepts `member_id` and `expected_epoch`.
Both require the target's space and current manager/human_admin authority or the
protected local owner credential. Human identities additionally require a current
human-kind human_admin or owner. Epoch mismatch rejects with Conflict.

Rotation advances epoch and returns that generation's credential; revocation
advances epoch without returning a credential. Explicit rotation restores access.
Every dispatch authenticates the current persisted credential before receipt replay,
reads, heartbeat, or mutations. Expiry uses the Engine clock; heartbeat cannot renew
it. Identity, roles, claims, and history survive. Legacy members and issuance markers
default to epoch 0 with unchanged derivation and no expiry. Receipts hydrate only
their originally issued generation, never the latest key. Persisted state contains
verifiers and server-owned generation markers, not generated raw bearer values.

Role and credential mutations preserve the same currently active administrator
set. Non-owner callers cannot remove the last usable administrator's authority,
revoke it, or give its credential a finite expiry without another active
administrator. Protected local owner credential operations can override this for
recovery. Handoff is checked at mutation time: a valid handoff may expire later,
requiring protected owner recovery. No permanent-admin or auto-renewal policy is
introduced.

Each successful rotate/revoke returns an `audit_event`, persisted in a separate
serde-default `credential_audit_events` ledger in the same Immediate transaction as
the credential update and receipt. Fields are id, space_id, actual actor (member ID
or owner), target_member_id, action, epoch_before, epoch_after, request_id,
Engine-clock created_at, and owner_recovery. Failed intents and receipt replay do
not append events. Audit metadata excludes generated bearer/verifier values and
forum authors/topics. Caller-controlled request_id is not redacted. This slice
adds no ledger browse API/UI and makes no crash/power-loss injection claim.

This change depends only on the persisted Engine core and native history already
in main. Source provenance, GitHub adapters, HTTP, UI, participants, and capability
credentials remain outside this slice.
