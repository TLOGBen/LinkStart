# 三個公開 Skill 的責任與 UX

Type: grilling
Status: resolved
Blocked by: 02, 03, 06, 07, 12

## Question

`open-connect`、`link-in`、`register-app` 三個 public skills 的前置條件、輸入、可觀測輸出、失敗語意與互相協調方式應如何切分，才能讓一般入口簡單、進階操作可組合，且 Claude/Codex 只在 adapter 層分歧？

## Answer

- `open-connect` 是一般使用者的 composite entry。輸入 App Manifest；選取 `open-connect/assets/bin/<target>/` 的 exact-version Runtime；完成 discovery + authenticated health/version handshake。同版重用、缺席則 on-demand start、版本衝突 fail closed，只有明示 `explicit_drain_restart` 才切換。之後依序呼叫 `link-in` 與 `register-app`，回傳不含 bearer 的 `OpenConnectReceipt`。
- `link-in` 只建立／rebind 目前 Origin Session 的 Agent Connection，輸出 opaque `connectionId`、display-only Callsign、adapter、`previewGrade` 與 connection status。Claude：Channel → 通過 self-test 的 Monitor → `claude_adapter_unavailable`；Codex：只接受 allowlisted LinkStart-owned app-server remote thread，standalone embedded TUI 回 `codex_origin_mode_unsupported`。它不啟動 Daemon、不註冊 App、不管理 Event queue。
- `register-app` 只接受固定 App Manifest v1 與 online `connectionId`，驗證後建立 durable App Registration；`launch=open` 時核發一次性 grant並交 OS browser，只回 redacted receipt。它不 attach Origin、不啟動 Daemon，也不把 App 回答視為 tool approval。
- 固定 orchestration 為 `open-connect → resolve/start/reuse Daemon → link-in → register-app → composite receipt`；任一步失敗即停止後續步驟。Durable registration 不假裝 rollback；browser launch failure 回 registration receipt + `browser_launch_failed`。
- Skills 只擁有使用者 workflow；Receipt、Delivery Ack、Feedback、ordering/dedupe 與 delivery errors 全屬 LinkStart Runtime protocol，不得在 skill 層定義第二套。
- 唯一 binary owner 為 `plugins/linkstart/skills/open-connect/assets/bin/<target>/linkstart[.exe]`；另外兩個 skills 共用同 launcher，不複製 binary、不首次下載、不要求 system install。

公開 receipts：`OpenConnectReceipt`、`AgentConnectionReceipt`、`AppRegistrationReceipt`；所有成功輸出帶 `previewGrade`，所有 secrets 只以 redacted/存在性呈現。

Skills 只解析 Rust CLI 的 `help --json` 與各 command `--json` contract，不依賴人類表格 prose；help/status/ps 不得輸出 bearer、launch grant 或完整 capability。
