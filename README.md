# LinkStart

LinkStart 是一個只監聽 loopback 的本機訊息 Runtime。它讓 agent 產出的 HTML／本機 app 可以把使用者互動送回**原本那個** Claude Code 或 Codex 工作階段，再由該工作階段把回饋送回頁面；過程不需要另外執行 `claude -p` 或 `codex -p` 建立新 session。

目前版本是 **LinkStart v1 Preview**：Rust Runtime 與持久化協定是 stable core；Claude／Codex Origin Session adapter 仍是 Preview。平台 adapter 的安裝與 session orchestration 屬 `linkstart` Integration Plugin 層，不在 Rust core 裡。

## 它怎麼運作

```text
HTML / local app
  │  HTTP JSON：送出互動，先取得 durable Event Receipt
  │  SSE：接收 agent feedback
  ▼
LinkStart daemon（127.0.0.1、每位 OS 使用者一個、SQLite）
  │
  ├─ Agent Connection：Origin Session 的 adapter 監看事件並 Delivery Ack
  └─ App Instance：每次註冊都有獨立 capability 與 exact-origin policy
        │
        ▼
原本的 Claude Code / Codex session
```

頁面事件採 receipt-first：Runtime durable 寫入後回傳 `received`，adapter 真正接走後才變成 `delivered`。同一個 `eventId` 搭配相同 payload 是冪等重送；相同 ID 搭配不同 payload 會衝突。這不宣稱模型處理的 exactly-once，也不把「訊息已投遞」冒充「模型已完成回覆」。

## Preview 支援邊界

- **Claude Code**：目標路徑是雙向 Channel（Research Preview）。plugin monitor + MCP reply tool 是實驗性相容路徑。
- **Codex**：Origin thread 必須從一開始由同一個 LinkStart-owned app-server 擁有；active turn 使用 steer，idle thread 使用 turn start，TUI 以 remote 模式接入。Unix app-server adapter 是 Experimental Preview；Windows WebSocket 路徑仍屬實驗性，不宣稱 production-ready。
- LinkStart 不會熱接管一個已經獨立啟動、未由 LinkStart 擁有的 Codex TUI session。
- 未知 adapter 版本或 protocol major 一律 fail closed，不做猜測式相容。

## CLI

人類可讀說明：

```console
linkstart --help
linkstart help
linkstart help daemon
```

機器可讀說明與身分：

```console
linkstart help --json
linkstart version --json
```

Runtime 操作與診斷：

```console
linkstart daemon start --json
linkstart daemon stop --json
linkstart daemon restart --json
linkstart status --json
linkstart ps --json
linkstart doctor --json
linkstart connections list --json
linkstart apps list --json
```

Adapter 會使用以下低階命令；一般使用者應優先透過 Integration Plugin 的 `open-connect`、`link-in` 與 `register-app` skills：

```console
linkstart monitor wait --connection-id <id> --capability <token> --json
linkstart monitor ack --connection-id <id> --capability <token> --event-id <id> --json
linkstart feedback send --connection-id <id> --capability <token> \
  --app-instance-id <id> --feedback-id <id> \
  --payload '{"message":"已收到"}' --json
```

Capability 是秘密。不要把它放進 query string、cookie、CLI log、畫面截圖或版本控制；`help/status/ps` 也不會輸出 capability。

## 最小 PoC 流程

1. Integration Plugin 執行 `open-connect`，啟動或重用 daemon，並把**當前 Origin Session** 連成一個 Agent Connection。
2. `register-app` 以 manifest 註冊 HTML app，取得只屬於該 App Instance 的 launch grant／capability。
3. HTML 載入 `web/linkstart.js`（或使用 `examples/self-contained.html` 的同等邏輯），以 `fetch + Authorization: Bearer …` 送出互動。
4. `link-in` 的 adapter 在同一 Origin Session 收到事件、交付給 agent，並送出 Delivery Ack。
5. Agent feedback durable 寫回 Runtime，頁面透過 SSE 收到並更新 UI。

直接啟動 Runtime 做協定層測試：

```console
cargo run -- daemon start --json --state-dir /tmp/linkstart-poc
cargo run -- status --json --state-dir /tmp/linkstart-poc
```

完整 Origin Session PoC 應由 Integration Plugin 建立連線與 app registration；不要手工複製 capability 當成正式操作方式。

## 本機安全模型

「只跑 localhost」不等於瀏覽器沒有來源邊界。若頁面由 daemon 同源供應，確實不需要 CORS；但 LinkStart 也支援 self-contained HTML 與其他 localhost dev server，它們對 `127.0.0.1` Runtime 仍是 cross-origin request。因此 Runtime 使用的是窄化的瀏覽器閘門，而不是公網式開放 CORS：

- 只 bind `127.0.0.1`，不對 LAN 或 Internet 監聽。
- App Instance 使用 256-bit bearer capability；`connectionId` 只是 locator，不是授權。
- 逐次比對 `Host` 與 manifest 的 exact `Origin`，只對吻合來源回傳 CORS／Private Network Access headers。
- JSON 與 SSE 都走 `fetch` 並帶 `Authorization`；不把秘密塞進 URL，也不使用無法帶 header 的原生 `EventSource`。
- 頁面輸入只是資料，不能替 agent 核准權限、擴大 scope 或繞過既有 approval gate。
- 狀態目錄應維持 OS 使用者私有權限；daemon discovery 與資料庫不應進版控。

## MOP 與 MOE

請分開看兩層證據：

- **MOP（工作有完成）**：Rust tests 通過、三平台 binary 在原生 runner 建置、`version --json` 正確、Linux 是 static musl、macOS 同時含 x86_64/arm64、release manifest 與 SHA-256 驗證通過。
- **MOE（目標真的達成）**：真實 HTML 互動進入原本 Claude/Codex Origin Session，該 session 實際收到內容並把 feedback 回到同一頁面。

CI、mock、`curl` round trip 與假的 app-server 都只能證明 MOP 或協定層效果，不能替代真實 Claude/Codex Origin Session MOE。

## 建置與測試

本機原生建置：

```console
cargo fmt --check
cargo check --locked
cargo test --locked --all-targets
cargo build --locked --release
```

Release validator 測試：

```console
python3 -m unittest -v scripts/release/test_release.py
```

`.github/workflows/release.yml` 在 pull request 與手動觸發時建立並上傳三個 target bundle。只有工作流程執行於精確的 `v0.1.0` tag 時，才會再產生以下符合 Integration Plugin consumer contract 的組裝產物：

```text
assets/
├── bin/
│   ├── linux-x64-musl/linkstart
│   ├── windows-x64/linkstart.exe
│   └── macos-universal/linkstart
└── checksums.json
```

本機 assembly 必須取得三個真實 build bundle；`scripts/release/release.py` 缺任何 target 都會失敗，絕不製造假的跨平台 binary。

## 發布與安裝

推送與 `Cargo.toml` 完全一致的 tag（本版為 `v0.1.0`）後，Actions 才會進入 release job。它會在三個原生 build job 與 assembly validation 全通過後建立 GitHub Release，發布：

- `linkstart-v0.1.0-plugin-assets.zip`：保留上述 Integration Plugin 注入路徑。
- `checksums.json`：以 exact-key schema 記錄 Runtime semver、protocol major、release tag，以及每個 target 的相對路徑、大小、SHA-256、source repository/commit/tag 與 workflow run provenance。

Tag/version 不符、target 缺漏或重複、Linux 非 static、macOS 非 universal、Windows 非 x64 PE、`version --json` 不符，或任何 checksum 不符時，release 會拒絕發布。

終端使用者建議從 `common-dev` marketplace 安裝 `linkstart` Integration Plugin；它會挑選目前平台的 binary，先核對 `checksums.json`、版本與 protocol，再直接執行。從 GitHub Release 手動安裝時，也應先驗證 manifest，不要只下載裸 binary。

## 授權

[MIT](LICENSE)
