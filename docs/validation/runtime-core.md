# Author validation: runtime core

Scope: `src/runtime_core.rs`, public library export, example, API documentation. This is author evidence, not independent prove-it or review. The v2 library is not wired into the existing daemon.

Environment: Windows, commit identity confirmed with git var: `Vakarve <16316583+TLOGBen@users.noreply.github.com>`. Base `f7ee28b`. Commands run in the dedicated `LinkStart-core-review` checkout. No production database was used; tests create disposable state.

## Vertical regression cycles

1. `cargo test --lib member_projection_preserves_role_and_explicit_grant_sources --locked`
   - RED: expected two authorization sources; only manager source was returned (1 failed).
   - GREEN: independent source entries retain both manager and explicit grants (1 passed).
2. `cargo test --lib member_projection_hides_endpoints_the_reader_cannot_discover --locked`
   - RED: `member projection leaked hidden endpoint id` (1 failed).
   - GREEN: projection filters endpoints using reader visibility; both projection regressions passed.
3. `cargo test --lib legacy_github_without_server_approval_cannot_dispatch_even_as_manager --locked`
   - Initial test request omitted required topic body and returned Invalid; corrected the test input before counting RED.
   - RED: direct authorization rejected legacy data, but restrictive dispatch returned Ok and imported a topic.
   - GREEN: the same transaction also requires server-owned GitHub approval before receipt replay; legacy data cannot create a topic.
   - SQLite is used only to construct an old-format migration input; all authorization and no-topic assertions use the public Engine.

`cargo test --lib --locked`: 24 passed, including restart/deduplication, racing claims, revoke-before-replay, scoped grants, immutable submission checks, owner-only settings, heartbeat leases, and readonly transaction behavior.

## Final author gate

- `cargo test --all-targets --locked`: PASS, 27 tests (24 new core library, 1 existing HTTP roundtrip, 2 existing lifecycle); binary/example test harnesses each contain 0 tests. No compiler warnings.
- Initial all-targets run exposed the pre-existing Windows lifecycle pipe EOF hang. A separate prerequisite commit repairs it; see [Windows lifecycle evidence](windows-lifecycle.md). Existing HTTP and lifecycle tests do not exercise v2 and are counted only as baseline regressions.
- `cargo run --example runtime_core --locked -- target/runtime-core-example-state`: PASS, public consumer reported reopened=true and deduplicated=true. The example was run again against the same directory to verify repeat execution.
- `git diff --check`: PASS. Only source, example and documentation are staged; no private state, owner token, database, build output or credential fixtures are included.

Independent prove-it/review are pending against the pushed exact head. These author results do not substitute for those gates.

 No model, native terminal, source adapter, daemon v2, or GUI acceptance is claimed.

## Storage follow-up after independent F1/F2 findings (2026-10-21)

This is author implementation evidence; independent prove-it/review against the new pushed head remain required.

- F1 public-boundary RED: a real Windows Users SID (`S-1-5-32-545`) explicit Read ACE on an existing token allowed `Engine::open` to succeed. GREEN: the same public call fails closed and token bytes are unchanged. Actual Windows ACL APIs also verify a new directory/token protected DACL and reject foreign explicit grants on an existing directory/database. Trusted granting SIDs are current user and LocalSystem only.
- F2 public-boundary RED: a short canonical token with no DB made every open fail. GREEN: recovery atomically publishes a fully flushed token while holding an OS initialization lock. Existing DB with empty/short/missing token fails without changing that token; restoring its original token reopens successfully.
- A separate worker process holds the actual OS lock and leaves a recognizable partial staged file. Three other processes invoke the public `Engine::open`; before the holder is killed they cannot publish a token or DB, and after termination they converge on one owner and deduplicated space. Parent-side polling sees only complete canonical token bytes. This constructs an interrupted filesystem state; it is not syscall-by-syscall crash injection into production and does not prove every power-failure order.
- `cargo test --all-targets --locked`: 34 top-level tests passed (24 core, 1 HTTP, 2 lifecycle, 6 storage regressions plus 1 process-worker entry). Child-worker runs are not counted again.
- Public `runtime_core` example passed twice against the same disposable state directory; owner credentials were not printed.
- Build output uses a dedicated external `CARGO_TARGET_DIR`. Moving an already-built target directory exposed stale compiled `CARGO_BIN_EXE` paths in HTTP tests; rebuilding this package restored the all-target gate. The failed run remains in the evidence bundle.
- Raw RED/GREEN/final logs and version manifest: main checkout `.codex/implementation/pr2-storage-fix/`; no token, DB or build output is committed. Windows tested; Unix execution, second-account login, unsupported/network filesystem semantics and power loss remain unverified.

## Minimum compiler follow-up after independent F3 (2026-10-21)

The storage OS lock uses `std::fs::File::lock` (stable since Rust 1.89). The package now declares `rust-version = "1.89"`, and the release workflow's shared `RUST_VERSION` is `1.89.0` for its Linux, Windows and macOS compiler consumers. The historical `rustc 1.88.0 (fixture)` provenance test value is unchanged because it is test data, not a compiler selection.

Author verification uses explicit `cargo +1.89.0` with a separate external target directory; no global toolchain default changes. Windows `test --all-targets --locked` passes all 34 top-level tests, and the public example passes twice against the same disposable directory. Exact-head logs and manifest are retained in the main checkout `.codex/implementation/pr2-toolchain-fix/`.

This validates the configured minimum on Windows only. It does not claim that the complete three-platform release workflow is green. Linux/macOS jobs and Python workflow steps are not rerun here; the separate known Windows Python v1 process-liveness issue is outside this toolchain-only patch. Independent exact-head proof/review remain required.
