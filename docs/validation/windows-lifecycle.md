# Windows lifecycle gate repair

This prerequisite is separate from the v2 library. The baseline lifecycle test used Linux `/proc` on Windows; the parent checkout's native `OpenProcess`/`GetExitCodeProcess` liveness check and fixture cleanup are retained here. Liveness assertions were not weakened.

The first full all-targets execution stalled in `control(... daemon start).output()`. A phase trace plus an 8-second output-completion bound reproduced the failure: `control start` was printed, but no `control returned`. Cleanup stopped only that test's recorded daemon. Null stdin and DETACHED_PROCESS flags alone each reproduced the same timeout and were discarded as fixes.

The installed Rust 1.99.1 standard library source calls `CreateProcessW` with handle inheritance enabled by default. Its `inherit_handles(false)` and extended attribute APIs remain nightly-only ([Rust CommandExt](https://doc.rust-lang.org/stable/std/os/windows/process/trait.CommandExt.html)). Microsoft documents how [handle inheritance](https://learn.microsoft.com/en-us/windows/win32/sysinfo/handle-inheritance) keeps inherited handles available in children.

The Windows-only fixed daemon launcher now calls CreateProcessW with `bInheritHandles = FALSE` and `CREATE_NO_WINDOW`. It quotes its literal application/argument vector for the Windows C runtime, inherits the ordinary environment/current directory, creates no console, owns the process handle through `OwnedHandle`, and closes the initial thread handle immediately. It does not change process-global inheritance flags. Unix retains Command.spawn. This narrow helper deliberately does not implement arbitrary env/cwd overrides.

Validation command:

```powershell
cargo test --test lifecycle_cli --locked -- --nocapture
```

GREEN: 2 passed in 5.33 seconds. Trace showed both start and restart returned; the original assertions verified PID replacement, old process exit, new process liveness, HTTP ready, explicit stop, and discovery record removal. The occupied-port test still returned one failure JSON value.

The output wait is bounded in the regression test, so future inheritance regressions report the blocking stage instead of hanging indefinitely. This proves the author's Windows test environment; other supported OSes still require their own CI verification.
