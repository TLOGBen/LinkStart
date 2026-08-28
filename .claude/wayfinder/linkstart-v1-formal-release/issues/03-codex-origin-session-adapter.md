# Codex Origin Session Adapter 契約

Type: research
Status: resolved
Blocked by:

## Question

LinkStart Integration Plugin 如何讓產出 App 的 Codex Origin Session 從一開始就可由 app-server 持續控制，並正確使用 active-turn steering、idle-turn start、thread resume 與 streamed events；對已經以 standalone TUI 開啟、未由 app-server 擁有的 session，v1 應宣告什麼限制？

## Assets

- [Research：Codex Origin Session Adapter](../research/03-codex-origin-session-adapter.md)

## Answer

Codex Origin thread 必須從建立或明確 resume 起就由同一個 LinkStart-owned app-server process 持有，Codex TUI 從開始即以 `--remote` 加入。App Event 在 active turn 使用 `turn/steer(expectedTurnId)`，idle 使用 `turn/start`；feedback 與完成狀態由 subscribed item/turn events 回傳。已開啟的 standalone embedded TUI 不支援 hot takeover；必須先結束，再由 LinkStart launcher 明確 resume。Origin app-server process offline 時回 `origin_offline`，不得為投遞而自動 cold resume。

Evidence：[Codex Origin Session Adapter 研究](../research/03-codex-origin-session-adapter.md)。
