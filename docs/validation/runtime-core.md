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
