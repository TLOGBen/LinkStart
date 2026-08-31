# CONTRACT — Codex WebSocket Origin 與 capability admission

## 目標
LinkStart 可以正式 Skill 內的固定 transport 連上明確指定的 loopback Codex app-server；Codex 升版不因版本字串單獨造成斷線，但任一必要 capability 失敗仍 fail closed。

## 前提（Premises）
- `已驗`：Codex `0.151.0` + Runtime `0.1.5` 在 loopback WebSocket 完成真 `received → armed → same-thread delivered → same-App feedback → closed`。
- `已驗`：既有 JSONL adapter 可由 `LINKSTART_CODEX_COMMAND_JSON` 接入 WebSocket bridge，但發布 Skill 無公開 endpoint 入口。
- `已驗`：官方 Codex app-server 提供 `initialize`、`thread/resume`、`thread/read`、`turn/start`、`turn/steer`，WebSocket 仍屬 experimental。
- `已驗`：Feedback command 與 Host supervisor 必須共用 `context.json.codex-adapter.sqlite3`。

## 可斷言條文
- [ ] A1：明確的 loopback WebSocket endpoint 必須可由正式 `adapter start` 接入；不得在執行時產生、複製或改寫 bridge executable。
- [ ] A2：endpoint 僅接受精確 `ws://127.0.0.1:{port}`；任何其他 scheme、host、userinfo、path、query 或 fragment 必須拒絕。
- [ ] A3：bridge 必須只依賴 Python 標準庫，正確處理 text、fragmentation、ping/pong、close、stdin EOF 與非法 frame；不得依賴環境已安裝的 `websockets`。
- [ ] A4：Codex version/userAgent 僅作診斷資訊，不得參與 admission；必須以真 `initialize → thread/resume → thread/read` 完成 capability admission。
- [ ] A5：handshake 失敗、thread identity 不符或 required method 拒絕必須 fail closed，且不得記錄 `armed`。
- [ ] A6：既有 managed control-socket proxy 路徑與 foreground `arm/respond` 輸出不得退化。
- [ ] A7：App Event 只能投遞至綁定的同一 `threadId`；idle 用 `turn/start`，active 用 `turn/steer(expectedTurnId)`，不得 fork、cold resume 或改投。
- [ ] A8：Feedback 必須由 Agent 排入預設 private ledger、Host flush 回原 `appInstanceId`；capability、credential 與 App secret 不得出現於 argv、stdout、receipt 或 log。
- [ ] A9：`armed`、schema probe、unit test 均只是 MOP；只有真 App Event 在原 turn 後進入同 thread，且 Feedback 回同 App 才可記為 MOE。
- [ ] A10：不得存在 Codex version allowlist、denylist、minimum version 或 `>=` 判斷；相容性只由當次 capability probe 與真 MOE 證據決定。

## 錯不起表面（Surface Inventory）
| 表面 | 格式 | 影響（資產 → 後果｜類別） | 釘死測試 |
|------|------|------|------|
| adapter start success | `{"adapter":"codex-app-server","leaseStatus":"armed","monitorMode":"host-lease","ok":true,"operation":"adapter","threadId":"{id}"}` | Origin identity → 錯線程被嗚醒｜邏輯核心 | `adapter_start_receipt_is_redacted_and_exact` |
| invalid endpoint | `{"error":"codex_app_server_url_invalid","ok":false}` | loopback trust boundary → 請求外送或密鑰暴露｜上下游契約 | `websocket_url_gate_is_exact` |
| incompatible Origin | `{"error":"codex_origin_mode_unsupported","ok":false}` | Origin availability → 虛假 armed 造成事件丟失｜穩定性可靠性 | `capability_admission_fails_closed` |
| explicit close | `credentialDeleted:true, connectionRevoked:false, leaseStatus:"closed"` | local credential → 關閉後仍可被使用｜上下游契約 | `close_reports_exact_lifecycle_truth` |
| MOP/MOE receipt | MOP 不得輸出 `LINKSTART_ACTIVE`；MOE 必須同時帶 `threadIdSame:true,sameAppInstance:true,deliveryStatus:"delivered"` | PM decision channel → 使用者誤信已連線｜UI/UX | `live_moe_same_thread_same_app` |

## Verbatim Constants
```text
RUNTIME_VERSION=0.1.5
PROTOCOL_MAJOR=v1
CODEX_APP_SERVER_URL=ws://127.0.0.1:{port}
ERROR_INVALID_URL=codex_app_server_url_invalid
ERROR_UNSUPPORTED=codex_origin_mode_unsupported
EVENT_MARKER=linkstart:event:{eventId}
```
