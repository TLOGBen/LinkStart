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

1. A8 第一輪審查判定 FAIL（M1、M2 兩項 med），修正在 #22。合併後在最終 SHA 上重新審查，並再跑一次 A7。

## 驗收項目

| # | 項目 | 狀態 | 證據（SHA／平台／紀錄） |
|---|---|---|---|
| A1 | 任意 TUI CLI 可在 Runtime PTY 中啟動、操作、讀回輸出 | 證實（Windows，API 與 Runtime 擁有的終端） | `f49b44f`／Windows／`prove-it/records/024624b-claude-pty-pilot.md`（sha256 `bf0a877d…`）；A4、A5、A7 的實測中又各跑了一次 Claude Code 問答 |
| A2 | 兩個以上 CLI 並存於同一 TaskSpace，瀏覽器與 CLI 都能操作，可交接 | 證實（Windows） | daemon `a1571fb`＋agent 端 `--address`／Windows／`prove-it/records/a1571fb-a2-two-cli-handoff.md`：Codex（Sol 6.2，沙箱內）認領並送審 → Claude（haiku）核准 → 人在瀏覽器上按測試通過，`done r4`；權限邊界 4 類全部被拒 |
| A3 | 撤權與角色檢查 | 證實（兩個平台） | #16 `2dae7e3`：收回 grant、降級角色、撤銷憑證、rotate 之後，被撤權者的下一次寫入被拒（403／401），手上的控制租約在撤權的同一刻結束；憑證「到期」沒有對應的撤權呼叫，租約是在下一個人 acquire 時才結束，對要接手的人來說結果相同，可以馬上取得控制；observe 每次讀取前後各授權一次 |
| A4 | 人類啟動設定（核准執行檔、參數、工作區） | 證實（兩個平台；真 daemon 在 Windows） | #17 `14039e7`：只有 owner／人類管理員能指定執行檔，agent 只能 `terminal.restart` 核准過的設定；PTY 終端的子程序不繼承宿主 session 變數（實測 `CLAUDE*` 0 個），使用者自己的設定和無頭登入用的 `CLAUDE_CODE_OAUTH_TOKEN` 保留（後者在 #22 修正）。owned Codex host 和 `gh` 不在過濾範圍內（K13）；管理介面有 Esc／方向鍵／Tab／Enter／Ctrl-C 單鍵 |
| A5 | 重啟恢復；結果未知的操作不報成功 | 證實（Windows 當機實測；兩個平台測試） | #18：`taskkill /F` 之後，子程序樹一起結束；任務仍是 `in_progress`、assignee 不變；history 讀得到當機前的回答；同一個 request_id 重送 claim，結果相同；重送 create 回 `UnknownEffect`；agent 重啟核准的終端後可以回答。會改變狀態的操作 panic 時回 `UnknownEffect`；dispatch 鎖（#18）和 Engine 鎖（#22）中毒後都會恢復，Runtime 仍然可以使用、可以停止。這兩條 panic 路徑中，只有 Engine 這條有直接的測試（`panic_mid_transaction_leaves_engine_usable_and_owner_recognized`），dispatch 這條只經過程式碼審查。關閉中的 Runtime 回 `draining`，`start` 不會重用它。儲存初始化另有假說證明（`.claude/hypothesis/storage-init/`） |
| A6 | GitHub 持續處理循環 | 證實（Windows，TLOGBen/LinkStart） | A3＋A4 整合 build：watch 每 30 秒同步一次，新 issue #21 在 15.2 秒內變成任務 → 認領 → 送審 → 核准 → 測試通過 `done` → `sources.writeback` 留言回 issue，用 `gh` 讀回確認 → issue 關閉。#19、#20 是腳本錯誤時開的測試 issue，都已關閉 |
| A7 | 真實閒置至少 65 分鐘後，同一 Session 仍可回應 | 進行中 | A4 第一版的 build（`96c59c9`）：60 秒冒煙測試通過；正式的 65 分鐘還在跑 |
| A8 | 同一個 SHA 上的最終整合審查 | 未開始 | |

## PR 狀態

| PR | 分支 | 狀態 | 驗證 |
|---|---|---|---|
| #12 | feature/cli-runtime-shutdown | 已合併 `46f495d` | Windows／Linux 全套測試、CI 三平台綠、獨立 review（5 項：3 項已修，2 項轉為 K1、K2） |
| #13 | feature/runtime-incarnation | 已合併 `ad749d2` | 兩個平台全套測試、CI 三平台綠、獨立 review MERGE-OK（後續處理 K3、K4） |
| #14 | feature/agent-saved-plan-recovery | 已合併 `214104a` | F1／F2 先 RED 後 GREEN；review 指出 F1 的另一條路徑，已補修（`d3daae7`）；CI 三平台綠 |
| #15 | feature/agent-direct-connect | 已合併 `5601c38` | `--address`；K1 部分修正；HUNT-2026-003 測試修正 |
| #16 | feature/a3-revocation | 已合併 `2dae7e3` | RED → GREEN；獨立 review 找到 1 項 med（憑證到期沒有結束租約），已修（`93de904`）；兩個平台全套測試、CI 三平台綠 |
| #17 | feature/a4-launch-setup | 已合併 `14039e7` | RED → GREEN；review 找到 1 項 med（前綴規則誤刪使用者設定），已改成明確清單；合進 main 之後兩個平台重跑；CI 三平台綠 |
| #18 | feature/a5-restart-recovery | 已合併 `482a930` | K3／K9；review 找到 1 項 med（panic 把鎖毒死，連 stop 都停不掉），已修（`16b03a1`）；HUNT-2026-004 測試修正；CI 三平台綠 |
| #22 | feature/a8-fixes | 待合併 | A8 審查的 M1（Engine 鎖中毒後無法停止）與 M2（刪掉使用者的 OAuth token）；關閉中仍可撤銷憑證 |

## 已知問題

- **K1（部分已修）**：`terminal.observe` 和 `/v2/health` 已經移到鎖外。會改變狀態的終端操作（write、acquire、stop、create）仍然依序經過同一把鎖：某個 agent 的 stop 最多 500 毫秒、create 最多 5 秒，這段時間其他 agent 的寫入會被延後，但不會失敗。
- **K2**：`NativeState.receipts` 會隨每次寫入或續約無限增長。長時間運作時記憶體會慢慢變大；A7 的 65 分鐘閒置沒有寫入，所以沒有測到這點。
- **K4**（#13 review）：`runtime_identity::response` 遇到 `Connection: close` 會丟掉已經完整收到的最終回應（`runtime_identity.rs`）。
- **K5**（#14 review）：加鎖前的空窗期如果寫入了較新的 in-memory fallback，恢復流程只合併加鎖前讀到的舊版本，最新的私有錯誤紀錄會遺失（`runtime_agent.rs`）。
- **K6**（假說 storage-init）：`protect_if_present` 在「刪除中」時依賴約 1 秒內完成；可以改成刪除中就跳過，移除這個時間前提。其他資料庫的 `protect` 用的也是「先檢查存在再設 ACL」的寫法。
- **K7**（假說的第二份推導）：其他程序的 SQLite 寫入交易如果超過 `busy_timeout` 的 10 秒，`Engine::open` 會以 `SQLITE_BUSY` 失敗。
- **K8**（`--address`，中高嚴重度）：不驗證 Runtime 身分。舊的 port 如果被其他本機程序佔走，成員憑證會被送過去。建議在送出憑證前，讓伺服器先證明它知道這個憑證（nonce＋HMAC）。在這之前，不要讓 agent 拿真正的憑證使用 `--address`。
- **K10**（#17 review）：核准紀錄不會重新檢查核准人；核准人之後被撤權，agent 仍然可以重啟該終端。另外，核准一個 shell（例如 `pwsh.exe`）就等於讓有 send 權限的 agent 能在裡面執行任何指令。`LaunchPolicyStore::admin()` 接受 agent 的 manager 角色，目前沒有對外開放；將來若要開放 install，必須改成只允許人類。
- **K11**（#18 review）：`daemon stop` 遇到一直不退出的終端時只會回 pending，需要改用 `linkstart tool` 送 `runtime.force_shutdown`，錯誤訊息沒有提示這條路。（Engine 鎖中毒的部分已在 #22 修正。）
- **K13**（A8 review）：環境變數過濾只套用在 PTY 終端；owned Codex host（`runtime_codex_host.rs`）和 `gh`（`runtime_source.rs`）仍繼承全部環境變數。
- **K14**（A8 review）：用同一個 request_id 重送 `members.rotate_credential` 時，會再跑一次撤權擋門，誤結束 agent 用新憑證剛取得的租約。只會讓 agent 重新 acquire，不是安全問題。
- **K15**（A8 review）：#17 之前建立的 terminal endpoint 沒有 `launch_approval`，連 owner 都無法 restart，也沒有補核准的工具；只能建新的 endpoint，history 就接不上舊的。
- **K16**（A8 review）：撤權擋門和 shutdown 會對每個 session 的 mutex 做 `lock().unwrap()`，只要有一個 session 鎖被毒死就會連鎖 panic。session 鎖內的程式碼目前寫得很保守，觸發機率低。
- **K12**（觀察）：`tests/owned_supervisor.rs` 的 `running()` 5 秒期限在人為高負載下（同時跑 4 份壓力測試）會 17/25 失敗，空閒時 25/25 通過。和 HUNT-2026-003 同一類；正常負載下若出現再開案。
- 已修：K3（#18）、K9（#18）；A8 審查的 M1、M2 和「關閉中無法撤權」（#22）。

## 紀錄

- 2026-11-19：Claude 接手。前導測試發現 `terminal.write` 會拒絕只含空白的輸入（例如單獨的 Enter），已修正並用真的 Claude Code 實測通過。
- 2026-11-19：#12 新增的修正：Linux 對整個程序群組強制結束；Windows ACL 改用原生 API（啟動從 6–9 秒降到約 0.4 秒）；SQLite 暫存檔在初始化時的競態；Ctrl-C 忽略旗標被子程序繼承；lifecycle 測試因 port 重用而互相干擾；macOS CI 映像被淘汰。
- 2026-11-19：A2 前導片（兩個真實 CLI 交接）走通；新增 `--address`；修了 K1 的觀看與健康檢查部分。合併 #12、#13、#14、#15。
- 2026-11-19：`/baransu:hypothesis` storage-init；`/baransu:hunt` HUNT-2026-003（`owned_codex_host` fixture 的 500ms 逾時）。
- 2026-11-19：本機留下 3 個 `linkstart.exe`（PID 48212、39048、36576），自動模式不允許直接結束，仍在執行。
- 2026-11-20：Claude 上一輪在合併完 #15 後停下來問使用者要不要繼續，使用者不在，空轉了 14 小時。之後改為一路做到目標完成。
- 2026-11-20：A3（#16）、A4（#17）合併；A5（#18）完成審查修正；A6 在 TLOGBen/LinkStart 上走完一次完整迴圈；A7 的 65 分鐘閒置開始跑。`/baransu:hunt` HUNT-2026-004：`runtime_agent` 的鎖在 drop 後仍被別的執行緒 fork 出的子程序短暫握住（約 1ms），已修正測試。
