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

A1–A8 全部達成（最終 SHA `a1e5588`）。之後的工作是處理「已知問題」：K8（#24）、K10（#25）、K2（#27）、K15（#29）已修正；其餘照 `已知問題` 一項項處理，K11、K13、K4、K5–K7 由並行 session 各自修。

## 驗收項目

| # | 項目 | 狀態 | 證據（SHA／平台／紀錄） |
|---|---|---|---|
| A1 | 任意 TUI CLI 可在 Runtime PTY 中啟動、操作、讀回輸出 | 證實（Windows，API 與 Runtime 擁有的終端） | `f49b44f`／Windows／`prove-it/records/024624b-claude-pty-pilot.md`（sha256 `bf0a877d…`）；A4、A5、A7 的實測中又各跑了一次 Claude Code 問答 |
| A2 | 兩個以上 CLI 並存於同一 TaskSpace，瀏覽器與 CLI 都能操作，可交接 | 證實（Windows） | daemon `a1571fb`＋agent 端 `--address`／Windows／`prove-it/records/a1571fb-a2-two-cli-handoff.md`：Codex（Sol 6.2，沙箱內）認領並送審 → Claude（haiku）核准 → 人在瀏覽器上按測試通過，`done r4`；權限邊界 4 類全部被拒 |
| A3 | 撤權與角色檢查 | 證實（兩個平台） | #16 `2dae7e3`：收回 grant、降級角色、撤銷憑證、rotate 之後，被撤權者的下一次寫入被拒（403／401），手上的控制租約在撤權的同一刻結束；憑證「到期」沒有對應的撤權呼叫，租約是在下一個人 acquire 時才結束，對要接手的人來說結果相同，可以馬上取得控制；observe 每次讀取前後各授權一次 |
| A4 | 人類啟動設定（核准執行檔、參數、工作區） | 證實（兩個平台；真 daemon 在 Windows） | #17 `14039e7`：只有 owner／人類管理員能指定執行檔，agent 只能 `terminal.restart` 核准過的設定；PTY 終端的子程序不繼承宿主 session 變數（實測 `CLAUDE*` 0 個），使用者自己的設定和無頭登入用的 `CLAUDE_CODE_OAUTH_TOKEN` 保留（後者在 #22 修正）。owned Codex host 和 `gh` 不在過濾範圍內（K13）；管理介面有 Esc／方向鍵／Tab／Enter／Ctrl-C 單鍵 |
| A5 | 重啟恢復；結果未知的操作不報成功 | 證實（Windows 當機實測；兩個平台測試） | #18：`taskkill /F` 之後，子程序樹一起結束；任務仍是 `in_progress`、assignee 不變；history 讀得到當機前的回答；同一個 request_id 重送 claim，結果相同；重送 create 回 `UnknownEffect`；agent 重啟核准的終端後可以回答。會改變狀態的操作 panic 時回 `UnknownEffect`；dispatch 鎖（#18）和 Engine 鎖（#22）中毒後都會恢復，Runtime 仍然可以使用、可以停止。這兩條 panic 路徑中，只有 Engine 這條有直接的測試（`panic_mid_transaction_leaves_engine_usable_and_owner_recognized`），dispatch 這條只經過程式碼審查。關閉中的 Runtime 回 `draining`，`start` 不會重用它。儲存初始化另有假說證明（`.claude/hypothesis/storage-init/`） |
| A6 | GitHub 持續處理循環 | 證實（Windows，TLOGBen/LinkStart） | A3＋A4 整合 build：watch 每 30 秒同步一次，新 issue #21 在 15.2 秒內變成任務 → 認領 → 送審 → 核准 → 測試通過 `done` → `sources.writeback` 留言回 issue，用 `gh` 讀回確認。Runtime 只會留言，issue 是驗收腳本用 `gh` 關的。#19、#20 是腳本錯誤時開的測試 issue，都已關閉 |
| A7 | 真實閒置至少 65 分鐘後，同一 Session 仍可回應 | 證實 `a1e5588` | Windows，Claude Code（haiku）在 Runtime 擁有的 PTY 中，閒置 3,930 秒（65.5 分鐘）後同一 Session 仍為 `live` 並回答問題，daemon 未重啟。三次正式執行：A4 第一版 `96c59c9`（閒置 3,930 秒，之後 6.1 秒回答）、`482a930`（3,930 秒，4.0 秒）、最終 `a1e5588`（3,930 秒，閒置前 6.1 秒、閒置後 4.1 秒回答）。一次執行因電腦意外關機中斷，作廢重跑 |
| A8 | 同一個 SHA 上的最終整合審查 | 證實 `a1e5588` | 獨立 reviewer 審 `5601c38..a1e5588`（#16、#17、#18、#22）。第一輪（`482a930`）判 FAIL：M1 Engine 鎖中毒後 Runtime 停不掉，M2 刪掉使用者的 OAuth token，兩項都在 #22 修正。第二輪（`a1e5588`）判 PASS：沒有 high／med，剩下的 low 記為 K17、K18。同一個 SHA：Windows 與 Linux 全套測試（HTTP 各 25/25）、CI 三平台綠 |

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
| #22 | feature/a8-fixes | 已合併 `a1e5588` | A8 審查的 M1（Engine 鎖中毒後無法停止）與 M2（刪掉使用者的 OAuth token）；關閉中仍可撤銷憑證 |
| #23 | docs/goal-complete | 已合併 `8407af1` | 帳本：A1–A8 全部達成 |
| #24 | fix/k8-address-auth | 已合併 | K8：`--address` 先驗證 Runtime 身分再送憑證；HUNT-2026-005 測試修正 |
| #25 | fix/k10-approval-recheck | 已合併 | K10：啟動核准不會比核准人的信任活得久（終端 restart 與 owned launch plan）；agent manager 不能改人類的角色與憑證 |
| #26 | chore/lint | 已合併 `32a16bc` | 啟用 baransu lint，加上 LinkStart 自己的 5 條檢查（`.claude/lint/sets/local/linkstart.toml`） |
| #27 | fix/k2-receipts | 已合併 `270adb3` | K2：終端請求的收據有上限，過期留墓碑，重送永遠不會再執行一次 |
| #29 | fix/k15-approve-launch | 已合併 | K15：`endpoints.approve_launch` 讓現任人類替既有終端補核准，history 不中斷 |

## 已知問題

- **K1（部分已修）**：`terminal.observe` 和 `/v2/health` 已經移到鎖外。會改變狀態的終端操作（write、acquire、stop、create）仍然依序經過同一把鎖：某個 agent 的 stop 最多 500 毫秒、create 最多 5 秒，這段時間其他 agent 的寫入會被延後，但不會失敗。
- **K4**（#13 review）：`runtime_identity::response` 遇到 `Connection: close` 會丟掉已經完整收到的最終回應（`runtime_identity.rs`）。
- **K5**（#14 review）：加鎖前的空窗期如果寫入了較新的 in-memory fallback，恢復流程只合併加鎖前讀到的舊版本，最新的私有錯誤紀錄會遺失（`runtime_agent.rs`）。
- **K6**（假說 storage-init）：`protect_if_present` 在「刪除中」時依賴約 1 秒內完成；可以改成刪除中就跳過，移除這個時間前提。其他資料庫的 `protect` 用的也是「先檢查存在再設 ACL」的寫法。
- **K7**（假說的第二份推導）：其他程序的 SQLite 寫入交易如果超過 `busy_timeout` 的 10 秒，`Engine::open` 會以 `SQLITE_BUSY` 失敗。
- **K11**（#18 review）：`daemon stop` 遇到一直不退出的終端時只會回 pending，需要改用 `linkstart tool` 送 `runtime.force_shutdown`，錯誤訊息沒有提示這條路。（Engine 鎖中毒的部分已在 #22 修正。）
- **K13**（A8 review）：環境變數過濾只套用在 PTY 終端；owned Codex host（`runtime_codex_host.rs`）和 `gh`（`runtime_source.rs`）仍繼承全部環境變數。
- **K14**（A8 review）：用同一個 request_id 重送 `members.rotate_credential` 時，會再跑一次撤權擋門，誤結束 agent 用新憑證剛取得的租約。只會讓 agent 重新 acquire，不是安全問題。
- **K16**（A8 review）：撤權擋門和 shutdown 會對每個 session 的 mutex 做 `lock().unwrap()`，只要有一個 session 鎖被毒死就會連鎖 panic。session 鎖內的程式碼目前寫得很保守，觸發機率低。
- **K12**（觀察）：`tests/owned_supervisor.rs` 的 `running()` 5 秒期限在人為高負載下（同時跑 4 份壓力測試）會 17/25 失敗，空閒時 25/25 通過。和 HUNT-2026-003 同一類；正常負載下若出現再開案。
  - 2026-11-20，另外 4 個 session 同時編譯、跑測試時，Windows 上又有兩個同類的「N 秒內要完成」測試各失敗一次：`runtime_watch::tests::stop_preserves_inflight_accounting_until_bounded_source_job_finishes`（15 秒）和 `tests/runtime_control_bridge.rs` 的 `f5_get_first_completed_preserves_typed_observation`（`running()` 5 秒）。單獨重跑各 5/5 通過；兩者都不在 #29 改到的範圍。
- **K17**（A8 第二輪 L1）：`gated` panic 之後清掉中毒標記，但沒有遞增 impact epoch。如果 owner 已經拿到 shutdown ticket，而 panic 的 `terminal.create` 已經 spawn 了程序，舊 ticket 仍然有效，確認時看到的影響清單會少這個程序；不過 cleanup 仍會把它結束。建議在 `clear_poison` 之後鎖一次 gate、做 `epoch += 1`。
- **K18**（A8 第二輪 L2）：shutdown admission 的 closure 直接取 gate，在裡面 panic 時不會 `clear_poison`，gate 會一直中毒，v1／v2 health 的回報也不一致。程式碼很短，觸發機率極低。A5 寫的「dispatch 鎖中毒後會恢復」不涵蓋這條路徑。
- **K19**（#24 審查，low）：`--address` 的 Runtime 證明有幾個殘餘限制。
  - 證明沒有綁住之後的 TCP 通道。同一使用者的程式若在 Windows 上用 SO_REUSEADDR 和真 Runtime 共用同一個 port，仍可能截走連線；不過同一使用者本來就讀得到私有狀態目錄。
  - 持有 `credential_hash` 就能替該憑證偽造證明。目前 API 不輸出它，但不能讓它出現在 log 或備份裡。
  - `/v2/prove` 對知道 key_id 的人透露該憑證是否仍有效。
  - 伺服器只綁 `127.0.0.1`，client 用 `[::1]` 或 `127.0.0.2` 一律證明失敗，錯誤訊息不會指出這點。
- **K20**（HUNT-2026-005 另立案）：`runtime_terminal::tests::slow_observer_gets_gap_while_other_terminal_stays_responsive`（`runtime_terminal.rs:956`）在 Linux 4 份並行壓力下約 1/100 失敗，序列模式也一樣，和負載有關，屬於 K12 同類。
- **K21**（#25 審查後剩下的 low 與設計邊界）：
  - 核准一個 shell（例如 `pwsh.exe`）就等於讓有 send 權限的 agent 能在裡面執行任何指令。這是設計本身的結果，核准 shell 前要知道這點。
  - 憑證外洩時要先 `revoke_credential` 再 `rotate`；只做 rotate 不算信任中斷，攻擊者冒用期間做的核准會留下。
  - 升級前就發生的降級或撤銷沒有留下信任世代，被 owner 恢復後，升級前的舊核准仍算數。
  - workspace write 核准（`approve_workspace`）記下的核准人不會再被重新檢查；要靠 install 時的安裝者檢查擋。
  - agent manager 仍可撤銷 launch plan，最多讓 space 內的 plan 全部失效（只會減少權限）。
  - owned launch 在 `approve()` 檢查安裝者之後、最後一次確認之前，有幾毫秒的空窗；撤銷本來就不會停掉已在跑的程序。
- 已修：K3（#18）、K9（#18）；A8 審查的 M1、M2 和「關閉中無法撤權」（#22）；K8（#24：`--address` 送憑證前，Runtime 先用 HMAC 證明它知道這個憑證，證明綁定它自己的位址）；K10（#25：每位人類成員有一個信任世代，撤銷憑證或拿掉 `human_admin` 時加 1；終端核准與 launch plan 記下核准當下的世代，啟動時不相等就拒絕，所以重新發憑證或恢復角色都不會讓舊核准復活；agent manager 不能改人類的角色與憑證，也不能安裝 launch plan；沒有安裝者紀錄的舊 plan 必須重新安裝）；K2（#27：每個身分保留最新 2,000 筆、1 小時內的收據；收據只存請求的 SHA-256，不存內容；過期的變成墓碑，重送回 UnknownEffect，不會再執行一次；墓碑總數上限 20 萬，最舊的先忘。一個請求從開始到完成只佔一格，別的身分也擠不掉你的「結果未知」紀錄）；K15（#29：新工具 `endpoints.approve_launch`，只有 owner 或人類管理員能用，替指定程式的既有終端補上核准並記下當下的信任世代；呼叫時必須帶上和終端完全相同的 `expected_config`，人類核准的是自己看過的程式，不是一個 ID；上一筆核准留在 `previous_approval`。舊終端，或核准人已失去信任的終端，都能就地重新核准，history 接得上）。
- **K22**（#27 審查，low）：
  - `native-intents/` 底下 create、restart 的意圖標記檔從不清除，磁碟上會無限增長，屬於 K2 同類。
  - 墓碑池是所有身分共用的 20 萬筆；別的身分連續送超過 20 萬次請求，可以把你已過期的墓碑擠掉，那之後重送同一個 ID 會被當成新請求。
  - 只在同一個身分有新請求時才清它的過期收據；停用的身分最多留著 2,000 筆（總量由成員數決定）。
  - 參數錯誤（缺 generation、lease 不對）發生在動作之前，但同一個 ID 重送仍回 UnknownEffect（舊行為，不是這次引入）。

## 紀錄

- 2026-11-19：Claude 接手。前導測試發現 `terminal.write` 會拒絕只含空白的輸入（例如單獨的 Enter），已修正並用真的 Claude Code 實測通過。
- 2026-11-19：#12 新增的修正：Linux 對整個程序群組強制結束；Windows ACL 改用原生 API（啟動從 6–9 秒降到約 0.4 秒）；SQLite 暫存檔在初始化時的競態；Ctrl-C 忽略旗標被子程序繼承；lifecycle 測試因 port 重用而互相干擾；macOS CI 映像被淘汰。
- 2026-11-19：A2 前導片（兩個真實 CLI 交接）走通；新增 `--address`；修了 K1 的觀看與健康檢查部分。合併 #12、#13、#14、#15。
- 2026-11-19：`/baransu:hypothesis` storage-init；`/baransu:hunt` HUNT-2026-003（`owned_codex_host` fixture 的 500ms 逾時）。
- 2026-11-19：本機留下 3 個 `linkstart.exe`（PID 48212、39048、36576），自動模式不允許直接結束；2026-11-20 電腦重開機後已不存在。
- 2026-11-20：Claude 上一輪在合併完 #15 後停下來問使用者要不要繼續，使用者不在，空轉了 14 小時。之後改為一路做到目標完成。
- 2026-11-20：A3（#16）、A4（#17）合併；A5（#18）完成審查修正；A6 在 TLOGBen/LinkStart 上走完一次完整迴圈；A7 的 65 分鐘閒置開始跑。`/baransu:hunt` HUNT-2026-004：`runtime_agent` 的鎖在 drop 後仍被別的執行緒 fork 出的子程序短暫握住（約 1ms），已修正測試。
- 2026-11-20：A8 第二輪審查在 `a1e5588` 通過（#22 合併）。電腦意外關機中斷了 `a1e5588` 的 A7，重跑後閒置 3,930 秒、同一 Session 4.1 秒回答。A1–A8 全部達成。
- 2026-11-20：K8（#24）：舊版 binary 會把憑證送給冒充者（新測試在 `a1e5588` 上失敗）。修正後，自己回應、原封轉送、改寫 Host 轉送這三種冒充者都拿不到憑證。`/baransu:hunt` HUNT-2026-005：Linux 並行下 `runtime_agent` 恢復測試 3/100 失敗，序列模式 0/100，和 HUNT-2026-004 同因；修正後並行 0/100。
- 2026-11-20：K10（#25）：三輪獨立審查。第一輪找到 agent manager 能把被降級的人類升回去（high）與 owned launch 不重查安裝者（med）；第二輪找到撤銷後重發憑證會讓舊核准復活（med）；第三輪找到用時鐘比較會被時鐘倒退騙過（med），改成每位成員的信任世代計數。每個新測試都確認過：拿掉對應的檢查就會失敗。
