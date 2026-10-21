# Runtime launch policy 與 private worker linkage

此模組只安裝與解析可信本機 plan，沒有 provider spawn、HTTP route 或 lifecycle。HTTP 後續只能提交 plan ID／revision；`TrustedLaunchPlan`、`ApprovedLaunch` 不可 Deserialize。`ApprovedLaunch` 與含 worker credential 的內容不可 Debug／Serialize。Plan ID／worker slot ID 限 1..=256 bytes 且不可含 NUL；timeout 必須能精確表示為 1..=u64::MAX 毫秒，且本機 `Instant::checked_add` 可表示；sub-ms 值、餘數或不可表示 deadline 直接拒絕。不以任意短秒數代替平台檢查，也不宣稱已跑過超長 provider lifecycle。

可信安裝流程：既有 invite/register/heartbeat/grant 完成後，Runtime 呼叫 `provision_worker_slot` 保存 exact member／space／endpoint／attachment 的獨立 worker credential；再用 `install` 安裝 canonical executable、固定 argv、workspace、model、timeout、typed profile。只有目前 RuntimeOwner 或同 space 的 human_admin／manager，且具 endpoint manage，能安裝／撤銷。worker 必須目前 send + exact alive attachment；RuntimeOwner 不能當 worker。slot immutable：既有 slot 不能靜默換 credential，失效時回 WorkerCredentialRequired。明確重新 provision 使用新的 slot ID 及新 plan revision。

`approve` 是解析已安裝 plan，不是 HTTP approval。caller 需要目前精確 space／endpoint manage；不要求 caller 是 plan 安裝者，也不借用原管理者 credential。worker credential 另外驗證；Actor 與 worker 可不同。plan、revision 與 revocation 使用 SQLite；安裝同文件且未指定 previous revision 可 replay，更新要求 exact previous revision；revoke 推進 revision，舊安裝 replay 不能恢復。每次使用重新檢查目前權限、worker credential、attachment、canonical paths、revision 與 profile。Host spawn／supervisor dispatch 還要重新驗 actor 與 worker；`ApprovedLaunch` 不是永久授權 snapshot。

SQLite `v2-launch-policy.sqlite` 放在 Engine 同一 private directory，沿用既有 ACL／Unix permission 保護；public plan document 只保存 worker slot reference，不含 credential。private worker_slots 才保存 credential；此為同 OS 使用者保護，並非對相同 OS 身分的 sandbox。DELETE journal 與 transient journal 均位於受保護 parent。沒有通用 vault、credential 自動輪替或從 hash 還原。

Plan 與 worker slot 都以 `(space_id, endpoint_id, id)` 為實際 primary key；所有查詢、更新、replay 與 CAS 均使用完整 scope。同名 ID 可在不同 space 或同 space 不同 endpoint 獨立存在；其他 scope 的同名資料不影響本 scope 的 missing 結果或安裝。Schema v2 會在單一 SQLite IMMEDIATE transaction 中遷移原始無 version／v1 schema：保留原 JSON、credential、revision、revoked，從原 binding 導出 scope，最後一起發布 version 2。Malformed legacy row 會 rollback 整個遷移，保留原兩張表與其他資料；未知未來 version 拒絕開啟，不刪除重建 DB。v2 重複 open 不重寫 row。

WorkspaceWrite 只能來自 host `approve_workspace` 的 typed `ApprovedWorkspace`。持久化後用 crate-private restoration helper 驗證 exact canonical root／space／endpoint、0.163.0 version、非空 approved_by／capability evidence 與嚴格欄位，再還原 sealed capability。原批准紀錄是持久證據，並非原批准者的永久 token。非 Windows 明確 Unsupported；任何錯誤都不降級 ReadOnlyForum。

本 slice 測試使用 real Engine／SQLite／private files，fake config，完全不 spawn provider。HTTP injection、durable request ledger、dispatch race／grant revocation、exact turn interrupt、pause／resume 與 shutdown admission 仍由其他 slice 驗證；本文件不宣稱 fullUnifiedRuntime 完成。
