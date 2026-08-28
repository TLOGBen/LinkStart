# Daemon 發現、生命週期與版本切換

Type: prototype
Status: resolved
Blocked by: 01, 04, 05

## Question

每位使用者一個 LinkStart Daemon 的 discovery、start、health、idle、restart、crash 與 version-conflict 狀態機應如何運作；Event/Feedback journal 採何種 retention、delivery/rebind deadline 與 recovery；才能讓 `open-connect` 找到既有 Daemon、對不相容版本 fail closed，並只在明確操作後安全 restart 而不破壞其他 Agent Connection？

## Assets

- [Prototype：Daemon Lifecycle](../prototypes/07-daemon-lifecycle.html)

## Answer

- Daemon 採 on-demand start，不註冊 OS login auto-start。Discovery record 只作 locator；`open-connect` 必須再做 authenticated health/version/protocol handshake。
- 每位 OS 使用者最多一個 Daemon；同 exact Runtime version + protocol major 直接重用。Version 不同時 fail closed，active/reconnecting connections 存在時禁止自動 restart。
- 明確 upgrade 流程為 `request → drain（停止新 Receipt）→ journal commit → stop → start target → WAL recovery → same connectionId rebind`；protocol major 不相容時舊 connection 不可 rebind。
- Durable state 採 SQLite-style single journal、WAL、`synchronous=FULL`、receipt-first commit。Unix state dir `0700`／DB-WAL-lock `0600`；Windows current-user-only DACL。
- Defaults：delivery deadline 120 秒、rebind grace 30 秒、drain deadline 30 秒、Event/Feedback retention 7 天、零 live/reconnecting connection 且無 received Event 時 idle 15 分鐘自動停止。
- Crash 後保留 received Event 與未觀察 Feedback；重啟執行 WAL recovery並等待同 connectionId rebind。Grace 到期則 connection → offline、received Event → `failed(origin_offline)`，不 cold resume。
- `closed` connection terminal，capability revoke；Feedback 先 journal 後 SSE，App reconnect 以 cursor replay。
- 同一 Rust binary 同時提供 Daemon 與 deterministic control CLI：`--help`、`help [command]`、`help --json`、`version --json`、`status --json`、`ps --json`、`daemon start|stop|restart --json`、`connections list --json`、`apps list --json`、`doctor --json`。`ps` 是不含 secrets 的人類彙總；有副作用命令仍受 explicit drain/version/recovery gates 約束。

Prototype 已由真 browser 驅動 happy/version-conflict/crash scenarios，狀態在操作中改變且無 console/page errors。
