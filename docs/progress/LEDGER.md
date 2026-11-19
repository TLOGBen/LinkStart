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

Review #12，並重現各項 blocker（Linux public HTTP 失敗、#13 Windows 啟動延遲、#14 F1／F2 與 CI）。

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

| PR | 分支 | head | 狀態 | 卡點 |
|---|---|---|---|---|
| #12 | feature/cli-runtime-shutdown | `f49b44f` | Draft | 已修空白按鍵被拒（F1-terminal）；待 review、Linux public HTTP 4 項失敗、incarnation 缺口（由 #13 修） |
| #13 | feature/runtime-incarnation | `024624b` | Draft | Windows 啟動／重啟超過 8 秒門檻；需 rebase 到 `f49b44f` |
| #14 | feature/agent-saved-plan-recovery | `7088aca` | Draft | F1 診斷被覆寫、F2 舊 generation 錯誤污染；CI 三平台失敗 |

## 紀錄

- 2026-11-19：Claude 接手。前導測試發現，`terminal.write` 會把只含空白的輸入（例如單獨的 Enter）當成無效而拒絕。已補測試，先確認測試失敗（RED），修正後通過（GREEN），並用真的 Claude Code 實測通過。
