//! Resource handles compress auth/attachment/lease ordering; daemon remains final authority.
#![allow(dead_code, unused_variables)]
mod source;
mod types;
pub use types::*;
#[cfg(test)]
mod usage;
use std::{marker::PhantomData, sync::Arc};

// This is an authenticated daemon channel, never a public in-process state mutation path.
struct Channel {
    private: (),
}
struct Binding {
    channel: Arc<Channel>,
    identity: IdentityId,
    space: SpaceId,
    runtime_generation: u64,
    attachment_epoch: u64,
    secret: Vec<u8>,
}
pub struct Runtime {
    channel: Arc<Channel>,
}
pub struct Owner {
    channel: Arc<Channel>,
    owner_credential: Vec<u8>,
}
pub struct Space {
    binding: Arc<Binding>,
}
pub struct Topic {
    binding: Arc<Binding>,
    id: TopicId,
}
pub struct Terminal {
    binding: Arc<Binding>,
    session: SessionIdentity,
}
pub struct Provider {
    binding: Arc<Binding>,
    session: SessionIdentity,
}
pub struct TerminalControl {
    binding: Arc<Binding>,
    session: SessionIdentity,
    lease: Vec<u8>,
}
pub struct Subscription<T> {
    binding: Arc<Binding>,
    stream: Vec<u8>,
    item: PhantomData<T>,
}

impl Runtime {
    pub async fn connect_local() -> Result<Self> {
        unimplemented!()
    }
    /// Protected same-OS-user management handshake; ordinary join cannot produce Owner.
    pub async fn local_owner(&self) -> Result<Owner> {
        unimplemented!()
    }
    pub async fn register(
        &self,
        invitation: Invitation,
        name: String,
    ) -> Result<RegisteredIdentity> {
        unimplemented!()
    }
    /// Protected daemon vault lookup does not accept a caller's claim of verified credentials.
    pub async fn credential(&self, slot: String) -> Result<Credential> {
        unimplemented!()
    }
    pub async fn join(&self, credential: &Credential, space: SpaceId) -> Result<Space> {
        unimplemented!()
    }
    /// Registry + saved explicit sources + installed adapters only; probe before stale marking.
    pub async fn startup_discovery(&self) -> Result<Vec<CandidateView>> {
        unimplemented!()
    }
    /// Candidate data is untrusted; handshake validates instance/session/protocol/generation.
    /// A verified probe is not membership or control authority; attach rechecks it at use.
    pub async fn probe(&self, owner: &Owner, candidate: CandidateId) -> Result<VerifiedTarget> {
        unimplemented!()
    }
    pub async fn spaces(&self, credential: &Credential) -> Result<Vec<SpaceView>> {
        unimplemented!()
    }
}
impl Owner {
    pub async fn create_space(&self, request: RequestId, spec: SpaceSpec) -> Result<Bootstrap> {
        unimplemented!()
    }
    pub async fn recovery(&self, space: SpaceId, new_manager: IdentityId) -> Result<Credential> {
        unimplemented!()
    }
    pub async fn settings(&self) -> Result<Settings> {
        unimplemented!()
    }
    pub async fn preview_settings(&self, settings: Settings) -> Result<SettingsPlan> {
        unimplemented!()
    }
    pub async fn settings_impact(&self, plan: &SettingsPlan) -> Result<SettingsImpact> {
        unimplemented!()
    }
    pub async fn apply_settings(
        &self,
        request: RequestId,
        plan: SettingsPlan,
    ) -> Result<SettingsImpact> {
        unimplemented!()
    }
    pub async fn prepare_shutdown(&self) -> Result<ShutdownTicket> {
        unimplemented!()
    }
    /// Ticket binds owner, runtime generation, deadline and inventory revision.
    pub async fn confirm_shutdown(
        &self,
        request: RequestId,
        ticket: ShutdownTicket,
    ) -> Result<ShutdownJob> {
        unimplemented!()
    }
    pub async fn shutdown_progress(&self, job: &ShutdownJob) -> Result<ShutdownProgress> {
        unimplemented!()
    }
    pub async fn force_shutdown(
        &self,
        request: RequestId,
        job: &ShutdownJob,
    ) -> Result<ShutdownProgress> {
        unimplemented!()
    }
    /// Uses a protected, generation-specific OS witness outside the dying daemon.
    /// Disconnection, PID reuse, or accepted requests never produce ConfirmedExited.
    pub async fn observe_exit(&self, job: &ShutdownJob) -> Result<ExitObservation> {
        unimplemented!()
    }
}
impl Space {
    pub async fn endpoints(&self) -> Result<Endpoints> {
        unimplemented!()
    }
    pub async fn pool(&self) -> Result<Vec<PoolEntry>> {
        unimplemented!()
    }
    pub async fn members(&self) -> Result<Vec<MemberView>> {
        unimplemented!()
    }
    pub async fn heartbeat(&self) -> Result<AttachmentHealth> {
        unimplemented!()
    }
    /// Does not kill child; every operation on all descendants is fenced immediately.
    pub async fn release(&self) -> Result<()> {
        unimplemented!()
    }
    pub async fn invite(&self, request: RequestId, roles: Vec<Role>) -> Result<Invitation> {
        unimplemented!()
    }
    pub async fn set_roles(
        &self,
        request: RequestId,
        member: IdentityId,
        roles: Vec<Role>,
    ) -> Result<()> {
        unimplemented!()
    }
    pub async fn remove_member(&self, request: RequestId, member: IdentityId) -> Result<()> {
        unimplemented!()
    }
    pub async fn close(&self, request: RequestId) -> Result<()> {
        unimplemented!()
    }
    pub async fn set_presets(&self, request: RequestId, presets: Vec<Preset>) -> Result<()> {
        unimplemented!()
    }
    pub async fn grant_endpoint(
        &self,
        request: RequestId,
        member: IdentityId,
        endpoint: EndpointId,
        rights: EndpointRights,
    ) -> Result<()> {
        unimplemented!()
    }
    /// Launch + admission + registry commit; RequestId retry returns same endpoint or conflict.
    pub async fn create_terminal(
        &self,
        request: RequestId,
        spec: OwnedTerminalSpec,
    ) -> Result<Terminal> {
        unimplemented!()
    }
    pub async fn attach_terminal(
        &self,
        request: RequestId,
        target: ExternalTarget,
    ) -> Result<Terminal> {
        unimplemented!()
    }
    /// UI selects THIS space, then attaches a recently probed candidate. Runtime
    /// rechecks probe expiry, membership, ownership and actual source identity.
    pub async fn attach_candidate(
        &self,
        request: RequestId,
        target: VerifiedTarget,
    ) -> Result<EndpointView> {
        unimplemented!()
    }
    pub async fn connect_provider(
        &self,
        request: RequestId,
        spec: ProviderSpec,
    ) -> Result<Provider> {
        unimplemented!()
    }
    pub async fn terminal(&self, expected: SessionIdentity) -> Result<Terminal> {
        unimplemented!()
    }
    pub async fn provider(&self, expected: SessionIdentity) -> Result<Provider> {
        unimplemented!()
    }
    pub async fn create_topic(&self, request: RequestId, spec: TopicSpec) -> Result<Topic> {
        unimplemented!()
    }
    /// Lists all readable topics in this space; cursor is scoped and authorization rechecked.
    pub async fn topics(&self, after: Option<Cursor>, limit: usize) -> Result<TopicPage> {
        unimplemented!()
    }
    /// Cross-topic comment activity, scoped to this space; includes topic/message identifiers.
    pub async fn activity(&self, after: Option<Cursor>, limit: usize) -> Result<BoardPage> {
        unimplemented!()
    }
    pub async fn topic(&self, id: TopicId) -> Result<Topic> {
        unimplemented!()
    }
}
impl Topic {
    pub async fn snapshot(&self) -> Result<TopicView> {
        unimplemented!()
    }
    /// One transaction: reauthorize, dedupe space+author+request, compare revision,
    /// validate action/state/assignee, append comment+journal and mutate task, then notify.
    /// A retry with identical request/content returns the original receipt even after revision moves.
    /// Review/test must bind expected_submission; SubmitReview creates a fresh submission.
    /// Member role is rechecked even when they still own an assignment.
    /// Conflict/denial writes no comment and changes no state. No automatic CLI execution.
    pub async fn act(&self, request: RequestId, command: ForumCommand) -> Result<ForumReceipt> {
        unimplemented!()
    }

    /// Atomic message+journal+dedupe commit precedes notification. Scope: space+author+request.
    /// Hash includes topic/reply/body; mismatched contents return RequestConflict.
    pub async fn post(
        &self,
        request: RequestId,
        reply_to: Option<MessageId>,
        text: String,
    ) -> Result<BoardReceipt> {
        unimplemented!()
    }
    pub async fn read(&self, after: Option<Cursor>, limit: usize) -> Result<BoardPage> {
        unimplemented!()
    }
    pub async fn subscribe(&self, after: Option<Cursor>) -> Result<Subscription<BoardEvent>> {
        unimplemented!()
    }
    pub async fn acknowledge(&self, cursor: Cursor) -> Result<()> {
        unimplemented!()
    }
}
impl Terminal {
    pub fn id(&self) -> &EndpointId {
        unimplemented!()
    }
    pub async fn session(&self) -> Result<SessionIdentity> {
        unimplemented!()
    }
    pub async fn output(
        &self,
        after: Option<OutputCursor>,
    ) -> Result<Subscription<OutputFrame<TerminalEvent>>> {
        unimplemented!()
    }
    pub async fn screen(&self) -> Result<ScreenSnapshot> {
        unimplemented!()
    }
    pub async fn control(&self, choice: ControlChoice) -> Result<TerminalControl> {
        unimplemented!()
    }
    /// Detaches local participation; never kills an externally owned process.
    pub async fn disconnect(&self, request: RequestId) -> Result<()> {
        unimplemented!()
    }
    pub async fn stop_owned(
        &self,
        request: RequestId,
        deadline: std::time::Duration,
    ) -> Result<StopProgress> {
        unimplemented!()
    }
}
impl TerminalControl {
    /// No RequestId/replay API: a lost reply cannot establish whether bytes reached child.
    pub async fn write(&self, bytes: Vec<u8>) -> Result<RawWrite> {
        unimplemented!()
    }
    pub async fn resize(&self, columns: u16, rows: u16) -> Result<()> {
        unimplemented!()
    }
    pub async fn release(&self) -> Result<()> {
        unimplemented!()
    }
}
impl Provider {
    pub fn id(&self) -> &EndpointId {
        unimplemented!()
    }
    pub async fn session(&self) -> Result<SessionIdentity> {
        unimplemented!()
    }
    pub async fn capabilities(&self) -> Result<Capabilities> {
        unimplemented!()
    }
    pub async fn send(&self, request: RequestId, text: String) -> Result<DeliveryReceipt> {
        unimplemented!()
    }
    pub async fn delivery(&self, request: RequestId) -> Result<DeliveryReceipt> {
        unimplemented!()
    }
    pub async fn approve(
        &self,
        request: RequestId,
        approval_id: String,
        allow: bool,
    ) -> Result<DeliveryReceipt> {
        unimplemented!()
    }
    /// Caller obtains turn from a receipt or TurnStarted; cross-session turns are rejected.
    pub async fn cancel(&self, request: RequestId, turn: TurnId) -> Result<DeliveryReceipt> {
        unimplemented!()
    }
    pub async fn tool_result(
        &self,
        request: RequestId,
        call: String,
        result: String,
    ) -> Result<DeliveryReceipt> {
        unimplemented!()
    }
    pub async fn events(
        &self,
        after: Option<OutputCursor>,
    ) -> Result<Subscription<OutputFrame<StructuredEvent>>> {
        unimplemented!()
    }
    pub async fn disconnect(&self, request: RequestId) -> Result<()> {
        unimplemented!()
    }
}
impl<T> Subscription<T> {
    /// Bounded queue; re-check current policy+epoch before each returned item.
    /// Revocation linearizes with dispatch: no later delivery after revocation completes.
    pub async fn next(&mut self) -> Result<Option<T>> {
        unimplemented!()
    }
    pub async fn close(self) -> Result<()> {
        unimplemented!()
    }
}
