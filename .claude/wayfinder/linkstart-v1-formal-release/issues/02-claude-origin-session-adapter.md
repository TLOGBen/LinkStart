# Claude Origin Session Adapter 契約

Type: research
Status: resolved
Blocked by:

## Question

LinkStart Integration Plugin 應以 Claude Code Channels、Monitor/background completion、Agent SDK 或其明確組合作為 canonical adapter，才能在 Origin Session idle 後接收 App Event、保持 Callsign/connectionId 關係、回傳 agent feedback，並在 Daemon restart 後恢復而不另開 session？

## Assets

- [Research：Claude Origin Session Adapter](../research/02-claude-origin-session-adapter.md)

## Answer

Two-way Claude Code Channel 是 canonical semantic adapter，以 `notifications/claude/channel` 接收 App Event、以同 plugin MCP `send_feedback` tool 回覆；對已開啟且未 opt-in Channel 的 Origin Session，採 plugin Monitor + 同一 MCP reply protocol 作 compatibility adapter。File-mailbox/background completion 只保留 diagnostic fallback；Agent SDK、Remote Control 與自動 session resume 不符合 Origin running-session contract。Claude process offline 時 fail closed；Daemon restart 只做同 connection rebind，不另開 session。

Evidence：[Claude Origin Session Adapter 研究](../research/02-claude-origin-session-adapter.md)。
