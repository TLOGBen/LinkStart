# LinkStart 進度帳

**用法**：每完成一步就更新這份檔案並推上 GitHub。新的 session 先讀這份，再核對 git、PR 和實際程序；任何「完成」都要附上 SHA、平台、指令和證據。原始證據留在本機（`.claude/prove-it/`、`.codex/`），這裡只記路徑和雜湊。

## 目標（2026-11-19 已確認）

多 Agent 協作平台：

- 任何 CLI 都能由 Runtime 用 PTY 啟動並持有，在同一個 TaskSpace 裡分工、交接、留言。
- 人從管理介面查看與控制；可以接 GitHub issue／PR 持續處理。
- 離開、真實閒置至少 65 分鐘，或 Runtime 重啟之後，工作都能接續。
- 有結構化協定的 CLI（例如 Codex）屬於選配強化，不是加入平台的前提。

計畫：`.claude/think/linkstart-resume.md`（本機）。

## 下一步

依序合併 #12 → #13 → #14 到 main，然後開始做 A2 前導片：兩個不同的真實 CLI 在同一個 TaskSpace 裡，從瀏覽器和 CLI 兩個入口操作，並完成一次交接。做 A2 時一併處理 K1（全域鎖）。

## 驗收項目

| # | 項目 | 狀態 | 證據（SHA／平台／紀錄） |
|---|---|---|---|
| A1 | 任意 TUI CLI 可在 Runtime PTY 中啟動、操作、讀回輸出 | 部分證實：單一 CLI、API 入口 | `f49b44f`／Windows／`prove-it/records/024624b-claude-pty-pilot.md`（sha256 `bf0a877d…`），修正後在新資料夾完成信任詢問加問答 |
| A2 | 兩個以上 CLI 並存於同一 TaskSpace，瀏覽器與 CLI 都能操作，可交接 | 未開始 | |
| A3 | 撤權與角色檢查 | 未開始 | |
| A4 | 人類啟動設定（核准執行檔、參數、工作區） | 契約已審，未實作 | |
| A5 | 重啟恢復；結果未知的操作不報成功 | 局部證據 | |
| A6 | GitHub 持續處理循環 | 未開始 | |
| A7 | 真實閒置至少 65 分鐘後，同一 Session 仍可回應 | 未開始 | |
| A8 | 同一個 SHA 上的最終整合審查 | 未開始 | |

## PR 狀態

| PR | 分支 | head | 狀態 | 驗證 |
|---|---|---|---|---|
| #12 | feature/cli-runtime-shutdown | `a1571fb` | 可合併 | Windows／Linux：`cargo test --locked --no-fail-fast` 全過；public HTTP 各 20/20；fmt 和 CI 的 Python 測試全過。獨立 review 找到 5 項：3 項已修，2 項轉為 K1、K2 |
| #13 | feature/runtime-incarnation | 待 merge `a1571fb` | 可合併（在 #12 之後） | 在 `6ca145b`（含 `245ebdb`）上，Windows／Linux 全套通過；原本的 8 秒門檻問題已由 native ACL 修正解決 |
| #14 | feature/agent-saved-plan-recovery | `abe9294` | 可合併（在 #12 之後） | F1／F2 都先確認測試失敗（RED），修正後通過（GREEN）；Windows／Linux 全套通過 |

## 已知問題（未修，已排入切片）

- **K1**：`/v2/tools` 的全域 std Mutex 在 tokio 工作執行緒上包住了會阻塞的原生終端操作（`terminal.create` 最多可等 5 秒）。只要有一個操作卡住，其他 agent 的所有呼叫和 `/v2/health` 都會跟著卡住。位置：`src/runtime_http.rs:236-264`。排在 A2。
- **K2**：`NativeState.receipts` 會隨著每次寫入或續約無限增長。排在 A7（長時間執行）。

## 紀錄

- 2026-11-19：Claude 接手。前導測試發現 `terminal.write` 會拒絕只含空白的輸入（例如單獨的 Enter），已修正並用真的 Claude Code 實測通過。
- 2026-11-19：#12 新增的修正：
  - Linux 的強制結束改為對整個程序群組送 `killpg`，並且只在群組還有成員時才送訊號。
  - Windows 的 ACL 改用原生 API。daemon 啟動從 6–9 秒降到約 0.4 秒，這也是 #13 那個 8 秒門檻問題的根因。
  - SQLite 暫存檔在初始化時的競態。
  - Ctrl-C 忽略旗標會被子程序繼承，導致 Windows 上的優雅停止失效。
  - lifecycle 測試會因為 port 重用而互相干擾。
- 2026-11-19：本機在測試失敗時留下 3 個 `linkstart.exe`（PID 48212、39048、36576）。自動模式不允許直接結束程序，所以這 3 個程序仍在執行。
