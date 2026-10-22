//! Read-only lookup of existing receipts. Never retries or reconciles an operation.
use crate::{
    runtime_core::{ApiError, Engine, ToolRequest},
    runtime_source::WriteState,
    runtime_source_jobs::SourceJobs,
};
use serde_json::{json, Value};

#[derive(Clone)]
pub struct OperationService {
    engine: Engine,
    source_jobs: SourceJobs,
}
fn error(code: &str, message: &str) -> ApiError {
    ApiError {
        code: code.into(),
        message: message.into(),
    }
}
fn text<'a>(args: &'a Value, key: &str) -> Result<&'a str, ApiError> {
    args[key]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| error("Invalid", &format!("{key} required")))
}
impl OperationService {
    pub fn new(engine: Engine, source_jobs: SourceJobs) -> Self {
        Self {
            engine,
            source_jobs,
        }
    }

    /// `source_kind` is explicit because core request IDs and source job IDs overlap.
    pub fn execute(&self, credential: &str, request: &ToolRequest) -> Result<Value, ApiError> {
        if request.tool != "operations.get" {
            return Err(error("Unsupported", "only operations.get is supported"));
        }
        if request.request_id.trim().is_empty() {
            return Err(error("Invalid", "request_id required"));
        }
        let space = request
            .space_id
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| error("Invalid", "space_id required"))?;
        let args = request
            .args
            .as_object()
            .ok_or_else(|| error("Invalid", "args must be an object"))?;
        if args.keys().any(|key| {
            !["endpoint_id", "source_kind", "operation_request_id"].contains(&key.as_str())
        }) {
            return Err(error("Invalid", "unsupported operations.get argument"));
        }
        let endpoint = text(&request.args, "endpoint_id")?;
        let operation = text(&request.args, "operation_request_id")?;
        let kind = request.args["source_kind"].as_str().ok_or_else(|| {
            error(
                "Unsupported",
                "explicit source_kind required; receipt stores may have overlapping IDs",
            )
        })?;
        let missing = || json!({"operation_request_id":operation,"source_kind":kind,"space_id":space,"endpoint_id":endpoint,"state":"unknown","lookup":"not_found","evidence":"no receipt found for this identity in the selected local store and scope; external effect is unknown"});
        match kind {
            "forum" => match self
                .engine
                .forum_operation_receipt(credential, space, endpoint, operation)?
            {
                Some(receipt) => Ok(
                    json!({"operation_request_id":operation,"source_kind":kind,"space_id":space,"endpoint_id":endpoint,"state":"confirmed","tool":receipt["tool"],"topic_id":receipt["topic_id"],"receipt":receipt["receipt"]}),
                ),
                None => Ok(missing()),
            },
            "source_job" => {
                // Authenticate the requested scope before even looking for a source job.
                // The callback does no IO and makes no recursive Engine call.
                self.engine.with_endpoint_authorization(
                    credential,
                    space,
                    endpoint,
                    "observe",
                    |_| Ok(()),
                )?;
                let job = match self.source_jobs.get(credential, operation) {
                    Ok(job) => job,
                    Err(e) if e.code == "NotFound" => return Ok(missing()),
                    Err(e) => return Err(e),
                };
                if job.space_id != space || job.endpoint_id != endpoint {
                    return Err(error(
                        "Denied",
                        "source job differs from requested resource scope",
                    ));
                }
                let state = match job.status.as_str() {
                    "queued" | "running" | "applying" => "pending",
                    "completed"
                        if job
                            .write_receipt
                            .as_ref()
                            .is_none_or(|r| r.state == WriteState::Confirmed) =>
                    {
                        "confirmed"
                    }
                    _ => "unknown",
                };
                Ok(
                    json!({"operation_request_id":operation,"source_kind":kind,"space_id":space,"endpoint_id":endpoint,"state":state,"receipt":job}),
                )
            }
            _ => Err(error(
                "Unsupported",
                "source_kind must be forum or source_job",
            )),
        }
    }
}
