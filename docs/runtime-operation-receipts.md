# Read-only operation receipts

`OperationService::new(engine, source_jobs).execute(credential, &ToolRequest)` is the transport-neutral receipt lookup. HTTP routing and participant checkpoint references are separate integration work.

```json
{
  "request_id": "lookup-1",
  "tool": "operations.get",
  "space_id": "space-id",
  "args": {
    "endpoint_id": "endpoint-id",
    "source_kind": "forum",
    "operation_request_id": "original-request-id"
  }
}
```

`source_kind` is mandatory: `forum` selects Engine receipts and `source_job` selects SourceJobs. The ledgers can legally contain the same request ID. A missing or unsupported kind returns `Unsupported`; there is no precedence or cross-store guessing. Unknown arguments are rejected.

Every lookup authenticates the current credential and requires current membership in the requested space and current endpoint `observe` authority. Owner and space administrator roles retain the existing endpoint authorization semantics. Expired, rotated-away or revoked credentials do not authorize lookups. Invite tokens are not lookup credentials. A request ID alone never authorizes a caller.

Forum lookups select only receipts owned by the current identity. A second identity, including an administrator or owner, cannot borrow another identity's receipt. New receipts record server-owned `operation_scope` metadata: original tool, space, topic, optional endpoint and optional required dispatch role. Only `topics.create` and `task.act` receipts are exposed. Credential, invitation, membership and configuration receipts return `Unsupported` and are never hydrated. The original topic must still exist in the same space; current topic observation and any recorded required role are checked. If the original operation required an endpoint, that endpoint must still be observable as well as the query endpoint. A normal forum mutation through `Engine::execute` has no original endpoint; its trusted tool/space/topic metadata remains sufficient when the caller supplies a currently observable endpoint in that space. Legacy receipts lacking trusted scope return `Unsupported`; fingerprints and returned body text are not treated as authorization metadata.

Source lookups reuse `SourceJobs::get`, preserving its current identity and current resource authorization. The returned job must match both the requested space and endpoint. The service never calls `start`, `reconcile`, source adapters, provider/model APIs, or remote reads. There is no automatic resend. Job states `queued`, `running`, and `applying` map to `pending`. `completed` maps to `confirmed` only if there is no external write receipt or that receipt is confirmed. Other states, including a failed or uncertain write, map conservatively to `unknown`; the original job snapshot remains in `receipt`.

Successful forum receipts return `confirmed`. A selected store with no matching receipt returns `state: unknown`, `lookup: not_found`, and evidence explicitly limited to the current identity/local store/scope. This does not establish that an external operation was never sent and is not `not_admitted`. No inferred successful or failed receipt is manufactured.

The query request ID is not entered into deduplication or audit ledgers. Forum lookup performs only a read transaction and never dispatches or hydrates data. Source lookup performs existing authorization reads and `SourceJobs::get`; its authorization transaction rolls back without writes. Regression tests compare raw persisted core JSON, source job records and write receipts before/after, and compare gh process calls across repeated lookup and reopen.

Validation: `cargo test --test operation_receipts` exercises real Engine/SQLite and SourceJobs APIs in four combined scenarios: ordinary forum create/claim/comment receipts and two identities; original endpoint/role/grant/credential revocation; legacy/sensitive receipts; and source pending/confirmed/unknown recovery. The source scenario also checks cross-space and endpoint scopes, colliding IDs across the two stores, credential rotation and expiry, and confirmed external receipt persistence. Only the gh process boundary is fake: one POST loses its response and one returns a validated synthetic comment. The first stays unknown after restart; repeated queries make zero further GET/POST calls. Fake gh behavior is not evidence of remote GitHub correctness. HTTP wiring, participant checkpoint validation and provider autonomy require separate proof.
