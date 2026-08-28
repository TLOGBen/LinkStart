# CONTRACT — Codex 掛機喚醒 Origin Adapter

## 目標
在 Agent turn 已結束且 App 仍連線時，App Event 仍能喚醒產出 App 的同一個 Codex thread；監控生命週期不再依附模型回合，並保留既有 foreground 相容路徑。

## 前提（Premises）
- `已驗`：Codex App Server `0.150.1` 提供 `thread/resume`、`thread/status/changed`、`turn/start`、`turn/steer(expectedTurnId)`；來源為本機 CLI schema 與官方文件。
- `已驗`：目前 repo 只允許 Codex `0.149.1`，且沒有 app-server client；來源為 `references/codex.md` 與 repo 搜尋。
- `已驗`：Runtime `0.1.5` 的 `arm` 是最長 3,600 秒的 bounded wait，timeout 只回 `rearmRequired:true`；來源為 `runtime.py`。
- `已驗`：現有 `respond` 綁定 Ack、Feedback、下一次 wait；Runtime Ack／Feedback 重試尚未完整冪等；來源為 `runtime.py`、`src/store.rs`。

## 可斷言條文
- [ ] A1：沒有 active turn 時，App Event 必須由 adapter 以 `turn/start` 投遞到綁定的同一 `threadId`；不得建立、fork 或改投其他 thread。
- [ ] A2：有 active turn 時，App Event 必須以 `turn/steer` 與當下 `expectedTurnId` 投遞；active/idle race 必須重新判定，且同一 `eventId` 最多產生一份 model-visible input。
- [ ] A3：只有 app-server 明確接受投遞後才能記錄 Delivery Ack；拒絕、離線或不確定結果必須保留可對帳、可重試狀態。
- [ ] A4：adapter 必須跨 bounded-wait timeout 自動 re-arm；只有明確 close、Origin offline 或不可恢復 schema/version mismatch 才停止 lease。
- [ ] A5：adapter 重啟或投遞回應遺失後，必須以固定 event marker 對帳；不得因 crash window 重複喚醒或漏 Ack。
- [ ] A6：Feedback 必須可在不接管下一次 wait 的情況下獨立送回原 App Instance；既有 `arm`／`respond` foreground 行為與輸出 schema 不得退化。
- [ ] A7：Runtime Ack 與 Feedback 對相同 identity、相同 payload 的重試必須回成功等價結果；相同 identity、不同 payload 必須 fail closed。
- [ ] A8：connection capability、App capability、控制端 credential 不得出現在 argv、stdout、receipt、log 或 model-visible input；App payload 一律標記為不可信且不得授予 approval、permission 或 scope。
- [ ] A9：不具可控制 app-server transport 的 Codex host 必須回 `codex_origin_mode_unsupported`，且不得宣稱支援掛機；`close` 不得宣稱 Runtime-side revoke。
- [ ] A10：MOE 必須實測 Agent turn 完成後由 App 送 Event、同一 `threadId` 被喚醒、App 收到 Delivery Ack 與 Agent Feedback；build、mock、另一 thread 或 foreground wait 均不得代替。

## 錯不起表面（Surface Inventory）
| 表面 | 格式 | 釘死測試 |
|------|------|----------|
| adapter start receipt | JSON 必含 `adapter:"codex-app-server"`、`monitorMode:"host-lease"`、`threadId`、`leaseStatus:"armed"`，不得含 credential | `adapter_start_receipt_is_redacted_and_exact` |
| timeout/re-arm | 正常 re-arm 不輸出 user-facing error；狀態查詢只回 `leaseStatus:"armed"` | `timeout_rearms_without_false_failure` |
| unsupported Origin | `{"ok":false,"error":"codex_origin_mode_unsupported"}` | `unsupported_origin_fails_closed` |
| explicit close | JSON 必含 `credentialDeleted:true`、`connectionRevoked:false`、`leaseStatus:"closed"` | `close_reports_exact_lifecycle_truth` |
| foreground compatibility | 既有 `arm`／`respond` JSON keys 與錯誤碼維持不變 | `foreground_contract_remains_compatible` |

## Verbatim Constants
```text
RUNTIME_VERSION=0.1.5
PROTOCOL_MAJOR=v1
CODEX_ALLOWLIST=0.150.1
ADAPTER=codex-app-server
MONITOR_MODE=host-lease
EVENT_MARKER=linkstart:event:{eventId}
ERROR_UNSUPPORTED=codex_origin_mode_unsupported
METHOD_IDLE=turn/start
METHOD_ACTIVE=turn/steer
```
