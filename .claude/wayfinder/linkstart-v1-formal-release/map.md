## Destination

正式發布 LinkStart v1 與 common-dev 的 `linkstart` Integration Plugin：本機 HTML／localhost App 能在原 turn 結束後，透過每位使用者一個的 LinkStart Daemon，持續把互動送回同一條 Claude Code Origin Session 或 Codex Origin Session，並在同一 App 收到回饋。

## Notes

- Canonical project：`https://github.com/TLOGBen/LinkStart`；Runtime、protocol、browser client 與 release artifacts 的 source of truth 在此 repo。
- Sibling integration repo：`/home/vakarve/projects/common-dev-plugin`；只承載薄的 `linkstart` Integration Plugin，Claude source canonical、Codex output generated。
- Wayfinder 只解決決策；route 清楚後才交棒實作與正式發布，不在 tickets 裡偷做 destination。
- 最後一張 decision ticket 關閉時自動評估 `$strategic-advance` admission；只有 `OBJECTIVE_LOCKED + MULTI_TRANSITION_CAMPAIGN` 且同時出現 execution stall、live-state risk 或 session saturation 才開 campaign，否則用普通 contract-banded execution。
- 預設繁體中文；grilling tickets 使用 `$grilling` + `$domain-modeling`，research tickets 使用 `$research` subagent，prototype tickets使用 `$prototype`。
- 使用者已授權後續 HITL decisions 採 operator 的 recommended default，不再逐題停問；只有沒有安全預設、會改變 Destination 或需要新權限時才暫停。
- 第一版：單機、單 OS 使用者、loopback；每位使用者最多一個 Daemon；Origin Session 的 agent process 離線時回 `origin_offline`，不排隊、不建立新 session。
- App Instance 只綁一個 Agent Connection；App Event 在 instance 內有序，以 `eventId` 去重；Delivery Ack 只代表 adapter 接受。
- Public skills：`open-connect`、`link-in`、`register-app`；Callsign 只供顯示，routing 使用 opaque `connectionId`。
- App Manifest 只描述 App；Claude/Codex plugin manifest 沿用各平台標準。
- LinkStart Runtime artifacts 由 GitHub Actions 建置：Windows x64、Linux x64 musl、macOS universal；第一版 unsigned + SHA-256。
- 平台 binary 直接放在 `open-connect/assets/bin/<target>/` 並由 skill-local launcher 執行，不採首次下載或系統預裝。
- Runtime 已決定採 Rust；protocol 使用 major version（先 `v1`），Runtime 使用 SemVer，Plugin 內嵌確切 Runtime tag。
- 不同 protocol/runtime 的既有 Daemon 必須 fail closed，經明確操作才 restart；不得自動中斷其他 session。
- LinkStart release workflow 自動建立 common-dev 同步 PR，不得直接 push common-dev main。
- License：MIT。
- 驗收必須分開 MOP（mechanism 有執行）與 MOE（同一 Origin Session 與同一 App 真正完成往返）。

## Decisions so far

- [Rust Runtime 與本機 Transport 選型](issues/01-rust-runtime-and-local-transport.md) — v1 採 Rust + loopback HTTP/1.1 JSON/SSE；不加入 WebSocket 或 OS-specific IPC。
- [Claude Origin Session Adapter 契約](issues/02-claude-origin-session-adapter.md) — Channel 為 canonical、Monitor + MCP reply 為 current-session compatibility，file/background 只作 diagnostic。
- [Codex Origin Session Adapter 契約](issues/03-codex-origin-session-adapter.md) — Origin thread 必須由同一 LinkStart-owned app-server 從開始持有；standalone TUI 不可 hot takeover。
- [Protocol 狀態機與傳遞語意](issues/04-protocol-state-machine-and-delivery.md) — durable Receipt 與 Delivery Ack 分離；instance 單一 in-flight、有序去重，公開狀態不假裝模型 processing-complete。
- [Loopback Trust 與 Capability 邊界](issues/05-loopback-trust-and-capability-boundary.md) — 每 App Instance bearer capability + exact Origin/Host gate；App input 永不具 approval／擴權語意。
- [Platform Preview／Experimental Admission Policy](issues/12-platform-preview-admission-policy.md) — 發布 `LinkStart v1 Preview`，stable core 與 preview adapters 分標，unknown capability/version 一律 fail closed。
- [App Manifest 與 Browser Client 契約](issues/06-app-manifest-and-browser-client.md) — v1 支援 self-contained HTML／localhost App，使用 vanilla fetch-authenticated client，明確顯示 Receipt/Ack/Feedback 而不暴露 routing secret。
- [Daemon 發現、生命週期與版本切換](issues/07-daemon-lifecycle-and-version-switch.md) — on-demand 單一 Daemon、SQLite/WAL durable journal、版本衝突 fail closed、明確 drain/restart/rebind。
- [三個公開 Skill 的責任與 UX](issues/08-public-skill-responsibilities.md) — open-connect 編排全流程，link-in 只接 Origin Session，register-app 只建立／啟動 App；Runtime protocol 是唯一 delivery authority。
- [Skill-local Binary 與雙平台 Port 契約](issues/09-skill-local-binary-and-port-contract.md) — 唯一 binary copy 在 open-connect assets；direct execution 前驗 exact version/SHA，Codex 只由 transfer 生成並做 byte/mode parity gate。
- [GitHub Actions Release 與跨 Repo 同步](issues/10-release-and-cross-repo-sync.md) — LinkStart tag 建三平台 canonical assets/SBOM/checksums；最小權限 workflow 只開 common-dev 同步 PR，經 transfer/install/parity gates 後人工合併。
- [MOP／MOE 跨平台驗收矩陣](issues/11-mop-moe-release-gate.md) — Windows/Linux/macOS × Claude/Codex 六格分開驗 MOP/MOE；真 Origin Session evidence 不得由 build、mock或別的平台替代。

## Not yet specified


## Out of scope

- 第一版不支援區網、遠端、多使用者或跨裝置連線。
- 第一版不支援 Origin Session process 離線期間的 durable queue，也不自動建立替代 session。
- 第一版不支援一個 App Instance fan-out 多個 Agent Connection。
- 第一版不發布 Windows ARM64 或 Linux ARM64 artifacts。
- 第一版不以 macOS notarization 或 Windows code signing 作 release gate；只發布 unsigned artifacts 與 SHA-256。
- 第一版不提供 React、Vue、Svelte 等 framework-specific browser bindings；只發布 vanilla client。
- LinkStart workflow 不得直接 push common-dev main。
