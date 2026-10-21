# GitHub source library

The public Rust modules are `runtime_source`, `runtime_source_jobs`, and `runtime_watch`.
These are in-process components; the daemon CLI, HTTP routes, GUI, history, terminal,
and model runner are not connected to them in this delivery.

## Host lifetime and authority

Open one `Engine` for a protected local state directory, then
`SourceJobs::open(&dir, engine.clone())` and
`WatchManager::open(&dir, engine.clone(), jobs.clone())` once per host lifetime.
Opening either service is a recovery boundary, not a status request. Use the same
protected directory or a subdirectory of it for source/watch ledgers. Never store
caller credentials in endpoint config or print bootstrap/invitation responses.
The host's `gh` executable and existing login provide GitHub authentication.

A management-authorized caller registers an `endpoints.register` endpoint with
`kind: "github"` and `config: {"repository":"TLOGBen/LinkStart",
"allow_write_back":false}`. Repository identity accepts only explicit `owner/name`
on github.com and compares ASCII case insensitively. Display config is preserved.
Endpoint approval, current space membership, current endpoint authority and normal
forum posting permission are independently checked. No service identity bypasses
forum permission. Ordinary `Engine::execute` args cannot supply source provenance.

## Source jobs

`SourceJobs::start(credential, &ToolRequest)` returns `SourceJobSnapshot` promptly;
poll `get(credential, job_id)` until status leaves `running`/`applying`.
Every request requires `space_id`; its stable `request_id` is the job ID.

| Tool | Exact args | Resulting operation |
| --- | --- | --- |
| `sources.sync` | `endpoint_id` | GitHub Issue/PR GET, durable checkpoint, local topics |
| `sources.resume` | `endpoint_id`, `sync_job_id` | Apply a complete saved fetch checkpoint without another GitHub call |
| `sources.writeback` | `endpoint_id`, `number`, `body` | Explicit comment POST only with current `allow_write_back: true` |
| `sources.reconcile` | `endpoint_id`, `number`, `write_request_id` | GET-only reconciliation of the caller's original write job |

Snapshots expose `job_id`, `space_id`, `endpoint_id`, `status`, `topic_ids`, optional
`write_receipt` and `error`. Same identity/request/payload replays the stored job;
changed payload conflicts. `get` rechecks caller identity and current observe or
manage authority. Worker dispatch and projection also recheck current authority.

Source identity uses GitHub global node IDs; PRs are excluded from issue listings
before separate PR ingestion. Stable topics retain the original body and source
provenance; changed revisions append comments/events. Structured provenance records
the approved endpoint/repository, source/version, original author and external trust.
Untrusted source content grants no authority. Acknowledged older checkpoints cannot
append the same old revision again. Interrupted running/applying jobs reopen as
`unknown` without relaunch. Resume needs a complete checkpoint; an incomplete fetch
requires a new explicit read-only sync.

Writeback commits an Unknown intent before POST. Keep the same stable request ID:
confirmed replay returns its receipt, while uncertain outcomes refuse another POST.
Reconciliation reads for an exact body/marker and validated target URL. Missing or
ambiguous matches remain Unknown; operators must decide what to do. This provides
durable uncertainty handling, not an exactly-once remote-effect guarantee. A timeout
or cancellation cannot undo a remote effect already accepted by GitHub.

`SourceAdapter::new(SourceConfig)` is the lower-level synchronous API, exposing
`ingest_repository`, `snapshots`, `post_comment`, and `reconcile_comment`. Its policy
allowlist defaults empty and writeback defaults disabled. It does not independently
enforce Engine membership; use SourceJobs for that boundary. Executable/prefix config
and `open_with_config_factory` are trusted host/test configuration, never client args.

## Explicit read-only watches

`WatchManager::start(credential, &ToolRequest)` accepts exact tool
`sources.watch.start`, watch ID in `request_id`, `space_id`, and exact args
`{endpoint_id, interval_seconds}` with interval 30..86400 seconds. It immediately
schedules a read-only sync, then waits after each completed round; rounds never
overlap. It never dispatches writeback, resume, or a model. One active watch per
caller/endpoint/space and a global 64-controller limit apply.

`get(credential, watch_id)` and `list(credential, space_id)` read persisted snapshots
without dispatch. `stop(credential, watch_id)` fences future admission and returns
`stopping` while accepted work drains, otherwise `stopped`. All methods use current
authority. Same active ID/config replays; a changed config conflicts. Failed or stopped
IDs retain their outcome; use a new explicit ID for a fresh watch.

On reopen, active/stopping watches become Unknown with no worker. Credentials stay
in memory. Explicit reauthentication and same-ID/config start rearms an Unknown watch
with a new round after checking the previous job is no longer active; it never
relaunches the old Unknown job. Stop Unknown to abandon it. Failure/Unknown stops
automatic rounds. Watch snapshots expose watch/space/endpoint IDs, status, interval,
round, last job ID/status and error, never credentials.

For shutdown call `watches.cancel_all()` before `jobs.cancel_all()` and wait until
**both** `active_count()` values reach zero. Accepted stop or cancellation is not
process-exit evidence. One manager/service owner per state directory is required;
multi-daemon fencing and arbitrary subprocess descendant containment are not supplied.

## Executable public consumer

```powershell
cargo +1.89.0 run --locked --example runtime_sources -- C:\Temp\linkstart-sources-example TLOGBen/LinkStart
```

Use a dedicated local directory and an authenticated `gh` on PATH. The example
registers an explicit read-only endpoint, syncs Issues/PRs, checks stable replay and
checkpoint resume, reads protected provenance, starts/reads/lists/stops a watch,
then drains both services. It prints counts/status only. It never sends a comment.
Run it again against the same directory to exercise durable request replay.

Unit tests use local PowerShell fake gh processes and SQLite; they cover revocation,
revision/recovery, Unknown write/reconciliation, protected provenance and watch
lifetime boundaries. These are distinct from live GitHub read evidence. Real remote
writes, rate-limit behavior, daemon/HTTP/GUI integration, model execution, continuous
Agent work and power-loss/multi-daemon recovery remain unverified here.
