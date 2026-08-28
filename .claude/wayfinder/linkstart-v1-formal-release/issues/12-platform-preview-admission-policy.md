# Platform Preview／Experimental Admission Policy

Type: grilling
Status: resolved
Blocked by: 02, 03

## Question

LinkStart v1 的「正式發布」是否允許依賴 Claude Channels／Monitor 與 Codex Windows app-server WebSocket 這些 preview／experimental surfaces；若允許，支援聲明、能力偵測、fail-closed fallback、最低版本與退出條件應如何寫，才能誠實發布而不把 preview MOP 說成穩定 MOE？

## Answer

允許發布版本化 artifacts，但產品名稱與支援聲明固定為 **「LinkStart v1 Preview：Stable core + Preview platform adapters」**；「正式」描述 release discipline，不代表 Claude/Codex adapters 已 GA 或 production-ready。

- Claude product floor 設 `2.1.105+`：Channel 標示 `Research Preview`，Monitor 標示 `Experimental compatibility`。Channel 未 opt-in/allowlist 時可降到通過 self-test 的 Monitor；兩者皆不成立則 `claude_adapter_unavailable`，不自動落 file-mailbox。
- Codex 初始只 allowlist 已實測 `0.149.1`，每個新版本須重跑 CLI help、generated schema 與完整 MOE 才加入；不得以 `>=` 自動放行 preview protocol。
- Codex Unix remote app-server 標 `Experimental Preview`；Windows loopback WebSocket 明標 `Experimental, not supported for production`。Standalone embedded TUI 標 `Unsupported origin mode`。
- Capability detection 必須以實際 initialize/handshake、必要 methods/events、live subscription 與 nonce round-trip 證明，不能只看 version 或「Connected」文字。
- 任一 schema drift、transport mismatch、unknown version、Origin process offline 或 live-thread join 失敗都 fail closed，不 cold resume、不猜 fallback。
- Adapter 升 Stable 的必要條件：上游移除 preview/experimental/production-unsupported 標籤；Claude custom Channel 有一般 marketplace admission；Codex app-server/remote transport 有 production contract；連續兩個受支援版本完成全平台 MOP/MOE，且 LinkStart 不再依賴 undocumented source behavior。

在上述 admission gate 尚未全綠前，不得使用 `Stable Claude integration`、`Stable Codex integration` 或 `Production-ready Windows Codex` 字樣。
