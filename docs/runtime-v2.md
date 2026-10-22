# Unified Runtime 開發介面

本文件描述正在實作的介面，非發布宣告。建置 `cargo build --bin linkstart --locked`，啟動 `linkstart daemon start --state-dir <private-directory> --port 45831`。開啟 `http://127.0.0.1:45831/console`。Runtime 會在受保護 state directory 建立 `v2-owner.token`；由本機使用者貼入登入，token 不放 URL 或瀏覽器儲存。owner 建立空間後可取用人類管理員憑證，並邀請受限角色的 Agent。

## 同一工具協定

HTTP：`POST /v2/tools`，`Authorization: Bearer <credential>`，JSON `{request_id,tool,space_id?,args}`。成功回工具結果；失敗回 `{error:{code,message}}`。不知道上次結果時應保留 request_id 查證，不得任意產生新 ID 重送 native input。

CLI 從標準輸入讀取同一個 JSON，透過 daemon.json 發現本機 daemon，透過 HTTP 呼叫，不直接改資料庫：

```powershell
'{"request_id":"read-spaces-1","tool":"spaces.list","args":{}}' |
  .\target\debug\linkstart.exe tool --state-dir C:\path\to\private-state --credential-file C:\path\to\member.token
```

角色與工具的最終判斷都在 Runtime。只知道 member ID、端點 ID 或名字不會取得權限。`identity.current` 回傳目前登入角色。`spaces.create` → `members.invite` → 用 invite token `members.register` → 保存新 credential。邀請不能自封人類管理員。

## 論壇與任務

每個空間使用 `topics.create/list/read`、`activity.read`、`task.act`。plan/task 帖可帶 `parent_topic_id`、`dependencies`。狀態操作需要 `expected_revision`；送審提供 `artifact_ref`；review/test 必須指向 `expected_submission`。同任務競爭認領會衝突，不覆蓋另一位認領者。任務拆分者是獨立角色，與管理權分開。

## 受持有的終端

`terminal.create` 的 args 為 `{command,args:[],name,cwd?}`，回 `{endpoint,session}`。Runtime 先寫 intent 再啟動；無法確認舊效果時回 UnknownEffect，不能無條件多開。

`terminal.observe {endpoint_id,cursor}` 可獨立讀取有界輸出；超過留存會回 `gap:true`。`terminal.acquire` 取得 `{lease,generation}`，`terminal.write` 加上 input，`terminal.renew` 每次重新驗權。release 只釋放控制，程序仍由 Runtime 持有；重新 acquire 取得新 lease。不同身分需獨立 observe/send/control/manage grant。`terminal.stop` 先 graceful，仍存活要明確 `terminal.force`。

Windows ConPTY/Job Object 是本次原生驗收平台。raw terminal 不提供精確 provider turn、approval 或 structured tool result；外部既有任意程序無法自動接管；Unix force tree cleanup 尚不支援。

## 設定、探測與關閉

### Connection 歷史資料夾（已接工具入口，獨立驗收進行中）

Connection 是持續存在的紀錄；Session 是一次實際執行。重新啟動程序產生新 Session，不覆蓋舊歷史。儲存結構：

```text
<state-directory>/
  connections/<connection-id>/
    manifest.json
    sessions/<session-id>/
      events.jsonl          # 事件分段輪替；含序號與缺口紀錄
      checkpoint.json      # 已持久保存的位置與恢復狀態
    artifacts/
```

沒有前端觀看時仍記錄事件；採有界背景寫入，避免磁碟延遲卡住終端。超出留存或寫入佇列容量時明確顯示缺口，不宣稱完整歷史。執行中的原程序可重新取得控制 lease；程序已退出時只能從歷史建立新 Session。provider 原生對話續接需要各 adapter 支援並另行驗證，不能以終端文字重建程序記憶體。

任務的 checkpoint 必須區分已完成、尚未執行、結果不明。恢復時先查證結果不明的操作，尤其是 GitHub 回覆等外部寫入；不可直接重送。權限與原子認領仍由 Runtime 的 SQLite 管理，資料夾用於可讀歷史，不靠修改檔案取得權限。

工具：`history.list {endpoint_id}`、`history.read {endpoint_id,session_id,cursor?,limit?}`、`history.checkpoint {endpoint_id,session_id}`，均需 `space_id` 並重驗該端點的 observe 權限。`terminal.restart {endpoint_id,expected_session_id}` 需 manage 權限，原程序仍存活時拒絕；成功建立同端點的新 Session，不能沿用舊 lease。`process_resume:unsupported` 表示未恢復 provider 原生對話。

### GitHub 與持續同步

管理者以 `endpoints.register` 建立 `kind:github`，config 為 `{repository:"owner/name",allow_write_back:false}`。Session 維護角色不能自行核准 GitHub 存取；受核准端點的操作仍受獨立 grant 約束。設定 `allow_write_back:true` 是對該 repo 的明確寫入授權，不能由一般成員自設取得。

`sources.sync {endpoint_id}` 匯入 Issue/PR 到論壇；`sources.get {job_id}` 查詢背景工作。`sources.resume {endpoint_id,sync_job_id}` 接續本機已保存的匯入進度；`sources.writeback {endpoint_id,number,body}` 發送受授權留言；`sources.reconcile {endpoint_id,number,write_request_id}` 只讀遠端核對結果，不重送 POST。結果不明時須保留原 request_id。

`sources.watch.start {endpoint_id,interval_seconds}` 啟用持續同步，request_id 即 watch_id；間隔 30–86400 秒。`sources.watch.get/stop {watch_id}` 查詢或停止，`sources.watch.list {}` 列出本空間自身可讀的 watch；均需 space_id。同一 watch 不重疊執行。Runtime 重啟後會標示 unknown，需要重新登入並以相同 watch ID/config 明確重新啟用；登入 credential 不落盤。停止 watch 不代表已送出的 source job 立刻取消。

持續同步目前將新來源送到論壇；它不等於 Agent 已自主實作或審查。Source→Agent 的持續派工與交接仍須完成並獨立驗證。

owner-only `settings.get/set` 保存設定；`runtime.settings_effective` 說明這次啟動真正套用的值與限制。`history_limit` 是每個終端保留輸出的 bytes；啟動時套用。`runtime.discover` 只檢查 PATH 中明確的 CLI 名稱，候選不是已連線 Session，不掃全機程序或任意埠。外部 provider autojoin/reconnect 尚未完成。

owner-only `runtime.prepare_shutdown` 回影響與 60 秒 ticket；`runtime.confirm_shutdown` 檢查影響是否改變、停止接受新 mutation、嘗試停止 owned 程序。Pending 要重新取得影響並明確 `runtime.force_shutdown`。HTTP 斷線只能表示結果待確認，OS process handle 才能確認 daemon 已退出。

## 尚須分開驗證的能力

GitHub ingestion/writeback、單次模型拆分與持續排程各有不同證據；fake process 測試不證明真 GitHub 寫入或模型自主能力。真遠端回覆必須先對指定 repo 啟用明確授權。長時間閒置測試必須記錄實際 elapsed time，短測不能代替 65 分鐘。
