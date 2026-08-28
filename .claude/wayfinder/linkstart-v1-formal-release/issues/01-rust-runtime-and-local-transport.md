# Rust Runtime 與本機 Transport 選型

Type: research
Status: resolved
Blocked by:

## Question

在「每位使用者一個 Daemon、skill-local binary 直接執行、Windows x64／Linux x64 musl／macOS universal」的限制下，Rust 是否仍是 LinkStart Runtime 的最佳選擇；若是，v1 應採哪組本機 transport、HTTP/SSE/WebSocket 或 IPC primitives，才能同時滿足 browser App、Claude adapter、Codex app-server adapter、單一 binary 與低維運成本？

## Assets

- [Research：Rust Runtime 與本機 Transport](../research/01-rust-runtime-and-local-transport.md)

## Answer

LinkStart v1 Runtime 採 Rust，公開面只使用 loopback TCP 的 HTTP/1.1：client → Daemon 採 JSON request/response，Daemon → client 採 SSE。v1 不加入 WebSocket、Unix socket 或 Windows named pipe；Go 只在 Rust 維護能力成為已證明 execution risk 時作 fallback。三平台 artifacts 必須逐 target 執行 smoke/linkage 驗證，macOS 兩個 slices 經 `lipo` 合成 universal。Browser capability auth、CORS/LNA、`file://` 與 SSE cursor 留給後續 protocol/App tickets。

Evidence：[Rust Runtime 與本機 Transport 選型研究](../research/01-rust-runtime-and-local-transport.md)。
