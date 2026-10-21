# PR 9 F1 correction and verification

Starting head: 010de9ed9478cc839e2258812fa1869d72422fd9. Changes are confined to the existing launch-policy worktree; primary was not modified. No host helper changes, role restrictions, provider/HTTP integration or unrelated data migrations.

## Actual RED

- `cargo test --test runtime_launch_policy scope_namespaces -- --nocapture`: both cross-space and same-space/different-endpoint cases failed. Missing returned `(LaunchApprovalRequired, trusted launch plan required)` and foreign returned `(Denied, plan target mismatch)`. See f1-plan-red.log.
- `cargo test --test runtime_launch_policy worker_slots_cross -- --nocapture`: both scopes failed because an independently authorized worker slot with the same ID collided with the other scope's global ID. See f1-slot-red.log. This confirms the verifier's worker_slots lead through actual API calls, without banning manager identities as workers.
- instant-red.log records a **test assumption failure**, not a demonstrated product overflow: Windows Instant::checked_add can represent u64::MAX milliseconds. The corrected test follows the platform's actual result. It does not pretend every platform rejects that duration or claim long provider execution.
- f1-initial-suite.log retains an intermediate suite failure: the previous owner/cross-space test still expected Denied. The scoped lookup correctly returned LaunchApprovalRequired; that expectation was updated to the missing result. No failure log was hidden or relabeled as a passing gate.

## Narrow correction

Both tables now use `(space_id, endpoint_id, id)` as primary key. Every runtime SELECT, INSERT/UPSERT, replay lookup and revision/revoked CAS includes the exact authorized scope. The JSON's exact worker/plan binding check remains defense in depth; it is no longer the namespace. A trusted actor may install its own same-name plan/slot without reading or mutating the other scope's rows.

Schema version 2 migration is one SQLite IMMEDIATE transaction: detect original unversioned/v1 tables, rename both, create scoped tables, copy rows with scope derived from strict stored binding/document, preserve original JSON/binding/credential bytes and revision/revoked, drop legacy tables and publish version 2 at the same commit. A malformed row aborts and rolls back both DDL and already-copied rows. Unrelated table contents stay unchanged. Unknown future user_version fails closed. Version 2 repeat open does not rewrite rows. Migration fixtures only touched isolated tempfile databases; no real user state was migrated by these commands.

Install and approve additionally check Instant::now().checked_add(timeout), retaining exact integral milliseconds, u64 persistence bounds and 1..=256-byte/NUL-free IDs. Platform-supported extreme deadlines remain supported; no arbitrary short timeout cap was added. The future consumer still owns dispatch-time deadline/current-authorization checks.

## GREEN gates

- Windows `cargo test --test runtime_launch_policy -- --nocapture`: 19 passed, 0 failed. f1-windows-green.log.
- Ubuntu WSL same targeted suite: 19 passed, 0 failed. f1-linux-green.log.
- Windows `cargo test --all-targets`: passed, including launch-policy 19/19. f1-windows-all-targets.log.
- Ubuntu WSL `cargo test --all-targets`: passed, including launch-policy 19/19. f1-linux-all-targets.log.

The scoped regression cases compare approve and revoke code **and message** for missing/foreign IDs, install an independent same-name plan, and directly assert the foreign document/revision/revoked tuple is unchanged. Worker-slot cases provision the same ID in separate spaces/endpoints and resolve both exact credentials. Migration tests cover original user_version 0 and explicit 1, active revision 2, revoked revision 2, byte-preserving row snapshots, repeated v2 open, malformed legacy plan after worker rows were copied, complete rollback including no leftover renamed tables, unrelated state and future version 99 rejection without state mutation.

The all-target gates execute existing fake-provider/owned-child fixtures and temporary daemon/CLI tests, but no real Codex/Claude provider, no HTTP owned launch flow and no external side effects. Existing dead-code warnings remain honestly visible: ApprovedLaunch credential/facade and into_parts are awaiting the later consumer; runtime_source.stderr is pre-existing. FullUnifiedRuntime, lifecycle, provider containment and use-time revocation remain outside this slice.
