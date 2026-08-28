# Claude Origin Session Adapter 研究

查核基準：2026-08-27 10:21（Asia/Taipei）  
研究問題：LinkStart Integration Plugin 應採用哪個 Claude Code 機制，才能在 Origin Session 的 turn 結束、process 仍在線時接收 App Event，維持 `Callsign`／`connectionId` 關係，並將 agent feedback 回到同一 App；Daemon restart 後可恢復，且不自動另開 session？

## 結論

**Canonical semantic adapter 應是 Claude Code 的 two-way Channel：`notifications/claude/channel` 作為 inbound seam，Channel MCP server 的 `reply` tool 作為 outbound seam。** 這是唯一由 Claude Code 官方明確定義成「外部系統把事件直接注入 running session、Claude 再透過工具回覆外部系統」的介面。官方 fakechat 甚至已直接示範 `localhost browser → running Claude Code session → reply tool → browser` 的完整路徑。[Channels](https://code.claude.com/docs/en/channels) [Channels reference](https://code.claude.com/docs/en/channels-reference) [fakechat source](https://github.com/anthropics/claude-plugins-official/blob/main/external_plugins/fakechat/server.ts)

但 **Channels 目前仍是 research preview，不能在已開啟且未 opt in 的 Claude Code session 內動態 attach**。官方流程要求重新啟動 session 並傳入 `--channels plugin:...`；非 Anthropic allowlist 的自訂 channel 在 preview 期間還要用 `--dangerously-load-development-channels`。因此 LinkStart v1 若要求「安裝 plugin 後，對任意已開啟 Origin Session 立即連線」，不能只靠 Channel。[Channels quickstart](https://code.claude.com/docs/en/channels#quickstart) [Channels preview constraints](https://code.claude.com/docs/en/channels-reference#test-during-the-research-preview)

建議採明確的雙層策略：

1. **Canonical：Channel Adapter**，適用於 session 啟動時已 opt in LinkStart channel 的情況。
2. **Current-session compatibility Adapter：plugin Monitor + 同一 plugin 的普通 MCP reply tool**，適用於 plugin 已載入、但 session 未以 Channel flag 啟動的情況。Monitor 只負責 inbound wake；reply、ack 與 connection routing 由 MCP tool／LinkStart protocol 負責。
3. **File-mailbox／background completion 只保留為末級 fallback 或 diagnostic prototype**，不再作正式 Claude Adapter 主線。

Agent SDK、Remote Control 與 session resume 都不應成為 Origin Session Adapter；它們解決的是不同 ownership 或 recovery 問題。

## LinkStart 所需契約

Claude Adapter 的公開契約應固定為下列狀態，而不是綁死 fakechat 的 UI 或實作：

- `attach(originSession, connectionId)`：把既有 LinkStart Agent Connection 綁到**目前這個 Claude Code process/session**。
- `deliver(eventId, connectionId, type, payload)`：將一個 App Event 注入該 Origin Session；`Delivery Ack` 只表示 Adapter 接受，不表示 Claude 已處理。
- `reply(eventId, connectionId, type, payload)`：Claude 透過 tool 明確回傳；Daemon 再路由到同一 App Instance。
- `detach(reason)`：session 結束、channel/monitor 失效或 process 離線時，Daemon 將 connection 標成 `origin_offline`，不排隊、不自動 resume、不另開 session。
- `rebind(connectionId, originSessionId)`：Daemon restart 或使用者明確 resume 同一 conversation 後，由仍在線／重新啟動的 Adapter 主動重新註冊；必須校驗同一 session ID 與既有 connection capability，不能只用可讀的 Callsign。

`Callsign` 只作顯示；routing 一律使用 opaque `connectionId`。Channel 的 `meta` 是合適的傳遞欄位：官方允許把 routing context 放進 `Record<string,string>`，轉成 `<channel ...>` attributes；例如 `connection_id`、`event_id`、`app_id`。Claude Code 不會對 channel notification 回 ack，因此 LinkStart 必須保留自己的 event state，並以 reply/ack tool 回報接受或完成狀態。[Notification format and delivery](https://code.claude.com/docs/en/channels-reference#notification-format)

## 狀態分解

### 1. Current-session attach

**Channel：有條件，不可動態補掛。** Plugin 可以在 manifest 的 `channels` 欄位把 channel 綁到 bundled MCP server；但 session 必須在啟動時 opt in。fakechat 官方流程明確要求退出後以 `claude --channels plugin:fakechat@claude-plugins-official` 重啟。若 session 未把該 MCP server 註冊為 channel，server 即使呼叫 notification，Claude Code 也可能靜默丟棄，server 收不到錯誤。[Plugin channel declaration](https://code.claude.com/docs/en/plugins-reference#channels) [Silent drop behavior](https://code.claude.com/docs/en/channels-reference#notification-format)

**Monitor：可以在目前 session 內掛上，前提是 plugin 已載入且 Monitor 可用。** Plugin monitor 支援 `when: "on-skill-invoke:<skill-name>"`，第一次呼叫 `open-connect` 或 `link-in` 時才啟動 persistent command；每一行 stdout 都會成為 Claude notification。這正好補 Channel 無法 mid-session opt in 的缺口。[Plugin monitors](https://code.claude.com/docs/en/plugins-reference#monitors)

**Background completion：可以臨時掛上，但不是穩定 seam。** 主 session 可啟動 background command；該 command 完成後的 task notification 能讓同一 session 繼續。既有 localhost prototype 已證明此路徑，但它是一次性 completion、每次必須 re-arm，且 inbound、reply、cursor 與 lifecycle 都不是 Claude Code 正式 channel contract。官方只把 background command 定義成長任務機制，不提供 channel 的 routing metadata、ordered event queue 或 reply tool contract。[Background commands](https://code.claude.com/docs/en/tools-reference#background-commands)

### 2. Origin Session active／busy

**Channel 有明確語意。** Events 會 queue 進同一 session 並按順序處理；Claude 忙碌時收到的多個 notifications 會在下一 turn 一起交付。這與 LinkStart「App Instance 內有序、`eventId` 去重」相容，但 Channel transport 寫入成功不是 Claude 已處理，不能拿來當 completion ack。[Channel delivery behavior](https://code.claude.com/docs/en/channels-reference#notification-format)

**Monitor 的官方保證較弱。** 官方描述是 output line 抵達後 Claude 可在同一 session interject/react，但沒有 Channel 文件中的 structured `meta`、ordered grouping 與 delivery caveat。Monitor Adapter 因此應把 stdout line 視為 wake signal，真正 payload 由 Claude 再透過 MCP tool 依 `eventId` 拉取，避免把 protocol 完整性綁在文字通知格式上。[Monitor tool](https://code.claude.com/docs/en/tools-reference#monitor-tool)

### 3. Origin Session idle（turn 已結束、process 還在）

**Channel 是首選。** 官方定義即是 MCP server push event into a running Claude Code session，使 Claude 對 terminal 之外發生的事件作出反應；fakechat 證明 browser message 會進入該 session，Claude 呼叫 `reply` 後 browser 收到結果。[Channels overview](https://code.claude.com/docs/en/channels-reference#overview) [fakechat README](https://github.com/anthropics/claude-plugins-official/blob/main/external_plugins/fakechat/README.md)

**Monitor 是可接受 fallback。** Monitor command 在 session lifetime 內持續存在，output line 到達時 Claude 會反應；plugin monitor 不需要模型每輪手動 re-arm。[Monitor tool](https://code.claude.com/docs/en/tools-reference#monitor-tool) [Plugin monitor lifetime](https://code.claude.com/docs/en/plugins-reference#monitors)

**Background completion 只作 fallback。** 它可以喚醒 idle session，但一次完成即退出，re-arm 依賴模型後續確實再啟動 watcher；任何漏掉 re-arm 的 turn 都讓連線表面存在、實際失聯。這不應成為 formal release 的 canonical lifecycle。

### 4. LinkStart Daemon restart（Claude process 仍在線）

Daemon restart 不應導致 Claude session resume 或重建。Channel/Monitor Adapter process 仍屬 Claude session；它偵測到 Daemon 連線中斷後，只做 bounded reconnect，Daemon 回來後用既有 `connectionId`、`originSessionId` 與 capability 重新註冊。

這部分不是 Channels 自帶的 durability：官方 channel只保證 Claude Code 與 channel subprocess 之間的 stdio/MCP seam。LinkStart 必須自行定義：

- reconnect handshake 不建立新 connection；
- Daemon 重啟前後同一 `connectionId` 的 epoch／generation；
- 斷線期間不接受 App Event，App 收到 `origin_offline` 或 retryable daemon-unavailable，而不是 durable queue；
- reconnect 後從新的 App Event 繼續，不重播未 ack 的舊事件，除非 protocol ticket 另行決定。

Channel `meta.connection_id` 與 reply tool 的 `connection_id` 只負責 correlation；持久 mapping 應放 LinkStart Runtime state，不放 plugin cache path。Claude Code 的 `${CLAUDE_PLUGIN_DATA}` 可跨 plugin update 保存 Adapter 自有設定，但它不應取代 Daemon canonical routing state。[Plugin persistent data](https://code.claude.com/docs/en/plugins-reference#persistent-data-directory)

### 5. Origin Claude process offline

所有三種 in-process Adapter 都無法在 Claude Code process 結束後維持 origin：

- Channel server 是 Claude Code spawn 的 stdio MCP subprocess；session 不在線時 notification 無接收者。
- Plugin monitor 的生命週期到 session 結束為止。
- Background task 也屬該 session/process 的 task lifecycle。

因此必須遵守 map 已定義的 v1 規則：Daemon 將 connection 標成 `origin_offline`，不排隊、不建立 Agent SDK process、不呼叫 `claude -p`／`claude --resume` 代打。Channels 文件也明示 notification 沒有 end-to-end acknowledgement，且 session 未載入 channel 時可靜默丟棄；Daemon 不能用「POST 成功」推定 origin 在線。[Channel delivery caveat](https://code.claude.com/docs/en/channels-reference#notification-format)

### 6. Reply path

Channel 的 reply path 應直接沿用官方 two-way pattern：

1. Adapter 將 `connection_id`、`event_id` 放在 inbound channel `meta`。
2. Channel instructions 要求 Claude 以 `reply`（或 LinkStart 命名的 `send_feedback`）tool 回覆，傳回相同 identifiers。
3. MCP tool 將 feedback 交給 LinkStart Daemon；Daemon 路由到綁定該 `connectionId` 的 App Instance。
4. Tool result 只代表 Daemon 接受；App delivery/completion 由 protocol 狀態另行表達。

官方 reference 明確說 two-way channel 的 reply 就是一個標準 MCP tool；fakechat source 也把 reply 經 WebSocket broadcast 回 localhost UI。LinkStart 不需再造 file outbox 作主路徑。[Expose a reply tool](https://code.claude.com/docs/en/channels-reference#expose-a-reply-tool) [fakechat reply implementation](https://github.com/anthropics/claude-plugins-official/blob/main/external_plugins/fakechat/server.ts#L54-L124)

Monitor fallback 的 outbound 也應呼叫同一 MCP `send_feedback` tool，讓 Channel 與 Monitor 共用完全相同的 LinkStart reply protocol；差異只留在 inbound wake Adapter。

## 候選比較

| 候選 | attach 已開啟 Origin Session | idle wake | busy 時語意 | process offline | reply path | 判決 |
|---|---|---|---|---|---|---|
| Two-way Channel | 否；必須在 session 啟動時 opt in | 是，官方直接支援 | ordered queue；busy events 下一 turn 成組交付 | 不可 | 標準 MCP reply tool | **Canonical semantic adapter** |
| Plugin Monitor + MCP tool | 是；plugin 已載入且 Monitor 可用時 | 是 | 通知可 interject；protocol payload 應另行拉取 | 不可 | 另用同 plugin MCP tool | **Current-session compatibility Adapter** |
| Background completion / file mailbox | 是 | 是，但一次性 | completion notification；缺正式 event contract | 不可 | 另寫檔或 tool | **末級 fallback／diagnostic** |
| Agent SDK | 否；建立 SDK 所有的 subprocess/client | 只對 SDK 自己的 session | 由 SDK loop 管理，不是 interactive Origin process | 可從 disk resume，但會啟動另一 process | SDK event stream | **排除** |
| Remote Control | 可在 session 內 `/remote-control`，但只接 claude.ai／mobile | 是 | 該產品 surface 自有 queue/sync | local process 停止即失效 | Anthropic-hosted client path | **排除** |
| `--resume`／`--continue` | 不適用；它是 process 已停止後重開 conversation | 否 | 不適用 | 可在人工操作下以同 session ID 重開 | 新 process 的 interactive UI | **只作人工 recovery** |

## 為何 Agent SDK、Remote Control、resume 不是答案

### Agent SDK

Agent SDK 的 `ClaudeSDKClient` 預設建立 `SubprocessCLITransport`；`resume=session_id` 是把磁碟 transcript 載入**新的 SDK subprocess**，不是 attach 已在跑的 interactive Claude Code process。官方 cookbook 也把它描述成「同一 client 的 in-memory multi-turn」或「新 process 從 disk-backed session resume」。因此讓 Daemon 在 event 到達時啟動 SDK client，會改變 Origin ownership，並違反 v1 的 process offline 不自動建立替代 agent 原則。[Official Python SDK source](https://github.com/anthropics/claude-agent-sdk-python/blob/main/src/claude_agent_sdk/client.py#L1513-L1637) [SDK session model](https://platform.claude.com/cookbook/claude-agent-sdk-04-migrating-from-openai-agents-sdk)

### Remote Control

Remote Control 能把 claude.ai/code 或 Claude mobile client 接到正在本機執行的 session，並保持雙向同步；但它沒有提供 LinkStart local App 可實作的 channel/MCP endpoint。傳輸經 Anthropic API，且 local process 停止後 Remote Control 也停止。它是 end-user UI product，不是 Integration Plugin 的 programmable Adapter。[Remote Control architecture and limits](https://code.claude.com/docs/en/remote-control)

### Session resume

`claude --resume` 會以同一 session ID 重新開啟 conversation 並追加訊息；這證明 conversation identity 可恢復，但不等於 idle wake，也不等於 attach 原 process。它只應在使用者明確恢復 Origin Session 後觸發 Adapter rebind；Daemon 不得自行 resume。恢復時必須重新以 Channel opt-in flags 啟動，或由 Monitor fallback 重新 attach。[Manage sessions](https://code.claude.com/docs/en/sessions) [How Claude Code resumes](https://code.claude.com/docs/en/how-claude-code-works#resume-or-fork-sessions)

## Preview 與發布限制

Channels 的架構契約最正確，但 v1 release 必須明示以下限制：

- Channels 是 research preview，要求 Claude Code v2.1.80+；Team／Enterprise 必須由管理員啟用。
- 自訂 marketplace channel 尚不在 Anthropic allowlist；開發測試需 `--dangerously-load-development-channels plugin:linkstart@common-dev`。正式 plugin install 本身不會自動賦予 message injection 權限。
- Channel 必須在 session 啟動時 opt in；`open-connect` 不能偷偷把目前 session 升級成 Channel session。
- 本機檢查的 Claude Code 是 2.1.247，`claude plugin init --help` 已列出 `channel` scaffold；該版本符合 feature version 下限。但 `claude --help` 未公開列出 channel flags，應以官方 Channels 文件及真實 MOP 驗收為準，不以 help 文案當 capability gate。
- Plugin Monitor 也是 experimental，且只在 interactive CLI session 執行；在 Monitor tool 不可用的 host、設定 `DISABLE_TELEMETRY`／`CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC`，或 Bedrock／Google Cloud Agent Platform／Microsoft Foundry 時不可用。[Monitor availability](https://code.claude.com/docs/en/tools-reference#monitor-tool)

因此 release policy 應是 fail closed：

1. 啟動時偵測 Channel 是否已註冊；若是，使用 Channel。
2. 否則偵測 Monitor 是否可用；若可用且 plugin 已載入，使用 Monitor + MCP reply tool。
3. 兩者皆不可用時，明確回報 `claude_adapter_unavailable`；只有使用者明確選擇 diagnostic fallback 才啟用 background/file mailbox，不可宣稱持續連線。

## 建議的正式 Module 邊界

- **LinkStart Runtime／Daemon**：canonical connection state、`connectionId`、`eventId`、dedupe、App routing、restart epoch。
- **Claude Channel Adapter**：MCP `claude/channel` capability、notification emission、`meta` mapping、`send_feedback` tool。
- **Claude Monitor Adapter**：session-local long-running watcher，只把 Daemon event 轉成 wake notification；payload 與 reply 仍走相同 MCP protocol。
- **Skill layer**：`open-connect`／`link-in` 只做能力偵測、attach/rebind 與清楚的 preview UX，不管理 event queue，也不自行 resume session。
- **Diagnostic fallback**：file mailbox/background completion，與正式 Adapter 隔離，不共享 delivery guarantee 名稱。

Channel 與 Monitor 是兩個真實 Adapter，因此共用一個 LinkStart protocol seam 合理；file mailbox 不應成為第三套 canonical protocol。

## 必要驗收（本 ticket 的直接輸出）

正式決策至少需要以下 MOP／MOE，且每項記錄 Claude `session_id` 與 LinkStart `connectionId`：

1. Channel-enabled session：turn 結束後 App Event 進入同一 `session_id`，Claude 透過 `send_feedback` 回到同一 App。
2. Channel busy case：連續兩個 `eventId` 在 Claude busy 時到達，之後按序處理且不重複。
3. Current-session Monitor fallback：未以 Channel flag 啟動，但 plugin 已載入；呼叫 `open-connect` 後 idle wake 與 reply 成功。
4. Daemon restart：Claude process 不變；Adapter reconnect 後保持同一 `connectionId`，restart 期間不錯誤 ack event。
5. Claude process offline：App 立即得到 `origin_offline`，沒有 SDK subprocess、`claude -p`、自動 `--resume` 或新 session。
6. 人工 resume：使用者明確以原 session ID 重啟後，Adapter rebind；若未帶 Channel opt-in，必須落到 Monitor 或 fail closed，不能假裝 Channel 已恢復。

## 最終判決

**LinkStart 應把 two-way Claude Code Channel 定為 canonical Claude Origin Session Adapter；Monitor + MCP reply tool 是為既有 session attach 所必需的 compatibility Adapter。原 localhost file-mailbox／background completion trick 已證明「可以喚醒」，但在官方 Channels 存在後只應保留為 fallback／diagnostic，不應再承擔正式主線。**

這個判決同時滿足 LinkStart 的核心邊界：process 在線且 idle 時可被喚醒；active 時有明確 queue 語意；reply 能用 `connectionId` 回原 App；Daemon restart 只 rebind、不換 session；process offline 時 fail closed、不排隊、不另開 agent。
