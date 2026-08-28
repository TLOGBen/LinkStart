# MOP／MOE 跨平台驗收矩陣

Type: grilling
Status: resolved
Blocked by: 02, 03, 06, 07, 08, 09, 10, 12

## Question

正式發布前必須有哪些 Windows、Linux、macOS × Claude Code、Codex 的真實驗收案例，才能分別證明 MOP 與 MOE：App Event 進入同一 Origin Session、idle 後仍可互動、Daemon restart 可恢復、feedback 回到同一 App、不同版本 fail closed，且全程沒有另開未綁定 session？

## Answer

正式 tag 前建立 Windows、Linux、macOS × Claude Code、Codex 六格 evidence matrix；每格分開記 MOP 與 MOE，任何 MOE 空白都不得以 build、mock、curl 或另一平台結果代填。

### 每格 MOP

- exact target binary SHA/version/protocol、native executable smoke、authenticated health、private state-dir permissions。
- self-contained HTML 與 localhost App 的真 browser registration、launch grant redemption、JSON Receipt、fetch-SSE connect/reconnect、Feedback replay、CORS/LNA/error diagnostics。
- Claude/Codex capability detection、minimum/allowlisted version、adapter label、schema/help handshake；unknown version与 unsupported mode fail closed。
- common-dev Claude source／Codex generated assets parity、temporary plugin install/list/add、skill-local binary direct execution。

### 每格 MOE

每次測試記錄不可變的 `sessionId/thread.id`、`connectionId`、`appInstanceId`、`eventId`、`feedbackId` 與 browser DOM evidence：

1. Agent 產 App，turn 結束；App 再送 Event，idle Origin Session 被喚醒且 identity 不變。
2. Origin busy 時連送兩個 Event；instance sequence 有序、同 `eventId` 不重複、adapter 接受才 Delivery Ack。
3. Agent Feedback 回同一 App；browser 斷線期間 feedback 先 journal，重連後以 cursor replay，DOM 恰好顯示一次。
4. Daemon crash/restart；WAL recovery 後同 connectionId rebind，received Event/Feedback不遺失、不另開 session。
5. Origin process offline；App 得 `origin_offline`，沒有 `claude -p`、SDK代打、Codex cold resume或替代 session。
6. Runtime/protocol/adapter version mismatch；清楚 fail closed，無自動 restart或不受支援 fallback。
7. App 的「同意」／structured answer 無法批准任何工具或擴權；原生 approval仍是唯一 authority。

### Adapter-specific gates

- Claude：Channel-enabled canonical path、busy queue、`send_feedback`；另驗 current-session Monitor compatibility path。兩者皆保留同 session ID；unsupported provider/policy回 unavailable。
- Codex Unix（Linux/macOS）：LinkStart-owned app-server + Unix remote TUI，active `turn/steer(expectedTurnId)`／idle `turn/start`／running `thread/resume`。
- Codex Windows：loopback WebSocket remote TUI，明標 experimental/production unsupported；standalone embedded TUI hot takeover 必須拒絕。

### Evidence authority

- GitHub-hosted CI 可證明 build、unit/integration、browser MOP與mock adapter contract，不能證明登入後的真 Origin Session MOE。
- 六格真 MOE 必須在受控 native host 上執行並保存 runner/OS/CLI version、raw protocol receipts與browser screenshot/DOM。缺少 macOS/Windows host或登入權限時，terminal 是 `NEW_AUTHORITY_REQUIRED`，不可降級 gate。
- Runtime GitHub Release、common-dev 同步 PR與 main push是 release MOP；只有六格 matrix全 PASS、兩 repo version/README/changelog一致、無 blocking cleanup，才是正式發布 MOE。
