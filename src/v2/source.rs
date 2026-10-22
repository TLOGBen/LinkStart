//! Daemon-only seams: concrete provider variation, PTY ownership, durable journal.
//! Future server wiring calls ResourceOwner behind resource methods, never from v2 CLI Store.
use super::*;
use std::{future::Future, pin::Pin, time::Duration};
type Pull<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

// Only trusted adapter admission can create this value. Public locator fields are merely claims.
struct VerifiedSession {
    identity: SessionIdentity,
    capabilities: Capabilities,
    adapter_ticket: Vec<u8>,
}
struct AdapterCredentials {
    secret: Vec<u8>,
}
struct ProviderTurn {
    native_turn: TurnId,
    request: RequestId,
}
trait StructuredSource: Send {
    fn admit<'a>(
        &'a mut self,
        expected: &'a ProviderSpec,
        credential: &'a AdapterCredentials,
    ) -> Pull<'a, VerifiedSession> {
        unimplemented!()
    }
    fn send<'a>(
        &'a mut self,
        verified: &'a VerifiedSession,
        request: RequestId,
        text: String,
    ) -> Pull<'a, ProviderTurn> {
        unimplemented!()
    }
    fn next(&mut self) -> Pull<'_, Option<OutputFrame<StructuredEvent>>> {
        unimplemented!()
    }
    fn approve(&mut self, approval: String, allow: bool) -> Pull<'_, DeliveryState> {
        unimplemented!()
    }
    fn cancel(&mut self, turn: TurnId) -> Pull<'_, DeliveryState> {
        unimplemented!()
    }
    fn tool_result(&mut self, call: String, result: String) -> Pull<'_, DeliveryState> {
        unimplemented!()
    }
    fn disconnect(&mut self) -> Pull<'_, ()> {
        unimplemented!()
    }
}
// Real variation: Codex uses thread/resume + thread/read and start/steer(expectedTurnId).
struct CodexAppServer {
    private: (),
}
impl StructuredSource for CodexAppServer {}
// Claude SDK native session identity and approval/cancel capabilities remain provider-specific.
struct ClaudeSdk {
    private: (),
}
impl StructuredSource for ClaudeSdk {}

trait TerminalSource: Send {
    fn launch(&mut self, spec: OwnedTerminalSpec) -> Pull<'_, VerifiedSession> {
        unimplemented!()
    }
    /// Returns Unsupported without an explicit public control protocol and identity admission.
    fn attach(&mut self, spec: ExternalTarget) -> Pull<'_, VerifiedSession> {
        unimplemented!()
    }
    fn read(&mut self) -> Pull<'_, Option<OutputFrame<TerminalEvent>>> {
        unimplemented!()
    }
    fn write(&mut self, bytes: Vec<u8>) -> Pull<'_, RawWrite> {
        unimplemented!()
    }
    fn resize(&mut self, columns: u16, rows: u16) -> Pull<'_, ()> {
        unimplemented!()
    }
    fn stop_owned_tree(&mut self, deadline: Duration, force: bool) -> Pull<'_, Liveness> {
        unimplemented!()
    }
    fn disconnect_external(&mut self) -> Pull<'_, ()> {
        unimplemented!()
    }
}
struct WindowsConPty {
    private: (),
}
impl TerminalSource for WindowsConPty {}
struct UnixPty {
    private: (),
}
impl TerminalSource for UnixPty {}

// Single logical owner. No Clone/public fields; adapter tasks submit outcomes to this owner.
// Durable journal stores identity/membership/registry/settings; owner alone changes live leases.
struct ResourceOwner {
    journal: Journal,
    providers: Vec<Box<dyn StructuredSource>>,
    terminals: Vec<Box<dyn TerminalSource>>,
    attachments: Vec<AttachmentRecord>,
    controls: Vec<ControlRecord>,
    phase: RuntimePhase,
}
struct AttachmentRecord {
    space: SpaceId,
    identity: IdentityId,
    epoch: u64,
    expires_ms: u64,
}
struct ControlRecord {
    endpoint: EndpointId,
    attachment_epoch: u64,
    generation: u64,
}
enum RuntimePhase {
    Serving,
    Draining,
    AwaitingForce,
    ExitReady,
}
struct Journal {
    private: (),
}
struct AuthorizedRead {
    private: (),
}
struct AuthorizedWrite {
    private: (),
}
struct DurableCommit {
    receipt: BoardReceipt,
    journal_cursor: Cursor,
}
struct MutationFingerprint {
    request: RequestId,
    space: SpaceId,
    author: IdentityId,
    content_hash: Vec<u8>,
}
impl ResourceOwner {
    /// Checks runtime generation, epoch, membership, resource and operation policy together.
    fn read_scope(&mut self, binding: &Binding, resource: &str) -> Result<AuthorizedRead> {
        unimplemented!()
    }
    fn write_scope(&mut self, binding: &Binding, resource: &str) -> Result<AuthorizedWrite> {
        unimplemented!()
    }
    /// Policy mutation and each stream dispatch share the same serialization boundary.
    fn revoke(&mut self, binding: &Binding, member: IdentityId) -> Result<()> {
        unimplemented!()
    }
    fn fence_attachment(&mut self, binding: &Binding) -> Result<()> {
        unimplemented!()
    }
    fn check_writer(&mut self, binding: &Binding, lease: &[u8]) -> Result<AuthorizedWrite> {
        unimplemented!()
    }
    fn recover_after_restart(&mut self) -> Result<Vec<EndpointView>> {
        unimplemented!()
    }
    /// Probe only explicit records; never enumerate processes, arbitrary ports, or secrets.
    async fn probe_registry(&mut self) -> Result<Vec<CandidateView>> {
        unimplemented!()
    }
    /// Admission success alone does not grant membership or control.
    fn register_admitted(
        &mut self,
        request: RequestId,
        source: VerifiedSession,
    ) -> Result<EndpointId> {
        unimplemented!()
    }
    /// Persist launch intent before OS spawn; reconcile an interrupted spawn/registry
    /// commit by request id rather than launching a second child. Spawn is not SQL-atomic.
    async fn reconcile_launch(&mut self, request: RequestId) -> Result<EndpointView> {
        unimplemented!()
    }
    fn begin_drain(&mut self, ticket: &ShutdownTicket) -> Result<ShutdownJob> {
        unimplemented!()
    }
    async fn advance_shutdown(
        &mut self,
        job: &ShutdownJob,
        deadline: Duration,
    ) -> Result<ShutdownProgress> {
        unimplemented!()
    }
}
impl Journal {
    /// Real SQLite transaction: dedupe lookup/conflict + message + journal + receipt, then commit.
    /// Notification is only a wakeup; a crash after commit is repaired by cursor replay.
    fn commit_board(
        &mut self,
        authorization: AuthorizedWrite,
        fingerprint: MutationFingerprint,
        topic: TopicId,
        reply: Option<MessageId>,
        text: String,
    ) -> Result<DurableCommit> {
        unimplemented!()
    }
    fn page(
        &mut self,
        authorization: AuthorizedRead,
        topic: TopicId,
        cursor: Option<Cursor>,
        limit: usize,
    ) -> Result<BoardPage> {
        unimplemented!()
    }
    fn checkpoint(&mut self) -> Result<()> {
        unimplemented!()
    }
    fn record_pending_writes(&mut self) -> Result<usize> {
        unimplemented!()
    }
}

// Raw readers feed bounded per-subscriber buffers, never wait for a UI network reader.
// Gap carries replay cursor; terminal consumer fetches screen. Too-slow consumer disconnects.
struct BoundedOutbox<T> {
    capacity: usize,
    queued: Vec<T>,
}
impl<T> BoundedOutbox<T> {
    fn publish(&mut self, item: T) -> Result<()> {
        unimplemented!()
    }
    fn take(&mut self, read: AuthorizedRead) -> Result<Option<T>> {
        unimplemented!()
    }
}
