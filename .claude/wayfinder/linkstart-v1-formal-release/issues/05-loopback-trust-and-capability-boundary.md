# Loopback Trust 與 Capability 邊界

Type: grilling
Status: resolved
Blocked by: 04

## Question

在單機 loopback 範圍內，v1 最小必須如何建立 App Instance capability、Origin/Host/sender gate、connectionId 保護與 replay 防線，並明文禁止 App Event 被當成 tool approval、permission grant 或 scope expansion？

## Answer

- 每個 App Instance 使用 CSPRNG 產生、不可推導且與該 instance/Agent Connection 綁定的 bearer capability；`connectionId` 只作 locator，不是 authentication secret。
- 所有 JSON requests 與 SSE 都由 browser client 使用 `fetch` + `Authorization`；v1 不把長效 secret 放 query string、cookie，也不使用無法帶自訂 header 的 native `EventSource`。
- Daemon 同時驗 exact `Host`、已註冊的 exact `Origin`、method、content type、body limit 與 capability；CORS/LNA 只是 browser policy，不能代替 sender capability。
- Self-contained HTML 由 Daemon 提供 loopback launch URL；若支援 `file://`／`Origin: null`，只能透過一次性、短效 launch grant，且必須列入真 browser MOP，不能把 `null` origin 當身份。
- SSE reconnect 每次重新驗 capability；transport cursor 只能在同 App Instance/restart epoch 內 resume。Domain `eventId` 仍依既定同-ID同-payload idempotency，異 payload 回 conflict。
- App Event 永遠是不可信 user/application input，不能代表 tool approval、permission grant、scope expansion 或 sandbox escalation。v1 不宣告 Claude permission relay，也不替 Codex 回 approval request。
- State directory 在 Unix 建立為 `0700`、檔案 `0600`；Windows 使用 current-user-only DACL。不得持久記錄 bearer、credentials 或 app secrets 超過必要生命週期。
- Capability revoke、App Instance close 或 Agent Connection close 後，所有後續 request/reconnect fail closed。

此 contract 防同瀏覽器其他 origin、重放與 locator 猜測；同一 OS 使用者下的惡意 process 明列為 v1 threat-model 外，不冒充已隔離。
