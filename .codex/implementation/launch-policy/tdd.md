# Launch policy bounded slice evidence

Base: origin/main b95fd623724c49cbbf7c698c89483eb02c83c080.
Worktree: C:/Users/jimts/workspace/Gits/Vakarve/LinkStart-launch-policy.
Branch: feature/runtime-launch-policy. Effective Git identity: Vakarve / 16316583+TLOGBen@users.noreply.github.com; no global config changed.

## RED to GREEN

1. Windows `cargo test --test runtime_launch_policy identifier_and_persisted_timeout_boundaries -- --exact --nocapture`: real behavioral RED. The 1ns timeout was nonzero but serialized as 0ms; install returned Ok(1), while the test required Invalid. See windows-red.log. This is distinct from the earlier unexported-module compilation RED in primary.
2. Fix: require an exactly representable integral millisecond timeout in 1..=u64::MAX; reject sub-ms residue and overflow. Bound plan/slot IDs to 1..=256 bytes, reject NUL. ID checks share the same test, but the RED stopped first on timeout; no separate behavioral RED is claimed for the ID bug.
3. Windows targeted suite: 11 passed, 0 failed; windows-green.log.
4. Ubuntu WSL targeted suite: 11 passed, 0 failed; linux-green.log. Unix private directory 0700 / policy DB 0600 asserted. Linux WorkspaceWrite returns Unsupported. Windows test restores typed WorkspaceWrite across reopen without downgrade.
5. Windows / Ubuntu `cargo check --all-targets`: passed; windows-check.log / linux-check.log. These compile all targets without running provider fixtures. No real provider execution occurred.

The suite uses real Engine authorization, SQLite and protected files with fake config. It covers independent worker linkage, actor manage vs trusted admin installation, owner cross-space rejection, stale revision/revoke/replay, missing slot, credential rotation and clock expiry after reopen, malformed documents, secret-bearing argv rejection, durable reopen and profile preservation.

New dead_code warnings are intentionally unsuppressed: ApprovedLaunch credential/facade fields and crate-only into_parts are unused until the later supervisor/control-bridge integration. Existing runtime_source BoundedProcessOutput.stderr warning remains. No functionality was removed to silence warnings.

Scope excludes supervisor/API launch integration, HTTP injection, request ledger dedupe, dispatch revocation races, lifecycle and shutdown. ApprovedLaunch resolves a current plan; the later consumer must revalidate actor and worker at actual dispatch. Same-OS-user modification remains inside the existing private-storage trust boundary; this is not a sandbox or an encrypted vault. Windows ACL protection uses existing Engine protect; this suite does not independently parse Windows DACLs.
