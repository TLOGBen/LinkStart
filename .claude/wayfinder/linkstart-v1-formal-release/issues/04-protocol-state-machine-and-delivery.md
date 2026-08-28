# Protocol 狀態機與傳遞語意

Type: grilling
Status: resolved
Blocked by: 01, 02, 03

## Question

LinkStart protocol v1 應定義哪些 Agent Connection、App Registration、App Instance、App Event、Delivery Ack 與 agent feedback 狀態及合法轉移，才能誠實保證 instance 內有序、`eventId` 去重、idle Origin Session 喚醒與 `origin_offline`，而不誤稱 exactly-once 或 processing-complete？

## Answer

Protocol v1 採以下 platform-neutral state contract；Claude/Codex adapter 只能把平台事件映射進這個 contract，不得新增另一套 delivery 語意。

### Agent Connection

公開狀態為 `online`、`reconnecting`、`offline`、`closed`：

- attach 成功後進入 `online`。
- Daemon／adapter transport 暫時中斷、但 Origin process 仍可能存活時進入 `reconnecting`；同一 `connectionId` rebind 成功後回 `online`。
- Origin process 結束、rebind deadline 到期或 adapter fatal failure 時進入 `offline`。使用者明確 resume 同一 Origin Session 並再次 `link-in` 時，才可用原 capability rebind 回 `online`。
- 明確 detach／revoke 進入 terminal `closed`，不可復活。

### App Registration 與 App Instance

- App Registration 為 `registered → revoked`；`revoked` terminal。
- App Instance 為 `connected ↔ disconnected → closed`。
- App Instance 建立時綁定恰好一個 Agent Connection，生命週期中不可換綁；要換 Origin Session 必須建立新 instance。

### App Event、Receipt 與 Delivery Ack

提交時先驗 protocol major、App capability、schema、Origin／Connection 狀態與 payload limit；驗證失敗不建立 App Event。Origin 已 offline 時直接回 `origin_offline`，不收件、不排隊。

驗證成功後，Daemon 以 durable journal 原子寫入 App Event、配置 instance-local monotonic sequence，回傳 Event Receipt，公開狀態由 `received` 開始：

```text
received ──adapter accepts──> delivered
    ├────terminal error─────> failed
    └────cancel before ack──> cancelled
```

- 每個 App Instance 同時只允許一個 In-flight App Event，其餘 `received` events 依 sequence 等候。
- `delivered` 只表示 Agent adapter 已接受事件；不是模型開始處理、完成處理或 tool approval。
- `failed` terminal reason 至少包含 `origin_offline`、`delivery_timeout`、`adapter_rejected`、`connection_closed`。
- App 只能取消仍為 `received` 的 Event；`delivered` 後不能撤回，只能送新的修正 Event。
- 相同 `eventId` + 相同 canonical payload 回傳既有 Receipt 與目前狀態，不配置新 sequence；相同 ID + 不同 payload 回 `event_id_conflict`。
- Origin 仍 online、但 adapter 暫時不可投遞時，Event 保持 `received` 並等候安全 delivery boundary；超過 Daemon 定義的 deadline 才 `failed(delivery_timeout)`。精確 deadline、retention 與 restart recovery 由「Daemon 發現、生命週期與版本切換」決定。
- Daemon restart 後只恢復 durable `received` events；若同一 Agent Connection 未能 rebind 為 online，這些 Event 轉 `failed(origin_offline)`，不等待新的替代 session。
- Adapter acceptance 本身必須以 `eventId` idempotent；結果未知時先 reconcile，再決定是否 retry。LinkStart 只保證一個 logical Event record 與去重投遞，不宣稱跨模型 context 的 exactly-once。

Event Receipt 可由 HTTP response 返回；Event 狀態變更與 Delivery Ack 經 SSE 推送。SSE transport cursor 只用於 stream resume，不取代 domain `eventId`。

### Agent Feedback

- 每則 Agent Feedback 有 opaque `feedbackId`，並可選擇攜帶 `inReplyToEventId`；因此既能回應 App Event，也能主動回報進度。
- Feedback 先持久寫入 journal，再經 App Instance 的 SSE stream 送出；App reconnect 以 transport cursor replay 尚未觀察的 Feedback。
- Feedback 不把所關聯 App Event 改成 `completed`；Delivery Ack、Agent Feedback 與模型處理完成是三個不同概念。

### Adapter mapping

- Claude Channel／Monitor 只有在 channel notification 或 Monitor adapter 確認接收後才能產生 Delivery Ack。
- Codex active turn 以 `turn/steer(expectedTurnId)` 接受，idle thread 以 `turn/start` 接受；race／non-steerable 必須重新同步或保持 `received`，不得靜默換成新 session。
- Adapter process offline 一律映射為 Agent Connection `offline`；不得以 SDK subprocess、cold resume 或另一條 session 代打。

此 contract 保證 instance-local ordering、eventId idempotency、idle Origin Session delivery 與可觀測失敗；明確不保證 model processing-complete、tool approval、全域 ordering 或 exactly-once。
