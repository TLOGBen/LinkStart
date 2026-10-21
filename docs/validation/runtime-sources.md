# Author validation: source library

This records the author delivery slice, not independent prove-it or review.
Base: `de1c5e2` (merged durable runtime core). Product allowlist:
`src/runtime_core.rs`, `src/runtime_source.rs`, `src/runtime_source_jobs.rs`,
`src/runtime_watch.rs`, and three module exports in `src/lib.rs`. Consumer/docs:
`examples/runtime_sources.rs`, `docs/runtime-sources.md`, this record, and two scope
paragraphs in `docs/runtime-core.md`. No dependency/lockfile, HTTP, GUI, native,
history, agent runner, or Python adapter change is part of this slice.

## Provenance and original behavior cycles

The four source files were copied from the implementation checkout at fixed hashes,
then formatted with Rust 1.89.0. Original and final hashes, original TDD record
references, command logs and exact-head manifest are retained in the main checkout
under `.codex/implementation/source-library-pr/`; private state is outside this PR.
The core delta relative to base contains only source projection/provenance and
canonical repository comparison. Public role/endpoint entry points retain their
existing current-authority-before-receipt checks.

Original author RED/GREEN records are `source-tdd.md`, `source-jobs-tdd.md`,
`watch-tdd.md`, `source-provenance/result.md`, and
`source-provenance-case-fix/result.md` in the implementation evidence directory.
They cover ingestion/dedupe, explicit write policy, durable Unknown intent and
GET reconciliation, bounded subprocesses, authorized local projection, immutable
source revisions, checkpoint resume/older revision dedupe, revocation, restart,
protected origin receipts, case canonicalization, and explicit watch lifecycle.
They are development history, not proof of this PR's exact commit.

## Author gates

Precommit Windows verification with `rustc 1.89.0 (29483883e 2025-08-04)`:

- `cargo +1.89.0 fmt --all` followed by clean formatting check.
- `cargo +1.89.0 check --all-targets --locked`: PASS.
- `cargo +1.89.0 test --all-targets --locked`: PASS, 55 top-level tests: 45 library
  (24 core, 7 adapter, 10 source jobs, 4 watch), 1 existing HTTP, 2 lifecycle,
  7 storage integration. Child process test invocations are not counted again.
- `git diff --check`: PASS. Explicit file staging excludes owner tokens, databases,
  generated state, build output and credentials.

The shared bounded-process helper's `stderr` field generates one unused-field
warning because this slice excludes its future model-runner consumer. The field and
bounded stderr draining remain intact; no unnecessary dependency was added.

After committing, rerun formatting/check/all-target tests against that exact head
and run the public `runtime_sources` consumer against `TLOGBen/LinkStart` twice with
the same protected disposable state directory. The exact-head external manifest is
the authoritative run record. The consumer uses real GitHub GETs only, stable job
replay/checkpoint resume, protected topic reads, explicit watch control and shutdown
accounting; it never sends a remote comment. Public library tests use native local
PowerShell fake gh processes, not real GitHub permissions or writes.

## Unverified boundaries

Independent exact-head prove-it/review remain required before merge. Remote comment
writes, actual model execution, continuous Agent work, complete source HTTP/GUI
integration, source-to-Agent dispatch, GitHub rate-limit behavior, Linux/macOS test
execution, multi-daemon fencing, arbitrary process-tree containment and power-loss
recovery are not claimed. Existing v1 HTTP regressions do not establish source HTTP
integration. Prior dirty implementation binaries/live-chain records are not exact-
head evidence for this library PR.

## Follow-up: retained F1/F2 from independent review

F1 RED used public SourceJobs sync then Engine topics.read after changing only the
fake upstream title. The second source comment existed but omitted the new title.
GREEN adds the title to the immutable revision comment; the regression also checks
original topic title/body/provenance/workflow and the human comment remain intact.

F2 RED paused real public start admission on a separate SQLite BEGIN IMMEDIATE lock;
the actual busy handler confirmed admission reached the locked storage boundary.
Cancellation returned and observed zero before that earlier admission could commit,
count and spawn. GREEN serializes start through durable commit/accounting/spawn with
cancel_all and active_count using one admission mutex. RAII worker accounting and
bounded process cancellation remain unchanged. The test releases the real DB lock,
joins both callers, drains workers, and checks post-cancel start is rejected; no
product timeout was increased and no fake SourceJobs/Engine was substituted.

Both targeted REDs failed for the reported behavior and both GREENs pass. Follow-up
logs and the new exact-head manifest live in the main implementation checkout under
`.codex/implementation/source-library-fixes/`. The new all-target gate has 57
top-level tests (47 library plus the unchanged 10 baseline integration tests).
The final status-save error lead was inspected but is not part of these fixes;
failure injection and its behavior remain unverified. Existing live-write/model/
platform limits above remain in force. Independent gates must review the new head.
