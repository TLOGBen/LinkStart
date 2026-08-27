# LinkStart

LinkStart 是 loopback-only 的本機 Runtime 與 Integration Plugin，讓 agent 產出的互動 HTML／localhost App 在原 turn 結束後，仍能把事件送回**產出它的同一條** Claude Code session 或 Codex thread，並在同一個 App 收到 Agent Feedback。它不使用 `claude -p`、`codex -p`、SDK subprocess 或替代 session。

目前產品標示為 **LinkStart v1 Preview：Stable core + Preview platform adapters**。Runtime／protocol 使用 Rust 與 SQLite durable journal；Claude／Codex Origin Session adapters 仍是 Preview。

## 安裝 Integration Plugin

### Claude Code

```text
/plugin marketplace add https://github.com/TLOGBen/LinkStart.git
/plugin install linkstart@linkstart
/plugin enable linkstart@linkstart
```

呼叫：

```text
/linkstart:link-start <manifest.json>
```

### Codex

```console
codex plugin marketplace add https://github.com/TLOGBen/LinkStart.git
codex plugin add linkstart@linkstart
```

呼叫：`$link-start <manifest.json>`。

## Runtime 與單一 Skill

```text
HTML / localhost App
  │  HTTP JSON：送出 App Event，取得 durable Event Receipt
  │  fetch-authenticated SSE：接收 Delivery Ack 與 Agent Feedback
  ▼
LinkStart daemon（127.0.0.1、每位 OS 使用者一個、SQLite/WAL）
  │
  ▼
同一條 Claude Code Origin Session / Codex Origin thread
```

Plugin 只公開一個 `link-start` skill。它在內部依序執行 Runtime 驗證與 daemon discovery、Origin attach/rebind、App register/launch、monitor wait/ack/feedback。模型先判斷目前 host，再只讀一份對應 reference：

- Claude Code：`references/claude-code.md`，Channel 是 canonical Research Preview；Monitor 是通過 live self-test 後的 compatibility path，採 background arm → wake → ack/feedback → turn 結束前 re-arm。
- Codex：`references/codex.md`，要求 LinkStart-owned app-server 與同一 Origin thread；採 bounded foreground wait，active turn 用 steer、idle thread 用 turn start，事件或 timeout 後 re-arm。

Standalone Codex TUI 不支援 hot takeover。未知 adapter/runtime/protocol、Origin offline 或 same-session 證據不足都 fail closed。

Event Receipt 只證明 Runtime 已 durable 收件；Delivery Ack 只證明 adapter 已接受；Agent Feedback 是回到 App 的獨立訊息。三者都不代表模型完成處理。

## 標準 Marketplace 佈局

```text
LinkStart/
├── .claude-plugin/marketplace.json       # Claude marketplace: linkstart
├── .agents/plugins/marketplace.json      # Codex Layout A（GitHub 安裝入口）
├── plugins/linkstart/                     # Claude canonical plugin
│   ├── .claude-plugin/plugin.json
│   └── skills/link-start/
│       ├── SKILL.md
│       ├── references/{claude-code,codex}.md
│       ├── scripts/
│       └── assets/                        # exact v0.1.2 Runtime artifacts
└── codex/
    ├── .agents/plugins/marketplace.json  # Codex Layout B
    └── plugins/linkstart/                 # generated Codex package
```

`plugins/linkstart` 是 Claude source；`codex/plugins/linkstart` 只由 `codex-skill-transfer` 生成，不手改。

## Runtime CLI

```console
linkstart --help
linkstart help --json
linkstart version --json
linkstart status --json
linkstart ps --json
linkstart doctor --json
linkstart daemon start --json
linkstart daemon stop --json
linkstart daemon restart --json
linkstart connections list --json
linkstart apps list --json
```

Adapter 使用的低階命令：

```console
linkstart monitor wait --connection-id <id> --capability <token> --json
linkstart monitor ack --connection-id <id> --capability <token> --event-id <id> --json
linkstart feedback send --connection-id <id> --capability <token> \
  --app-instance-id <id> --feedback-id <id> \
  --payload '{"message":"已收到"}' --json
```

一般使用者應透過 `link-start` workflow；不要手工複製 capability 當成正式操作方式。

## HTML PoC

Repo 內提供 [`examples/self-contained.html`](examples/self-contained.html) 與 vanilla [`web/linkstart.js`](web/linkstart.js)。最短流程：

1. 安裝 plugin，在產出頁面的 Origin Session 呼叫 `link-start` 並提供 App Manifest v1。
2. Skill 驗證並啟動／重用 bundled Runtime，attach 當前 Origin Session，註冊 App Instance，再開啟 HTML。
3. 頁面以 `fetch + Authorization` 提交互動並取得 Event Receipt。
4. 同一 Origin Session 接受事件後送 Delivery Ack，再把 Feedback 寫回 Runtime。
5. 頁面透過 authenticated SSE 收到 Feedback 並更新 DOM。

Self-contained HTML 會在 launch registration 時被 Runtime snapshot，並由精確註冊的 `http://127.0.0.1:<port>/v1/launch-pages/<pageId>` 供應；一次性 grant 只放在 URL fragment，不進 query、cookie 或 HTTP request。這避免 Windows default-browser launch 丟失 `file://` fragment，頁面兌換後立即從 address bar 清除 fragment。localhost App 的 exact-origin 流程維持不變。

App Manifest 只描述 `protocolMajor`、App identity/version、entry、exact Origin policy、requested capabilities 與 structured inputs；不包含 bearer、`connectionId`、tool approval 或 permission relay。

`linkstart help --json` 的 `operations` 欄位提供 attach、register、launch、monitor、ack、feedback 的 endpoint／command、必填欄位或 options、回應 identity 與安全註記；它只提供 shape，不輸出任何實際 secret。

## v0.1.2 Release Assets

[GitHub Release v0.1.2](https://github.com/TLOGBen/LinkStart/releases/tag/v0.1.2) 由原生 Windows、Linux、macOS jobs 建置並組裝：

```text
skills/link-start/assets/
├── checksums.json
└── bin/
    ├── linux-x64-musl/linkstart
    ├── windows-x64/linkstart.exe
    └── macos-universal/linkstart
```

Skill 依**執行環境**選 target，並在執行前驗 Runtime `0.1.2`、protocol `v1`、source tag/commit/workflow provenance、SHA-256、size 與 Unix executable mode。它不首次下載、不從 `PATH` 猜測，也不拿本機重編 binary 頂替。

## 安全邊界

- Daemon 只 bind `127.0.0.1`；不服務 LAN 或 Internet。
- App Instance 使用 256-bit bearer capability；`connectionId` 與 Callsign 都不是 authentication secret。
- Runtime 驗 exact `Host`／`Origin`，JSON 與 SSE 都用 `fetch + Authorization`；secret 不放 query string、cookie、log、畫面或版控。
- App input 永遠是不可信資料。即使內容是「同意」，也不能批准工具、擴大 permission/scope 或繞過 sandbox/approval gate。
- 同 `eventId`＋同 payload 是冪等重送；同 ID＋不同 payload 回 conflict。每個 App Instance 同時只投遞一個 in-flight Event。

## MOP 與 MOE

- **MOP**：Rust tests、三平台原生 build、static Linux musl、macOS universal、Windows x64、release manifest、SHA-256、plugin transfer/parity 與 marketplace install 都通過。
- **MOE**：真實 HTML Event 在 turn 結束後進入原本 Origin Session，session/thread identity 不變，且真實 Feedback 回到同一 App Instance。

CI、mock、`curl` roundtrip、build success 或 health response 只能證明 MOP／protocol behavior，不能替代真 Claude/Codex Origin Session MOE。

## 建置與驗證

```console
cargo fmt --check
cargo check --locked
cargo test --locked --all-targets
python3 -m unittest -v scripts/release/test_release.py
```

推送與 Cargo version 完全一致的 `v*` tag 才會進入 release job；任何 target、version、format、provenance 或 checksum gate 失敗都不發布。

## License

[MIT](LICENSE)
