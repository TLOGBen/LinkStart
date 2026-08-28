# Rust Runtime 與本機 Transport 選型研究

- 研究日期：2026-08-27（Asia/Taipei）
- 對應問題：`issues/01-rust-runtime-and-local-transport.md`
- 範圍：只判斷 Runtime 語言、三平台 artifact 可行性，以及 Daemon 對 browser／adapter 的本機 transport；不決定 capability token、完整 protocol state machine、daemon lifecycle 或各平台 Origin Session adapter 的內部協定。

## 建議結論

**v1 採 Rust，並只公開一套 loopback TCP transport：HTTP/1.1 + JSON request/response + SSE server push。不要在 v1 同時加入 WebSocket、Unix domain socket 或 Windows named pipe。**

建議的責任切分如下：

1. Daemon 只綁定明確 loopback 位址，不監聽 LAN wildcard。
2. Browser App、Claude adapter、Codex app-server adapter 都使用同一組 versioned HTTP endpoints。
3. client → Daemon 的 command／event 使用短生命週期 HTTP request（以 JSON body 為主）；Daemon → client 的非同步更新使用 `text/event-stream`。
4. SSE wire protocol 固定，但 browser client 可依後續安全決策選 native `EventSource` 或帶 header 的 `fetch()` streaming；不要讓這個實作差異分裂 Daemon protocol。
5. v1 不啟用 axum 的 `ws` feature，也不維護 Windows named pipe 與 Unix socket 兩條 OS-specific code path。

這個選擇不是因為 Rust 是唯一可行語言。Go 是可信的第二選項，而且標準庫 HTTP 與 cross-build 更簡單；但在本題已鎖定的「精確 musl target、無外部 runtime、長駐且安全邊界敏感的單一 executable」權重下，Rust 的 target model、預設 static musl linkage、無 GC 的 ownership model，以及可用同一 HTTP stack 提供 JSON/SSE，整體較吻合。若團隊沒有 Rust 維護能力，Go 應視為合理 fallback，而非技術上不合格。

## 判斷基準

本研究以 map 已鎖定的限制作為 acceptance boundary：

- 每位 OS 使用者最多一個 Daemon，單機 loopback。
- skill-local binary 直接執行，不首次下載、不要求系統預裝語言 runtime。
- 發行 Windows x64、Linux x64 musl、macOS universal。
- 同一 Daemon 同時服務 browser App、Claude adapter、Codex app-server adapter。
- v1 優先降低 build／runtime／協定分支的維運成本。

不把微基準 benchmark 當作選型依據；目前沒有 LinkStart workload 可支持「Rust 一定比 Go 快／省記憶體」之類結論。

## 一、Rust 是否能交付既定 artifacts

### Target 與支援等級

| 發行物 | Rust 輸入 target | 目前官方支援狀態 | 決策含義 |
|---|---|---|---|
| Windows x64 | `x86_64-pc-windows-msvc` | Tier 1 with host tools；完整標準庫 | 用 Windows GitHub-hosted runner native build/test。 |
| Linux x64 musl | `x86_64-unknown-linux-musl` | Tier 2 with host tools；完整標準庫 | 官方保證建得出，不保證每次改動都跑過 target tests；release 必須執行產物 smoke test。 |
| macOS arm64 slice | `aarch64-apple-darwin` | Tier 1 | native test。 |
| macOS x86_64 slice | `x86_64-apple-darwin` | Tier 2 | 不能把 universal merge 成功當成 Intel slice 可執行；需 Intel runner native test，或至少在 Apple Silicon 以 Rosetta 加測。 |

Rust 官方把 Tier 1 描述為官方 binary releases 且每次改動自動 build/test；Tier 2 with host tools 則是「保證 build」，不保證產物一定經自動執行測試。[Rust platform support](https://doc.rust-lang.org/rustc/platform-support.html) 目前也明列上述四個 targets；Apple target 頁另列 arm64 為 Tier 1、x86_64 為 Tier 2，以及兩者最低 macOS 版本。[Rust Apple targets](https://doc.rust-lang.org/rustc/platform-support/apple-darwin.html)

因此「Rust 支援這三個發行物」成立，但不能推導為單次 cross-build 就足夠驗收。

### 單一 executable 與 native dependency 邊界

- Cargo 的 binary target 會編譯成可執行程式；`cargo build --release --target <triple> --bin <name>` 能為每個 target 產生最終 binary。[Cargo targets](https://doc.rust-lang.org/cargo/reference/cargo-targets.html) [cargo build](https://doc.rust-lang.org/cargo/commands/cargo-build.html)
- `x86_64-unknown-linux-musl` 的 C runtime 預設 static link；Rust Reference 也要求在成功 build 後檢查最終 binary 的實際 linkage。[Rust linkage](https://doc.rust-lang.org/reference/linkage.html)
- Windows MSVC 預設不等於完全 self-contained。若 release contract 要求不依賴動態 MSVC CRT，官方做法是以 `-C target-feature=+crt-static` 建置，並檢查最終 PE imports；不能只看 Cargo exit code。[Rust linkage](https://doc.rust-lang.org/reference/linkage.html)
- macOS 的「single binary」應解讀為一個可執行檔、不夾帶 LinkStart 自有 runtime；它仍正常依賴 macOS 系統 libraries，不能把 Linux static-musl 條件套到 Mach-O。

要守住這個邊界，Rust dependencies 應優先選 pure-Rust／system-library-only 路徑。此 transport 不需要 TLS，故不需要引入 OpenSSL；若未來增加 native C dependency，三平台「單一 binary」與 cross-link 條件必須重新驗證。

### macOS universal 不是 Rust target triple

Rust 分別產生 `aarch64-apple-darwin` 與 `x86_64-apple-darwin` Mach-O。Apple 的 universal binary 文件說明，來源需各編譯一次，再以 `lipo` 合併 architecture-specific binaries；`lipo -archs` 可檢查結果包含 `x86_64 arm64`。[Apple: Building a universal macOS binary](https://developer.apple.com/documentation/Apple-Silicon/building-a-universal-macos-binary)

建議 release flow：

1. 在 macOS runner 安裝兩個 Rust targets，並以相同 `Cargo.lock`、features、profile、`MACOSX_DEPLOYMENT_TARGET` 建置兩個 slices。
2. 各 slice 分別測試；Intel slice 不因 `lipo` 成功就免測。
3. `lipo -create <arm64> <x86_64> -output <universal>`。
4. `lipo -verify_arch arm64 x86_64 <universal>`，再對 universal executable 做啟動／health smoke test。

Rust 官方明確支援 Apple targets 間以 Clang cross-compile，但需要 Xcode／macOS SDK；這也是 merge job 必須留在 macOS runner 的原因。[Rust Apple targets](https://doc.rust-lang.org/rustc/platform-support/apple-darwin.html)

### GitHub Actions 可行性

GitHub 官方 runner 文件目前提供 x64 Windows、x64 Ubuntu、arm64 macOS 與 Intel macOS labels，足以 native build/test 四個 slices。[GitHub: Choosing the runner for a job](https://docs.github.com/en/actions/how-tos/write-workflows/choose-where-workflows-run/choose-the-runner-for-a-job)

對 v1 的最低要求不是先設計完整 release workflow，而是確認不存在平台阻塞：

- Windows job：native MSVC build + test + executable smoke test + PE dependency inspection。
- Linux job：`x86_64-unknown-linux-musl` release build + static-link inspection + 在 musl-compatible 環境執行 smoke test。
- macOS jobs：arm64 與 Intel 各 native test；macOS merge job 建 universal 並驗證兩個 slices。
- 使用明確 runner OS labels，不以會漂移的 `*-latest` 作長期 release contract；runner label 本身仍應在 release ticket 定期刷新。

Cargo 的 `rustup target add` 只安裝 target standard library，cross-linker／SDK 仍是額外需求；所以 native OS runner 比「一台 Ubuntu 交叉編全部平台」更低風險。[rustup cross-compilation](https://rust-lang.github.io/rustup/cross-compilation.html)

## 二、Runtime 語言選項

### Option R：Rust（建議）

建議組合：

- async/runtime：Tokio；只開實際需要的 `rt-multi-thread`、`macros`、`net`、`signal`、`sync`、`time` 等 features，不直接用 `full` 作 release baseline。
- HTTP routing：axum（建立在 Tokio、hyper、tower 上）。
- SSE：axum 內建 `response::sse::{Sse, Event, KeepAlive}`。
- JSON：Serde／`serde_json`。
- CORS：`tower-http` 的 `CorsLayer`，但只允許明確 origins/methods/headers，不採 `Any` 作產品預設。

axum 官方 crate docs 把自身定位為著重 ergonomics/modularity 的 HTTP routing/request-handling library，並直接支援 JSON、SSE、WebSocket（optional feature）與 tower middleware。[axum](https://docs.rs/axum/latest/axum/) [axum SSE](https://docs.rs/axum/latest/axum/response/sse/) [tower-http CORS](https://docs.rs/tower-http/latest/tower_http/cors/)

優點：

- 精確命中 musl target，Linux C runtime 預設 static。
- ownership 在不需要 GC 的情況下提供 memory-safety guarantees；適合長駐、處理不受信任 browser input 的 daemon。[The Rust Book: Ownership](https://doc.rust-lang.org/book/ch04-00-understanding-ownership.html)
- JSON、SSE、可選 WebSocket 都在同一 HTTP stack，未來真的需要升級時不必先更換 runtime。
- feature flags 可讓 v1 不編入 WebSocket／HTTP/2 等未採用面向。

代價：

- async Rust 與 ownership 的開發門檻高於 Go；這是主要維護風險。
- crate graph 比 Go `net/http` 標準庫路徑大；需 commit `Cargo.lock`、限制 features、做 dependency/license/security hygiene。
- musl 與 Intel macOS 是 Tier 2，必須把 runnable artifact test 當 release gate。

### Option G：Go（可信 fallback）

Go 官方列出 `windows/amd64`、`linux/amd64`、`darwin/amd64`、`darwin/arm64` 等有效 `GOOS/GOARCH` 組合；標準庫 `net/http` 直接提供 server 與 graceful shutdown primitives。[Go source installation / target combinations](https://go.dev/doc/install/source) [Go `net/http`](https://pkg.go.dev/net/http)

優點：

- cross-build 與 HTTP server 的標準庫路徑更短，團隊上手與 build script 通常更簡單。
- 關閉 cgo 且不引入 native dependencies 時，可避免 C toolchain／C library 成為 build 與執行相依；Go 官方 `cgo` 文件說明 `CGO_ENABLED=0` 會停用 cgo，cross-compiling 時預設也停用。[Go `cgo`](https://go.dev/cmd/cgo/)
- JSON/HTTP/SSE 皆容易實作；SSE 雖非獨立高階 API，但可用標準 `http.ResponseWriter` streaming。

代價：

- `linux/amd64` + `CGO_ENABLED=0` 是純 Go static executable，不是 Rust 那樣名稱與 ABI 都明確的 `x86_64-unknown-linux-musl` target；若 release contract 對「musl-linked」而非「可在 musl Linux 執行」有 byte-level 要求，需另行定義與驗證。
- Go standard toolchain 的 runtime 包含 garbage collector；這不是 v1 的功能阻塞，但 daemon 的 resource profile 要靠量測，不能假定與 Rust 等價。[Go GC guide](https://go.dev/doc/gc-guide)
- macOS universal 同樣需要分別編譯兩個 slices 再 `lipo`，並沒有免除雙架構測試。

### 語言判決

| 準則 | Rust | Go |
|---|---|---|
| 精確 Linux musl target | **直接符合** | 可在 musl Linux 執行，但「純 Go static」與「musl-linked」語義不同 |
| Windows/macOS artifact | 符合 | 符合 |
| macOS universal | 兩 slices + `lipo` | 兩 slices + `lipo` |
| 無系統語言 runtime | 符合 | 符合（Go runtime 內嵌於 executable） |
| 無 GC 長駐模型 | **符合** | 不符合，但不是已證明的實務問題 |
| HTTP 開發簡單度 | 中 | **高** |
| v1 綜合適配 | **建議** | fallback |

若後續 prototype 顯示 Rust 維護能力不足，切 Go 的判斷門檻應是「team execution risk」，而不是 transport 或平台能力；HTTP/JSON/SSE 的決策不需跟著改。

## 三、Transport 選項

### Browser 能共用的最低層是 loopback HTTP

一般 browser JavaScript 沒有任意 Unix socket 或 Windows named pipe API。Tokio 的官方 docs 也顯示 `UnixListener`/`UnixStream` 只在 Unix，而 `tokio::net::windows::named_pipe` 只在 Windows；採 IPC 就必須同時維護兩套 acceptor、權限、路徑、清理與測試，browser 前面仍要另放 HTTP bridge。[Tokio networking](https://docs.rs/tokio/latest/tokio/net/)

因此 IPC 沒有減少 v1 attack surface 或維運面，反而增加第二個 protocol entrance。它只在未來有明確的「非 browser adapter 必須依 OS user ACL 隔離」需求時值得重開評估。

### Option T1：HTTP request/response + SSE（建議）

資料流：

```text
Browser / Claude adapter / Codex adapter
  -- HTTP POST/DELETE + JSON --> LinkStart Daemon
  <-- HTTP status + JSON ------
  <-- GET text/event-stream ----  非同步 Daemon updates
```

適配原因：

- `fetch()` 可發出 JSON request；adapter process 也能用任何標準 HTTP client，三種 client 共用 contract。
- WHATWG `EventSource` 是 HTTP server push API，wire MIME type 為 `text/event-stream`，固定 UTF-8，內建斷線重連；server 可發 `id`，重連時 browser 會帶 `Last-Event-ID`。[HTML Standard: Server-sent events](https://html.spec.whatwg.org/multipage/server-sent-events.html)
- axum 已有 SSE response、event id、JSON data 與 keepalive primitives；不需自製 framing。[axum SSE](https://docs.rs/axum/latest/axum/response/sse/)
- browser → daemon 的方向已有普通 HTTP request，沒有為了「雙向」再採 WebSocket 的必要。
- HTTP status/code/body 可直接表達 version mismatch、`origin_offline`、conflict、retryability 等結果，不必在 WebSocket message envelope 裡重建 HTTP 已有語意。

限制與補償：

- SSE 只 server → client；client → server 使用 HTTP request，這是刻意的 asymmetric design。
- native `EventSource` constructor 只有 URL 與 `withCredentials`，不能任意加入 `Authorization` header；若 capability 設計要求 bearer header，browser client 應以 `fetch()` 讀取 SSE stream並自行管理 reconnect/cursor，或由安全 ticket 定義短效 stream ticket／cookie。**不可因而把長效 secret 放 query string 當成這張 ticket 的默認答案。** [HTML Standard: `EventSource` interface](https://html.spec.whatwg.org/multipage/server-sent-events.html)
- SSE 的 `id`／`Last-Event-ID` 是 transport cursor，不自動提供 durable queue、exactly-once 或 Delivery Ack；與 domain `eventId` 的映射留給 protocol state-machine ticket。
- keepalive 只維持 stream 活性；WHATWG authoring notes 建議以 comment line 對抗 idle timeout，axum 也提供可設定的 keepalive。不要把 keepalive 當 agent online proof。

### Option T2：WebSocket

WebSocket 標準提供 browser 與 server process 的雙向連線，axum 也能以 optional `ws` feature處理 upgrade。[WHATWG WebSockets](https://websockets.spec.whatwg.org/) [axum WebSocket](https://docs.rs/axum/latest/axum/extract/ws/)

但 v1 不建議採用：

- 目前訊息形狀沒有只能靠 full-duplex socket 解決的需求；POST + SSE 已覆蓋兩個方向。
- browser WebSocket constructor 只有 URL 與 subprotocols，不能任意設定 auth header；它並不自然解決 EventSource 的 auth 限制。[WHATWG WebSockets](https://websockets.spec.whatwg.org/)
- 需要自行定義 request correlation、application error、reconnect/resume、heartbeat、backpressure 與 graceful close；這些會和 protocol state machine互相纏繞。
- WebSocket handshake 仍帶 `Origin` 並整合 Fetch 行為，未繞過 local-network permission 類 browser policy。

重開 WebSocket 的可驗證條件應是：實測顯示單一 SSE + POST 無法滿足頻率／延遲／雙向 streaming／binary payload，且需求足以抵銷另一套 stateful transport 的成本。

### Option T3：Unix socket + Windows named pipe

不建議進 v1：

- browser 無法直接使用，HTTP listener 仍不可刪。
- Tokio 明確分成 Unix-only sockets 與 Windows-only named pipes，產生雙平台 code path。
- 若同一 client contract 還要跨 HTTP 與 IPC，需自行決定 framing、錯誤與 version negotiation 是否完全一致；測試矩陣增加而產品能力沒有增加。

未來只有在 capability ticket 證明 TCP loopback 無法達成必要的 per-user OS ACL，或大量 adapter traffic 實測需要 IPC，才應另開 decision ticket。

### Option T4：只用 polling／long polling

技術上可行，但不建議：需要反覆 request、timeout 與 cursor 管理；相較 SSE 沒有降低 browser policy 或 auth 問題，卻失去標準化 event stream、native reconnect 與低延遲 push。可保留短期 health/version endpoint 的普通 polling，但不作 App feedback 主通道。

## 四、Browser protocol 必須承認的限制

### CORS 不是 optional

App 與 Daemon 幾乎一定是 cross-origin（port 不同也算不同 origin）。Fetch Standard 要求 server 以 `Access-Control-Allow-Origin` 等 headers 明確分享 response；較複雜的 methods/headers 會先送 `OPTIONS` preflight。[Fetch Standard: CORS protocol](https://fetch.spec.whatwg.org/)

v1 transport contract 至少要規定：

- 支援 `OPTIONS`。
- 回應只反射已註冊／已允許的具體 `Origin`，並帶正確 `Vary: Origin`；不可產品預設 `*`。
- methods、request headers、credentials 採最小集合。
- 在 auth 前先驗 `Origin`，但不得把 `Origin` 當 capability 或使用者身份證明。
- `file://` 文件常呈現 opaque／`null` origin，無法靠該字串辨別是哪個本機檔案；因此「任意 file HTML 都可連」不應被當成免費相容性。HTML Standard 定義 opaque origin 序列化為 `null`。[HTML Standard: Origins](https://html.spec.whatwg.org/multipage/browsers.html#origin)

這支持後續 browser contract 優先讓 App 由明確 `localhost` origin 提供，或要求完成 registration 後才授予 capability；是否正式支援 `file://` 留給 App Manifest/browser ticket。

### Loopback 是 trustworthy，不等於不會出現 permission

Secure Contexts 將 `127.0.0.0/8`、`::1` 與符合規則的 `localhost` 視為 potentially trustworthy；所以 HTTPS page 連 `http://localhost` 具有標準上的 loopback 特例。[W3C Secure Contexts](https://w3c.github.io/webappsec-secure-contexts/)

但 browser policy 正在收緊。Chrome 的 Local Network Access 說明明確把 public → loopback request 納入 permission prompt，且 permission 只允許 secure context 請求；其規格 explainer 也把 public → loopback、local → loopback 視為 local network requests。[Chrome: Local Network Access](https://developer.chrome.com/blog/local-network-access) [WICG Local Network Access explainer](https://github.com/WICG/local-network-access/blob/main/explainer.md)

因此：

- 不能把「loopback」寫成「browser 永遠無提示、永遠可連」。
- HTTP/SSE 與 WebSocket 都可能受 LNA 整合影響，換 WebSocket 不是 escape hatch。
- browser MOP 必須在支援矩陣用真實 Chrome/Edge/Safari/Firefox 驗證 localhost App、HTTPS-hosted App、必要時的 `file://` App；`curl` 不算 browser proof。
- daemon 應提供可診斷的 health/version endpoint與明確錯誤，讓 browser client 分辨 daemon offline、CORS rejection、permission denial與 protocol mismatch。

### 位址與 port

- 只 bind loopback；避免 `0.0.0.0`／`[::]`。
- hostname contract 要一致。`localhost` 可能解析 IPv4 或 IPv6；若 daemon 只 bind `127.0.0.1`，client 不應任意改用 `localhost` 後假設兩者等價。
- 若要雙棧，明確建立 `127.0.0.1` 與 `[::1]` listeners並測試 port ownership／discovery；不要用 wildcard 模擬雙棧。
- port discovery、版本衝突與 daemon restart 屬 lifecycle/port tickets，本研究只要求所有 clients最後解析成同一 loopback HTTP base URL。

## 五、建議的 v1 transport contract 邊界

這不是 endpoint schema，而是後續 tickets 可以依賴的 transport 決策：

| 面向 | v1 決定 |
|---|---|
| Listener | loopback TCP only |
| Application protocol | HTTP/1.1 |
| Payload | UTF-8 JSON；health 可無 body |
| Client → Daemon | HTTP request/response |
| Daemon → client | SSE `text/event-stream` |
| Browser stream API | native `EventSource` 或 `fetch()` streaming，由 capability/auth contract 決定 |
| Versioning | URL 或 media contract 帶 protocol major `v1`；細節由 protocol ticket定義 |
| WebSocket | v1 不啟用 |
| Unix socket/named pipe | v1 不提供 |
| TLS | loopback v1 不自簽 TLS；信任依 capability + Origin/CORS，不把 plaintext loopback 誤稱網路加密 |

## 六、仍未知、需要後續 ticket／prototype 證明的事項

以下 unknowns 不阻擋本題作出 Rust + HTTP/SSE 決定，但必須顯式交棒：

1. **Capability transport**：native EventSource 無 custom header。最終採 cookie、短效 stream ticket或 `fetch()` stream，要由 loopback trust ticket 與 browser contract 共同決定。
2. **`file://` 支援**：不同 browser 的 origin／LNA 行為需要真實 browser prototype；若無法安全區分 `Origin: null`，v1 可明確不支援直接 file origin。
3. **SSE resume 語意**：是否有 bounded in-memory replay、cursor 如何與 domain `eventId` 分離，由 protocol state-machine ticket 決定；不得因 SSE 有 `Last-Event-ID` 就宣稱 durable delivery。
4. **macOS deployment target**：需由 release policy鎖定一致的 `MACOSX_DEPLOYMENT_TARGET` 並 native test兩個 slices。
5. **Windows CRT 自含程度**：需在 release artifact 上檢查 PE imports，確認 `+crt-static` 結果符合「skill-local single binary」。
6. **musl artifact 可執行性**：需在目標環境執行，而非只跑 `file`／linkage inspection。
7. **資源預算**：daemon idle RSS、每連線增量、SSE fan-out、shutdown latency 尚無 workload measurement；不得拿語言宣傳資料代替 MOP/MOE。
8. **LNA browser 漂移**：這是 living browser policy，release gate 應維持 Playwright／真 browser smoke matrix，而不是一次研究後永久假定。

## 建議 decision wording

> LinkStart v1 Runtime 採 Rust。Daemon 只提供 loopback TCP 的 HTTP/1.1 API：client-to-daemon 使用 JSON request/response，daemon-to-client 使用 SSE；browser、Claude adapter 與 Codex app-server adapter 共用同一 versioned transport contract。v1 不提供 WebSocket、Unix domain socket 或 Windows named pipe。Runtime 以 `x86_64-pc-windows-msvc`、`x86_64-unknown-linux-musl`、`aarch64-apple-darwin` 與 `x86_64-apple-darwin` 建置，macOS 兩 slices 經 `lipo` 合成 universal；每個 slice／artifact 都需在相符 OS/architecture 上執行 smoke test並檢查 linkage。Browser capability auth、CORS allowlist、LNA UX與 SSE resume 語意由後續 tickets 釘死。

## 一手來源索引

- Rust：[Platform Support](https://doc.rust-lang.org/rustc/platform-support.html)、[Windows MSVC targets](https://doc.rust-lang.org/rustc/platform-support/windows-msvc.html)、[Apple targets](https://doc.rust-lang.org/rustc/platform-support/apple-darwin.html)、[Linkage](https://doc.rust-lang.org/reference/linkage.html)、[rustup cross-compilation](https://rust-lang.github.io/rustup/cross-compilation.html)、[Cargo targets](https://doc.rust-lang.org/cargo/reference/cargo-targets.html)、[cargo build](https://doc.rust-lang.org/cargo/commands/cargo-build.html)、[Ownership](https://doc.rust-lang.org/book/ch04-00-understanding-ownership.html)
- Rust crates：[axum](https://docs.rs/axum/latest/axum/)、[axum SSE](https://docs.rs/axum/latest/axum/response/sse/)、[axum WebSocket](https://docs.rs/axum/latest/axum/extract/ws/)、[Tokio networking](https://docs.rs/tokio/latest/tokio/net/)、[tower-http CORS](https://docs.rs/tower-http/latest/tower_http/cors/)
- Web standards：[WHATWG Server-sent events](https://html.spec.whatwg.org/multipage/server-sent-events.html)、[WHATWG WebSockets](https://websockets.spec.whatwg.org/)、[WHATWG Fetch/CORS](https://fetch.spec.whatwg.org/)、[HTML Origins](https://html.spec.whatwg.org/multipage/browsers.html#origin)、[W3C Secure Contexts](https://w3c.github.io/webappsec-secure-contexts/)、[WICG Local Network Access](https://github.com/WICG/local-network-access/blob/main/explainer.md)
- Build platforms：[GitHub-hosted runners](https://docs.github.com/en/actions/how-tos/write-workflows/choose-where-workflows-run/choose-the-runner-for-a-job)、[Apple universal macOS binary](https://developer.apple.com/documentation/Apple-Silicon/building-a-universal-macos-binary)
- Go fallback：[Supported target combinations](https://go.dev/doc/install/source)、[`net/http`](https://pkg.go.dev/net/http)、[`cgo`](https://go.dev/cmd/cgo/)、[GC guide](https://go.dev/doc/gc-guide)
