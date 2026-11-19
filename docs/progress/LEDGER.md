# LinkStart 進度帳

**用法**：每完成一步就更新這份檔案並推上 GitHub。新的 session 先讀這份，再核對 git、PR 和實際程序；任何「完成」都要附上 SHA、平台、指令和證據。原始證據留在本機（`.claude/prove-it/`、`.claude/hypothesis/`、`.claude/hunt-report/`、`.codex/`），這裡只記路徑和雜湊。

## 目標（2026-11-19 已確認）

多 Agent 協作平台：

- 任何 CLI 都能由 Runtime 用 PTY 啟動並持有，在同一個 TaskSpace 裡分工、交接、留言。
- 人從管理介面查看與控制；可以接 GitHub issue／PR 持續處理。
- 離開、真實閒置至少 65 分鐘，或 Runtime 重啟之後，工作都能接續。
- 有結構化協定的 CLI（例如 Codex）屬於選配強化，不是加入平台的前提。

計畫：`.claude/think/linkstart-resume.md`（本機）。

## 下一步

1. 下一片是 A3：撤權之後，正在連線中的串流要立刻停止；角色降級後，下一次操作就要被拒絕。
2. 之後依序做 A4（人類啟動設定，包括首次啟動時的對話框，以及子程序的環境變數過濾）、A5、A6、A7、A8。

## 驗收項目

| # | 項目 | 狀態 | 證據（SHA／平台／紀錄） |
|---|---|---|---|
| A1 | 任意 TUI CLI 可在 Runtime PTY 中啟動、操作、讀回輸出 | 部分證實：單一 CLI、API 入口 | `f49b44f`／Windows／`prove-it/records/024624b-claude-pty-pilot.md`（sha256 `bf0a877d…`），修正後在新資料夾完成信任詢問加問答 |
| A2 | 兩個以上 CLI 並存於同一 TaskSpace，瀏覽器與 CLI 都能操作，可交接 | 前導片證實（Windows）；K1 的觀看與健康檢查部分已修 | daemon `a1571fb`＋agent 端 `--address`／Windows／`prove-it/records/a1571fb-a2-two-cli-handoff.md`：Codex（Sol 6.2，沙箱內）認領並送審 → Claude（haiku）核准 → 人在瀏覽器上按測試通過，`done r4`；權限邊界 4 類全部被拒 |
| A3 | 撤權與角色檢查 | 未開始 | |
| A4 | 人類啟動設定（核准執行檔、參數、工作區） | 契約已審，未實作 | |
| A5 | 重啟恢復；結果未知的操作不報成功 | 局部證據；儲存初始化已用假說證明 | `.claude/hypothesis/storage-init/`（`c909432`）：任何交錯、任一步當機下 owner token 唯一、不半寫、不輪替；R4 依賴「刪除中狀態 < 約 1 秒」 |
| A6 | GitHub 持續處理循環 | 未開始 | |
| A7 | 真實閒置至少 65 分鐘後，同一 Session 仍可回應 | 未開始 | |
| A8 | 同一個 SHA 上的最終整合審查 | 未開始 | |

## PR 狀態

| PR | 分支 | 狀態 | 驗證 |
|---|---|---|---|
| #12 | feature/cli-runtime-shutdown | 已合併 `46f495d` | Windows／Linux 全套測試、CI 三平台綠、獨立 review（5 項：3 項已修，2 項轉為 K1、K2） |
| #13 | feature/runtime-incarnation | 已合併 `ad749d2` | 兩個平台全套測試、CI 三平台綠、獨立 review MERGE-OK（後續處理 K3、K4） |
| #14 | feature/agent-saved-plan-recovery | 已合併 `214104a` | F1／F2 先 RED 後 GREEN；review 指出 F1 的另一條路徑，已補修（`d3daae7`）；CI 三平台綠 |
| #15 | feature/agent-direct-connect | 本次合併 | `--address`；K1 部分修正；HUNT-2026-003 測試修正；兩個平台全部 Rust 測試和 public HTTP 22/22 通過 |

## 已知問題（未修，已排入切片）

- **K1（部分已修）**：`terminal.observe` 和 `/v2/health` 已經移到鎖外。會改變狀態的終端操作（write、acquire、stop、create）仍然依序經過同一把鎖：某個 agent 的 stop 最多 500 毫秒、create 最多 5 秒，這段時間其他 agent 的寫入會被延後，但不會失敗。
- **K2**：`NativeState.receipts` 會隨每次寫入或續約無限增長。排在 A7。
- **K3**（#13 review）：v1 health 寫死回 `ready`，正在關閉的 Runtime 會被 `daemon start` 誤判成可以重用（`server.rs:249-252`、`runtime_identity.rs:149`）。
- **K4**（#13 review）：`runtime_identity::response` 遇到 `Connection: close` 會丟掉已經完整收到的最終回應（`runtime_identity.rs:83-89`）。
- **K5**（#14 review）：加鎖前的空窗期如果寫入了較新的 in-memory fallback，恢復流程只合併加鎖前讀到的舊版本，最新的私有錯誤紀錄會遺失（`runtime_agent.rs:534/565/664`）。
- **K6**（假說 storage-init）：`protect_if_present` 在「刪除中」時依賴約 1 秒內完成；可以改成刪除中就跳過，移除這個時間前提。其他資料庫（participants、launch_policy、control_bridge）的 `protect` 用的是同樣的「先檢查存在再設 ACL」寫法，也可能時好時壞。
- **K7**（假說的第二份推導）：其他程序的 SQLite 寫入交易如果超過 `busy_timeout` 的 10 秒，`Engine::open` 會以 `SQLITE_BUSY` 失敗。
- **K8**（`--address`，#15 review 第 1 項，中高嚴重度）：不驗證 Runtime 身分。舊的 port 如果被其他本機程序佔走，成員憑證會被送過去。建議的修法是在送出憑證前，讓伺服器先證明它知道這個憑證（nonce＋HMAC）。在這之前，不要讓 agent 拿真正的憑證使用 `--address`。
- **K9**（#15 review 第 5 項）：`spawn_blocking` 裡發生 panic 時，會變成 `Internal` 500。對於會改變狀態的工具，應該回 `UnknownEffect`，代表結果未知，而不是確定失敗。

## 紀錄

- 2026-11-19：Claude 接手。前導測試發現 `terminal.write` 會拒絕只含空白的輸入（例如單獨的 Enter），已修正並用真的 Claude Code 實測通過。
- 2026-11-19：#12 新增的修正：Linux 對整個程序群組強制結束（只在群組有成員時送訊號）；Windows ACL 改用原生 API（啟動從 6–9 秒降到約 0.4 秒，也是 #13 那個 8 秒門檻問題的根因）；SQLite 暫存檔在初始化時的競態；Ctrl-C 忽略旗標被子程序繼承；lifecycle 測試因 port 重用而互相干擾；macOS CI 映像被淘汰。
- 2026-11-19：A2 前導片（兩個真實 CLI 交接）走通；新增 `--address`；修了 K1 的觀看與健康檢查部分。
- 2026-11-19：使用者開放合併權限，依序合併 #12、#13、#14。
- 2026-11-19：`/baransu:hypothesis` storage-init：任何交錯下 R1–R5 守住（3 個程序窮舉；獨立推導做到 4 個程序），R4 的時間前提是緊的。`/baransu:hunt` HUNT-2026-003：`owned_codex_host` 偶發失敗，根因是 fixture 的 500ms 逾時，已改成 5 秒。
- 2026-11-19：本機在測試失敗時留下 3 個 `linkstart.exe`（PID 48212、39048、36576）。自動模式不允許直接結束程序，所以這 3 個程序仍在執行。
