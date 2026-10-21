# F2：actor admission 先於 private slot 與 worker preflight

- 起始 HEAD：`6039c405b19e6f140f2b5f3cc6209c85222f9c90`；工作目錄：`LinkStart-launch-policy`，開始時 `git status --short` 空白。
- 依據：primary `LinkStart/.codex/review/launch-policy-v2-stage1.md` 與 `launch-policy-v2-verdict.md` retained F2。target 無 `AGENTS.md`，遵循 caller 提供的環境指示。
- 產品變更只有 `src/runtime_launch_policy.rs`：`install` 在 slot lookup、worker 驗證、filesystem canonicalization、profile restore 前，以目前 actor 的 exact endpoint `manage` + trusted administrator role 做無副作用 admission；`provision_worker_slot` 也在 worker 驗證前做相同 admission。
- 兩條路徑原有 mutation-time `admin` callback 均保留；worker/slot 驗證留在 callback 外，避免 recursive Engine lock。scoped composite keys、migration、immutable worker slot、worker 的角色規則均未更動。

## TDD

- 先只加測試、保留原產品 source，執行 `cargo +1.89.0 test --test runtime_launch_policy admission_precedes --locked`。
- `f2-fix-red.log` 是真正 regression RED：`install invalid/absent` 得到 `WorkerCredentialRequired / provision independent worker credential first`，應為 actor admission 的 `Unauthenticated / invalid credential`；provision 的 cross-space caller 搭配 invalid worker 得到 worker 的 `Unauthenticated`，應為 actor 的 `Denied / space membership required`。兩個 test 均 failed，exit 101。
- 最初測試 fixture 遺漏 `unrelated_state`，其 setup error 原樣保存為 `f2-fix-test-setup-error.log`，不作產品 RED 證據。修正 fixture 後才建立上述真正 RED，之後才改產品 source。
- `installer_admission_precedes_private_slot_and_preflight_observation`：invalid actor、cross-space administrator、同 space 無 manage 的 worker、已 revoked manager，逐一比較 absent slot、existing wrong linkage、existing exact linkage、invalid filesystem preflight 的完整 code/message，16 組；每次拒絕後比較 private policy DB 的 slots、plans、revision、revoked、document、credential 與 unrelated sentinel。
- `provisioning_admission_precedes_worker_validation`：相同四類 actor，absent/existing slot × valid/invalid worker credential，16 組；比較完整 actor admission error 與相同 storage snapshot。
- 原有 manager-as-worker、scoped namespace、migration、CAS/revoke、credential/linkage 與 timeout 測試保留。

## 驗證與界線

- `cargo +1.89.0 test --test runtime_launch_policy --locked`：PASS，21 passed，0 failed，見 `f2-fix-green.log`。
- `cargo +1.89.0 test --all-targets --locked`：PASS，exit 0，見 `f2-fix-all-targets.log`。所有 test binaries 與 examples 終態成功；其中 history_authority 保留 1 個既有 ignored case，不將它列為已執行。
- `cargo +1.89.0 fmt -- --check`：PASS，exit 0，見 `f2-fix-fmt.log`（無輸出）。`git diff --check`：PASS。
- Toolchain：rustc 1.89.0 (29483883e 2025-08-04)，cargo 1.89.0 (c24e10642 2025-06-23)。Windows 原生 default features，沒有 `--no-default-features` 或 feature substitution。
- Source blob：`src/runtime_launch_policy.rs` = `fbac19cd54c7a5324832d86da99852110242f57d`；`tests/runtime_launch_policy.rs` = `9b52f4f0414dfcfa54569cca21afd65891ae8e55`。
- 這些是 library/Engine/SQLite 回歸證據；沒有 provider/model 呼叫，不主張 HTTP/provider launch integration、dispatch-time revocation 或其他平台的新證據。獨立 exact-head review 由本修正後另做。
