# App Manifest 與 Browser Client 契約

Type: prototype
Status: resolved
Blocked by: 04, 05

## Question

App Manifest、vanilla browser client 與 `register-app` 的最小可感知 contract 應長什麼樣，才能同時接 self-contained HTML 與既有 localhost web app，讓使用者看見連線狀態、Callsign、Event Receipt／Delivery Ack、可 replay 的 Agent Feedback，以及 agent 主動發起的 structured question／elicitation，而不暴露 routing credentials或把回答誤當 tool approval？

## Assets

- [Prototype：App Manifest 與 Browser Client](../prototypes/06-app-contract.html)

## Answer

- App Manifest v1 欄位固定為 `protocolMajor`、`appId`、`displayName`、`appVersion`、`entry`、`originPolicy`、`requestedCapabilities`、`structuredInputs`；Manifest 不含 bearer、connectionId、tool approval 或 permission relay。
- `entry.kind` 支援 `self_contained_html` 與 `localhost_app`；前者以一次性 fragment launch grant 換 App Instance capability，後者綁 exact scheme/host/port Origin。
- Browser client v1 採 vanilla JavaScript，公開 connect/register/send/cancel/reconnect 與 state/feedback/question subscriptions；所有 JSON/SSE 使用 `fetch + Authorization`，capability 只留記憶體。
- UI 必須分別呈現 App Instance／Origin Connection、Callsign、Event Receipt、Delivery Ack、failed/cancelled、Feedback replay 與 `origin_offline`；Callsign 不顯示 routing credential。
- Structured question 是 Agent Feedback；使用者回答會建立新的 App Event。即使回答文字為「同意」，也不能映射成 tool approval 或 permission verdict。
- v1 只交付 vanilla client；React/Vue/Svelte bindings 不進 v1。

Prototype 已由真 browser 驅動 happy/error/reconnect scenarios，狀態在操作中改變且無 console/page errors。
