# LinkStart

LinkStart 是讓互動式 App 與產出它的 AI coding-agent 對話持續交換訊息的本機連線領域；Claude Code 與 Codex 共用領域語言，但各自保留平台 adapter。

## Language

**Origin Session（原始 Session）**:
產出互動式 App、且後續 App 事件必須返回的同一條 Claude Code session 或 Codex thread。
_Avoid_: 新 Agent Run、相似 Session、目前 Agent

**LinkStart Runtime**:
擁有 LinkStart 通訊協議、連線狀態與 App 整合契約的獨立產品邊界。
_Avoid_: common-dev Plugin、Agent Skill

**LinkStart Daemon**:
每位作業系統使用者最多一個的本機常駐 LinkStart Runtime 實例，可同時服務多個專案、Origin Session 與 App。
_Avoid_: 每專案 Server、每 Session Hub

**Integration Plugin（整合 Plugin）**:
由 common-dev 發布、將 Claude Code 與 Codex 接入 LinkStart Runtime 的薄整合層；不擁有通訊協議本身。
_Avoid_: LinkStart Runtime、Protocol Server

**Agent Connection（Agent 連線）**:
Origin Session 與 LinkStart Daemon 之間可持續或恢復的連線關係，由不透明的 `connectionId` 唯一識別。
_Avoid_: Callsign、Agent Process、New Session

**Callsign（呼號）**:
LinkStart Daemon 為 Agent Connection 配發、供人辨識的短代號；不具 routing、授權或身份驗證語義。
_Avoid_: Connection ID、Token、帳號

**App Manifest**:
App 對 LinkStart 宣告自身識別與 protocol-visible capabilities 的持久描述；Claude/Codex 的 plugin packaging manifest 不屬於此概念。
_Avoid_: Plugin Manifest、Agent Manifest、Runtime Config

**App Instance**:
一次實際開啟並連上 LinkStart 的 App 執行個體；第一版只綁定一個 Agent Connection。
_Avoid_: App Definition、Browser Tab、Multi-agent Room

**App Registration**:
LinkStart Daemon 依 App Manifest 建立、將一個 App Instance 綁到一個 Agent Connection 的註冊關係。
_Avoid_: Plugin Install、Agent Link、Multi-session Broadcast

**App Event**:
App Instance 傳給 Origin Session 的有序互動訊息，以 `eventId` 識別並對重送去重。
_Avoid_: Tool Approval、Permission Grant、Exactly-once Message

**Event Receipt（事件收據）**:
LinkStart Daemon 已驗證並持久記錄 App Event、且為它配置 instance-local sequence 的證明；不代表 Agent adapter 已接受。
_Avoid_: Delivery Ack、Processing Complete、Agent Reply

**In-flight App Event**:
一個 App Instance 中目前唯一正在等待 Agent adapter 接受的 App Event；其他已收件事件依 sequence 等候。
_Avoid_: Parallel Delivery、Latest State、Agent Turn

**Delivery Ack**:
Agent adapter 接受 App Event 後由 LinkStart 回給 App Instance 的投遞確認；它不代表模型已完成處理或使用者已核准工具行為。
_Avoid_: Agent Reply、Tool Approval、Processing Complete

**Agent Feedback**:
Origin Session 傳回 App Instance 的訊息，以 `feedbackId` 識別，並可選擇用 `inReplyToEventId` 關聯某個 App Event。
_Avoid_: Delivery Ack、Event Completion、Tool Result
