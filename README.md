# LinkStart

讓互動式 HTML／localhost App 在原 turn 結束後，仍能把事件送回產出它的同一條 Claude Code session 或 Codex thread，並收到 Agent Feedback。

[![Release](https://img.shields.io/github/v/release/TLOGBen/LinkStart?label=release)](https://github.com/TLOGBen/LinkStart/releases/latest)
[![Build](https://github.com/TLOGBen/LinkStart/actions/workflows/release.yml/badge.svg)](https://github.com/TLOGBen/LinkStart/actions/workflows/release.yml)
[![License](https://img.shields.io/github/license/TLOGBen/LinkStart)](LICENSE)

## 安裝

### CLI release binaries

[v0.1.3 Release](https://github.com/TLOGBen/LinkStart/releases/tag/v0.1.3) 提供一個已組裝並附 provenance checksums 的套件：

```console
curl -LO https://github.com/TLOGBen/LinkStart/releases/download/v0.1.3/linkstart-v0.1.3-plugin-assets.zip
curl -LO https://github.com/TLOGBen/LinkStart/releases/download/v0.1.3/checksums.json
sha256sum checksums.json
unzip linkstart-v0.1.3-plugin-assets.zip
```

從 `assets/bin/` 選擇執行環境對應檔案：

| 平台 | 路徑 |
|---|---|
| Linux／WSL x64 | `linux-x64-musl/linkstart` |
| Windows x64 | `windows-x64/linkstart.exe` |
| macOS Intel／Apple Silicon | `macos-universal/linkstart` |

執行前請依 `assets/checksums.json` 驗證大小與 SHA-256。

### Claude Code plugin

```text
/plugin marketplace add https://github.com/TLOGBen/LinkStart.git
/plugin install linkstart@linkstart
/plugin enable linkstart@linkstart
```

### Codex plugin

```console
codex plugin marketplace add https://github.com/TLOGBen/LinkStart.git
codex plugin add linkstart@linkstart
```

Repo 使用單一 shared package：`plugins/linkstart/` 同時含 `.claude-plugin/plugin.json`、`.codex-plugin/plugin.json` 與一份 `skills/link-start/**`；沒有重複的 Codex skill/assets tree。

## Quick start

1. 準備 App Manifest v1 與 HTML／localhost App。
2. 在產出 App 的 Origin Session 呼叫 Claude `/linkstart:link-start <manifest.json>` 或 Codex `$link-start <manifest.json>`。
3. Skill 驗證 bundled Runtime `0.1.3`，啟動／重用 daemon，attach 當前 session，註冊並開啟 App。
4. App 送出互動後，`arm` 讓同一 Origin Session 收到 Event；模型只需一次 `respond --payload ...`，wrapper 會自動 Ack、Feedback 並進入下一次 wait。

CLI smoke：

```console
linkstart version --json
linkstart daemon start --json
linkstart status --json
```

## 運作方式

```text
HTML / localhost App
  │  HTTP JSON：App Event → durable Event Receipt
  │  authenticated SSE：Delivery Ack / Agent Feedback
  ▼
LinkStart daemon（127.0.0.1、每位 OS 使用者一個、SQLite/WAL）
  ▼
原本的 Claude Code session / Codex thread
```

Event Receipt 只證明 Runtime 已 durable 收件；Delivery Ack 只證明 adapter 已接受；Agent Feedback 是回到 App 的獨立訊息。三者都不代表模型完成處理。

SSE 依 durable notification cursor 持續讀取 SQLite，因此由另一個 CLI process 寫入的 Delivery Ack／Feedback 會直接出現在仍在線的 App；斷線後也從同一 cursor journal replay。

## CLI

| 命令 | 用途 |
|---|---|
| `linkstart help --json` | Runtime 版本、security 與 attach/register/launch/monitor/ack/feedback schema |
| `linkstart version --json` | Runtime SemVer 與 protocol major |
| `linkstart daemon … --json` | `start`／`stop`／`restart` lifecycle |
| `linkstart status --json` | Runtime 狀態 |
| `linkstart ps --json` | 不含 secret 的 connection/App 摘要 |
| `linkstart doctor --json` | state dir 與 SQLite/WAL 診斷 |
| `linkstart connections list --json` | Agent Connections |
| `linkstart apps list --json` | App Instances |
| `runtime.py context create` | 將 state dir、connection ID、capability 寫入 private `0600` context |
| `runtime.py arm` | 從 context 注入身份並進入 bounded monitor wait |
| `runtime.py respond` | 一次完成 pending Event Ack、stable-ID Feedback 與下一次 wait |
| `runtime.py close` | 刪除本機 ephemeral capability context |

`runtime.py` 位於 plugin 的 `skills/link-start/scripts/`。它不把 capability 印到 stdout；`close` 只刪除本機 secret，不冒稱已做 Runtime-side revoke。

## `$link-start` 使用方式

Claude Code：

```text
/linkstart:link-start ./app-manifest.json
```

Codex：

```text
$link-start ./app-manifest.json
```

Skill 會先辨識 host，再只讀一份對應 reference：

- `references/claude-code.md`：Channel canonical path；Monitor compatibility 使用 attached background `arm`／`respond-and-wait`，completion 回到同一 session。
- `references/codex.md`：LinkStart-owned app-server／same-thread boundary；使用相同 wrapper 做 bounded foreground wait。

兩份 reference 都以 Runtime `help --json` 為 schema authority，包含 attach、register、launch、context、arm、respond 的 exact request／command 與輸出欄位；不需要讀 Rust source，也不使用 `claude -p`、`codex -p`、SDK subprocess 或替代 session。

## 支援平台與 Preview 限制

| Surface | 狀態 |
|---|---|
| Rust Runtime／protocol v1 | Stable core |
| Claude Channel | Research Preview |
| Claude Monitor compatibility | Experimental compatibility |
| Codex Unix app-server／remote TUI | Experimental Preview |
| Codex Windows loopback WebSocket | Experimental；不宣稱 production-ready |
| Standalone Codex TUI hot takeover | 不支援 |

**Status：**build、protocol 與 marketplace 安裝已可機械驗證；真實 Origin Session 往返仍必須依上述 host-specific Preview 路徑各自驗證，不能用 mock 或另一平台結果代替。

## 安全

- Daemon 只 bind `127.0.0.1`，不服務 LAN／Internet。
- App Instance 使用 256-bit bearer capability；`connectionId` 與 Callsign 都不是 authentication secret。
- Runtime 驗 exact `Host`／`Origin`；JSON 與 SSE 使用 `fetch + Authorization`，secret 不放 query、cookie、log、畫面或版控。
- Session context 位於 private `0700` state dir、檔案 mode `0600`；identity mismatch 一律 fail closed。
- App input 永遠是不可信資料；「同意」不能批准工具、擴大 permission/scope 或繞過 sandbox/approval gate。
- 同 `eventId`＋同 payload 是冪等重送；同 ID＋不同 payload 回 conflict；每個 App Instance 同時只投遞一個 in-flight Event。

## 建置與發布

```console
cargo fmt --check
cargo check --locked
cargo test --locked --all-targets
python3 -m unittest -v scripts/release/test_release.py
```

`.github/workflows/release.yml` 在原生 Windows／Linux／macOS runners 建置；Linux 驗 static musl，macOS 組 universal binary，assembly 重算 manifest SHA-256。只有與 Cargo version 完全一致的 `v*` tag 且所有 gates 通過才建立 Release。

## License

[MIT](LICENSE)
