# Codex Origin Session Adapter 研究結論

## 結論

LinkStart v1 若要持續控制「產出 App 的同一條 Codex Origin Session」，必須讓該 thread 從建立前就位於同一個長駐 app-server process 內。正確形態不是「LinkStart 接管一個已開啟的 TUI」，而是：LinkStart 啟動可供多 client 連線的 app-server、由 LinkStart client 建立或載入 thread 並保存 `thread.id`／`thread.sessionId`，再讓 Codex TUI 以 remote mode 加入該 thread。app-server 持有 live `ThreadManager`／`CodexThread`；TUI 與 LinkStart 都只是同一 thread 的 JSON-RPC driver + event subscriber，並非互斥 owner。

對已經由 standalone TUI 以 embedded in-process app-server 開啟的 thread，v1 必須明說：**不支援 hot attach／hot takeover**。LinkStart 不得另啟 app-server 對同一 rollout 做 cold `thread/resume` 來假裝接管；那無法取得原 process 的 active turn 或 live event stream，還會形成兩個 live runtime。使用者必須關閉原 TUI，並由 LinkStart app-server + remote TUI 明確重新開啟／resume，才能進入受支援路徑。

## 查核基準

- 查核日期：2026-08-27（Asia/Taipei）。
- 官方文件：OpenAI Codex App Server 文件。它將 app-server 定義為 rich client 的深度整合介面，涵蓋 conversation history、approvals 與 streamed agent events；protocol 是雙向 JSON-RPC，且 remote TUI 以 `codex --remote` 連線。[OpenAI Codex App Server](https://developers.openai.com/codex/app-server/)
- 官方原始碼：`openai/codex` tag `rust-v0.149.1` 對應 commit `ff29a44391deccde0aba0f8390337d7f3c319ea4`。
- 本機直接核對：`codex-cli 0.149.1`；`codex --help` 與 `codex resume --help` 都提供 `--remote <ws://|wss://|unix://>`，`codex app-server --help` 提供 `--listen`；同版本 `generate-json-schema` 產物含 `thread/resume`、`turn/steer`、`expectedTurnId`、`thread/status/changed`、`item/agentMessage/delta`、`turn/completed`。

## 1. Ownership 的精確邊界

「app-server 擁有 thread」應理解為 process ownership，不是單一 connection ownership：

1. app-server process 內的 `ThreadManager` 持有 live `CodexThread`；`thread/start` 建立它，`thread/resume` 在同一 process 內先查 live manager，找不到才由 persisted history cold-load。
2. 每個 JSON-RPC connection 有獨立 subscription。`thread/start` 會把發起 connection auto-attach 到新 thread；同一 app-server 的另一 connection 可對相同 `thread.id` 呼叫 `thread/resume`，原子取得 history／active-turn snapshot 並訂閱其後事件。[thread/start auto-attach](https://github.com/openai/codex/blob/ff29a44391deccde0aba0f8390337d7f3c319ea4/codex-rs/app-server/src/request_processors/thread_processor.rs#L1461-L1478) [running-thread resume](https://github.com/openai/codex/blob/ff29a44391deccde0aba0f8390337d7f3c319ea4/codex-rs/app-server/src/request_processors/thread_processor.rs#L3940-L3990) [atomic subscribe contract](https://github.com/openai/codex/blob/ff29a44391deccde0aba0f8390337d7f3c319ea4/codex-rs/app-server/src/thread_state.rs#L55-L58)
3. Thread-scoped notifications送給該 thread 的所有 subscribed connection。因此 LinkStart client 與 remote TUI 可以同時看見同一條 turn/item stream；任一方斷線只移除其 connection subscription，不等於刪除 thread。[subscription state](https://github.com/openai/codex/blob/ff29a44391deccde0aba0f8390337d7f3c319ea4/codex-rs/app-server/src/thread_state.rs#L488-L550)
4. `thread.id` 是要持久保存、用於 `thread/resume`／`turn/*` routing 的識別；`thread.sessionId` 是 live session tree root，fork 後不一定等於 thread id，不能自行推導。官方文件要求 client 直接讀回 `thread.sessionId`。[Start or resume a thread](https://developers.openai.com/codex/app-server/#start-or-resume-a-thread)

因此 LinkStart 的「Agent Connection」應綁定 `{appServerInstance, thread.id, thread.sessionId}`，而不只綁 rollout 路徑或 TUI PID。

## 2. 從一開始建立可持續控制的 Origin Session

受支援的啟動順序如下：

1. LinkStart 啟動同一個長駐 app-server，使用可供多 client 連入的 transport。stdio 是單一父子程序管線，不適合 LinkStart client 與 TUI 兩個獨立 client；應使用 Unix socket，或 loopback WebSocket。
2. LinkStart client 完成 `initialize` → `initialized`，呼叫 `thread/start`，記錄回傳的 `thread.id` 與 `thread.sessionId`，並保持 transport reader 常駐。
3. 以同一 endpoint 啟動 TUI 並 resume 同一 thread，例如：

   ```text
   codex resume --remote unix:///path/to/app-server.sock <thread-id>
   ```

   Windows／無 Unix socket 時可用 `ws://127.0.0.1:<port>`；但官方文件明列 WebSocket transport 為 experimental、unsupported for production，這是 LinkStart v1 跨平台發布必須公開承擔的成熟度限制，不能寫成穩定 production surface。[Remote TUI and transports](https://developers.openai.com/codex/app-server/#connect-the-cli-terminal-ui)
4. TUI 的 `thread/resume` 加入已由同一 app-server 載入的 live thread；LinkStart connection 與 TUI connection 同時訂閱後續事件。

本機 0.149.1 另有一條 convenience path：Unix 上若預設 local app-server daemon 已先存在、且這次 TUI launch 沒有 `-c`、非預設 config loader、`--strict-config` 等不可重播 override，普通 `codex` 會探測 daemon socket 並自動連入；否則退回 embedded app-server。[target selection](https://github.com/openai/codex/blob/ff29a44391deccde0aba0f8390337d7f3c319ea4/codex-rs/tui/src/lib.rs#L850-L875) [reuse guard](https://github.com/openai/codex/blob/ff29a44391deccde0aba0f8390337d7f3c319ea4/codex-rs/tui/src/lib.rs#L909-L920) LinkStart 不應依賴這個隱式條件；正式 launcher 應明確傳 `--remote`。

只靠 Integration Plugin skill 在一個已開始的 standalone TUI turn 內執行，時間點已經太晚；skill 無法把 enclosing TUI 的 embedded `ThreadManager` 搬進另一個 app-server。故「由 LinkStart 入口啟動 Codex」是 adapter contract 的必要前置條件，不是可選 UX。

## 3. App Event 應如何送入同一條 thread

LinkStart 必須以自己已消化的 streamed state 判斷 thread 是 active 還是 idle，並在每個 Agent Connection 內序列化送入：

### Active turn：`turn/steer`

- 呼叫 `turn/steer`，帶 `threadId`、互動 input、目前 stream 記錄的 `expectedTurnId`。
- `expectedTurnId` 是 compare-and-set guard：若 active turn 已換掉，request 必須失敗，而不是把事件塞進錯的 turn。
- 成功只回同一 `turnId`，不會產生新的 `turn/started`。
- 無 active turn、turn id mismatch、review/compact 等不可 steer turn 都應回傳明確失敗；LinkStart 不得在尚有 active turn 時偷偷改呼叫 `turn/start`。[official turn/steer contract](https://developers.openai.com/codex/app-server/#steer-an-active-turn) [0.149.1 enforcement](https://github.com/openai/codex/blob/ff29a44391deccde0aba0f8390337d7f3c319ea4/codex-rs/app-server/src/request_processors/turn_processor.rs#L916-L1044)

### Idle thread：`turn/start`

- thread 已確認 idle 時呼叫 `turn/start`，帶 `threadId` 與 App Event input；回傳的新 turn 初始為 `inProgress`。
- 0.149.1 內部的 `turn/start` 實作目前使用 `start_or_steer_turn`，active 時可能退化成 steer；LinkStart 不應依賴這個 implementation detail。官方 protocol 已把「idle start」與「active steer」分成兩個 method，顯式分流才能保留 `expectedTurnId` 的競態保護。[official lifecycle](https://developers.openai.com/codex/app-server/#lifecycle-overview) [turn/start implementation](https://github.com/openai/codex/blob/ff29a44391deccde0aba0f8390337d7f3c319ea4/codex-rs/app-server/src/request_processors/turn_processor.rs#L478-L568)

判斷依據應來自 subscription stream：`turn/started` 記錄 active `turn.id`；`turn/completed` 的 final status（`completed`／`interrupted`／`failed`）清除 active state；`thread/status/changed` 用來交叉核對 thread runtime status。不要只在送出前做一次 `thread/read`，因為 `thread/read` 不 resume、也不 subscribe，查完即可能過時。[Events and turn lifecycle](https://developers.openai.com/codex/app-server/#events)

## 4. Thread resume 的兩種語意

`thread/resume` 必須區分：

- **同一 app-server process 內的 running resume**：第二個 connection 加入 live `CodexThread`。0.149.1 會把 persisted history 與 active-turn snapshot 合併後回覆，先把 connection 加入 subscription，再讓後續 event 通過，避免 snapshot／stream 之間漏事件。這是 LinkStart reconnect 或 remote TUI join 的正確路徑。[running resume response ordering](https://github.com/openai/codex/blob/ff29a44391deccde0aba0f8390337d7f3c319ea4/codex-rs/app-server/src/request_processors/thread_lifecycle.rs#L566-L790)
- **另一個／重啟後 app-server 的 cold resume**：由 persisted rollout 重建新的 live runtime。它保留 conversation history 與 thread identity，但不是對舊 process active turn 的接管。

依 map 既定限制「Origin agent process 離線時回 `origin_offline`，不排隊、不建立替代 session」，LinkStart v1 的自動 delivery path 只能使用前者。app-server process 掉線時不可為了投遞 App Event 自動 cold resume；冷 resume 應留給使用者明確重新開啟 Origin Session 的流程。

## 5. Streamed events 的最小正確消費方式

在 `thread/start` 或 `thread/resume` 後，LinkStart 必須持續讀同一 transport；官方要求以通知 stream 驅動 thread、turn 與 item lifecycle。[Protocol lifecycle](https://developers.openai.com/codex/app-server/#lifecycle-overview)

- `turn/started`：建立 active-turn state。
- `item/agentMessage/delta`：只供 App 即時增量顯示，按序 append。
- `item/started`／`item/completed`：`item/completed` 是該 item 的 authoritative final state。
- `turn/completed`：以 final turn status 關閉本次 delivery；failed turn 仍需傳回失敗狀態，不可只等文字。
- `thread/status/changed`：同步 `idle`／`active`／`systemError` 等 runtime 狀態。

LinkStart 不可把單一 delta、request response，或僅 `item/agentMessage/delta` 停止出現視為完成；完成邊界是 `turn/completed`。重連時先 `thread/resume` 取得 snapshot，再繼續消費 stream。

## 6. v1 必須公開的 standalone TUI 限制

Standalone TUI 在未找到可重用 daemon、也未指定 `--remote` 時，會建立 `InProcessAppServerClient`；remote／daemon path 則建立 `RemoteAppServerClient`。兩者是啟動時選定的不同 target，沒有「把已運行 embedded target 改掛到外部 app-server」的 protocol。[TUI app-server targets](https://github.com/openai/codex/blob/ff29a44391deccde0aba0f8390337d7f3c319ea4/codex-rs/tui/src/lib.rs#L246-L278) [embedded vs remote startup](https://github.com/openai/codex/blob/ff29a44391deccde0aba0f8390337d7f3c319ea4/codex-rs/tui/src/lib.rs#L419-L498)

因此 v1 宣告應是：

> LinkStart Codex Origin Adapter 只支援由 LinkStart app-server 建立／載入，且 TUI 自開始即以 `--remote`（或符合條件的既有 local daemon）連入的 thread。對已在 standalone embedded TUI 中開啟的 thread，不支援不中斷的 attach、active-turn steer 或 live-event replay。請先結束該 TUI，再透過 LinkStart launcher 明確 resume；LinkStart 不會在原 TUI 仍運行時另啟 app-server cold-resume 同一 thread。

這個限制仍容許使用者明確關閉後 resume 同一 persisted `thread.id`；不容許的是把「history 可讀」誤稱為「原 live Origin Session 已被接管」。

## Decision input

Ticket 的研究答案已足夠收斂為以下 adapter contract：

- 主線：一個 LinkStart-owned、可多 client 連線的 app-server process；LinkStart client 先 `thread/start` 並保存 identity，Codex TUI 再以 remote `thread/resume` 加入。
- Delivery：active → `turn/steer(expectedTurnId)`；idle → `turn/start`；non-steerable／race → 明確失敗並重新同步，不靜默換語意。
- Feedback：以 subscribed `turn/*` + `item/*` stream 回 App，`item/completed` 為 item 真值，`turn/completed` 為 delivery 終點。
- Reconnect：只對同一 live app-server 做 running `thread/resume`；origin process offline 時遵守 map，回 `origin_offline`。
- 限制：standalone embedded TUI 不可 hot takeover；須由 LinkStart launcher 重開／resume。
