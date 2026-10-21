# Durable runtime core library

This PR adds `linkstart::runtime_core`, an in-process public Rust library. The existing daemon CLI/HTTP/GUI does **not** route v2 commands to it. No terminal, source adapter, model runner, scheduler, or connection-history integration is included.

## Public entry points

```rust
Engine::open(state_dir: &Path) -> anyhow::Result<Engine>
Engine::execute(&self, credential: &str, request: &ToolRequest) -> Result<Value, ApiError>
Engine::execute_requiring_role(&self, credential, request, role) -> Result<Value, ApiError>
Engine::execute_requiring_endpoint(&self, credential, request, endpoint_id, operation) -> Result<Value, ApiError>
Engine::is_owner(&self, credential: &str) -> bool
owner_token_path(state_dir: &Path) -> PathBuf
```

`Engine` is Clone. Handles share a mutex and SQLite connection; separate instances serialize mutations through SQLite transactions. `open_with_clock` takes an `Arc<dyn Fn() -> u64 + Send + Sync>` Unix-millisecond clock for deterministic connection lease boundaries.

Requests have `request_id: String`, `tool: String`, `space_id: Option<String>`, `args: serde_json::Value` (an object). Unknown request/argument fields are rejected. Errors carry serializable `code` and `message`; callers should match `code`. Never log credentials or whole bootstrap/invitation responses.

`spaces.create` is local-owner-only. It returns `{space, administrator, manager}`; administrator has a credential and verified human kind, manager is optional. Read the protected local `v2-owner.token` only inside the trusted host. Invitation registration uses the invite token as credential and `{name}` as args; it cannot choose roles or acquire `human_admin`. New credentials are returned to that authenticated caller and must be held by the host, not supplied to model prompts.

## Commands

All space commands require a current membership (or explicit local owner). Global settings require `space_id: null` and local owner. Lists return `{items, next_cursor}`; pass an opaque string `cursor` and `limit` 1..100. Current cursors are insertion offsets, not snapshot isolation across separate requests.

| Tools | Args / result |
|---|---|
| `spaces.list`, `spaces.create` | list; create `{name, administrator_name?, manager_name?}` |
| `identity.current` | `{}` -> authenticated identity; owner has `kind: owner` |
| `members.list` | list with current roles, visible endpoint grant sources and connection lease aggregation |
| `members.invite`, `members.register` | invite `{roles}` -> invite token; register `{name}` -> member credential |
| `members.set_roles` | `{member_id, roles}` -> member; management only, last human administrator/manager protected |
| `members.heartbeat` | first `{}` -> attachment; renewal `{attachment_id}` -> latest lease and `heartbeat_seconds` |
| `topics.create` | `{title, body, kind?, parent_topic_id?, dependencies?}` -> topic |
| `topics.list`, `topics.read` | list; read `{topic_id, cursor?, limit?}` -> `{topic, comments, next_cursor}` |
| `activity.read` | space event list with author, timestamps, revision/state/submission metadata |
| `task.act` | `{topic_id, action, comment, expected_revision, artifact_ref?, expected_submission?, reply_to?, artifact_refs?}` -> `{topic,event}` |
| `settings.get`, `settings.set` | get `{}`; set `{settings}` -> saved runtime defaults |
| `endpoints.register` | `{name,kind,config?}` -> metadata only; never spawns a process |
| `endpoints.list`, `endpoints.grant` | visible list; grant `{endpoint_id,member_id,operations}` (empty removes explicit grants) |
| `endpoint.authorize` | `{endpoint_id,operation}` -> `{identity_id,owner,endpoint,operation}`; no grant changes |

Roles: `human_admin`, `session_maintainer`, `manager`, `implementer`, `reviewer`, `consultant`, `tester`, `observer`, `task_decomposer`. Management roles have full space authority; session maintainers register endpoints but cannot approve GitHub repository access. Other endpoint operations require independent explicit grants: `observe`, `send`, `control`, `manage`. Member grant projections retain both role and explicit sources, filtered by endpoints visible to the reader.

`task_decomposer` can create child topics, not claim/assign/manage implicitly. Topic kinds: `task`, `plan`, `question`, `review`, `discussion`. Parent and dependencies must already exist in the same space. Creation-only links cannot form cycles. Task actions: `comment`, `claim`, `block`, `resume`, `submit_review`, `approve_review`, `request_changes`, `pass_test`, `fail_test`, `reclaim`. Submission requires `artifact_ref`; reviewing/testing requires `expected_submission` matching immutable `current_submission.id`. `expected_revision` protects concurrent changes. A task mutation, comment, event, and receipt commit atomically.

Restrictive entry points validate the current required role or endpoint authority in the **same transaction before receipt replay**. Use these when an adapter has more restrictive authority than general forum posting. GitHub endpoints require server-created top-level approval from registration by management; caller config cannot establish approval. Legacy unapproved GitHub endpoints are denied by both authorization and restrictive dispatch. A production source adapter must separately validate repository locators and perform bounded external operations; this library does not access GitHub.

## Persistence and boundaries

`v2.sqlite3` uses WAL and FULL synchronization. Credential hashes are persisted; bootstrap credentials are derived with a local protected owner secret and rehydrated only in server-owned receipt positions. Same identity/request/content returns the original mutation result after restart; changed content conflicts. Current permissions are rechecked before replay. Read commands do not update the forum snapshot. Heartbeat renewal updates only the latest attachment row and does not append permanent pulse receipts. First attachment registration is deduplicated.

Connection leases are independent of durable identities: multiple browser clients can heartbeat independently. `alive` / `suspect` / `disconnected` describe connection liveness only, never OS process death. Claims are preserved. Default TTL is 30 seconds; owner settings change the TTL. Other settings are saved defaults and are not proof that absent adapters apply them.

Storage ACLs protect the directory for the current OS account (Unix 0700/0600, Windows current-user `icacls`); this is not isolation from another process running as the same user. Caller endpoint config is arbitrary JSON: hosts must not put credentials in it, because endpoint data is visible to authorized members. The core is a local trusted-host component, not an adversarial multitenant service.

Scaling limit: forum mutations rewrite one JSON snapshot including durable dedupe receipts and history. Receipt/event retention is intentionally unbounded; heartbeat rows are one per attachment, without automatic expired-attachment garbage collection. Reads deserialize the snapshot. This foundation is not yet suitable for unbounded high-volume workloads. `history_limit` does not truncate durable forum receipts/events.

## Executable consumer

Use a dedicated directory; the example creates durable state and prints only topic data, never credentials:

```powershell
cargo run --locked --example runtime_core -- C:\Temp\linkstart-core-example
```

The example bootstraps a human administrator, creates a task, drops/reopens the engine, checks durable deduplication and reads the topic through the public API. It is a library consumer recipe, not daemon/HTTP/GUI integration evidence. Independent prove-it and review must assess this PR's exact commit separately.
