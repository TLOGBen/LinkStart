//! Domain values carry no authority. Secret-bearing values have no public constructors/Debug.
#[derive(Clone, Debug)]
pub struct SpaceId(pub String);
#[derive(Clone, Debug)]
pub struct IdentityId(pub String);
#[derive(Clone, Debug)]
pub struct EndpointId(pub String);
#[derive(Clone, Debug)]
pub struct TopicId(pub String);
#[derive(Clone, Debug)]
pub struct MessageId(pub String);
#[derive(Clone, Debug)]
pub struct RequestId(pub String);
#[derive(Clone, Debug)]
pub struct CandidateId(pub String);
#[derive(Clone, Debug)]
pub struct Cursor(pub String);
/// Non-secret locator; Runtime still checks it belongs to this exact provider session.
#[derive(Clone, Debug)]
pub struct TurnId(pub String);
pub struct Credential {
    pub(super) sealed: Vec<u8>,
}
pub struct Invitation {
    pub(super) sealed: Vec<u8>,
}
pub struct SettingsPlan {
    pub(super) sealed: Vec<u8>,
}
pub struct ShutdownTicket {
    pub(super) sealed: Vec<u8>,
    pub(super) impact: ShutdownImpact,
}
impl ShutdownTicket {
    pub fn impact(&self) -> &ShutdownImpact {
        unimplemented!()
    }
}
pub struct ShutdownJob {
    pub(super) sealed: Vec<u8>,
}
pub struct VerifiedTarget {
    pub(super) sealed: Vec<u8>,
}
pub struct OutputCursor {
    pub(super) sealed: Vec<u8>,
}
/// Authority/evidence can only be constructed by the protected boundary, never a public bool.
pub struct ExitEvidence {
    pub(super) runtime_generation: u64,
    pub(super) witness: Vec<u8>,
}
pub type Result<T> = std::result::Result<T, Error>;
#[derive(Debug)]
pub enum Error {
    Denied,
    StaleAttachment,
    StaleControl,
    SessionMismatch,
    Unsupported(String),
    LastManager, // last human administrator OR manager
    TopicConflict { current_revision: u64 },
    RequestConflict,
    ActiveDependencies(Vec<EndpointId>),
    InventoryChanged,
    TicketExpired,
    ShuttingDown,
    UnknownEffect(RequestId),
    Unavailable,
    PersistenceFailed,
    SlowConsumer,
    InvalidSettings(Vec<String>),
    DeliveryFailed { request: RequestId, reason: String },
    NoActiveTurn(RequestId),
    TurnNotObserved(RequestId),
}
pub enum Preset {
    FrontendLink,
    PersistentTerminal,
    AsyncCollaboration,
}
pub enum Role {
    Manager,
    HumanAdmin,
    SessionMaintainer,
    Implementer,
    Reviewer,
    Consultant,
    Tester,
    Observer,
}
pub struct EndpointRights {
    pub observe: bool,
    pub send: bool,
    pub control: bool,
}
/// Owner provisions the first manager identity; a display name never grants authority.
pub struct SpaceSpec {
    pub name: String,
    pub administrator_name: String,
    pub first_manager_name: Option<String>,
    pub presets: Vec<Preset>,
}
pub struct Bootstrap {
    pub space: SpaceId,
    pub administrator: RegisteredIdentity,
    pub manager: Option<RegisteredIdentity>,
}
pub struct RegisteredIdentity {
    pub identity: IdentityId,
    pub credential: Credential,
}
pub struct OwnedTerminalSpec {
    pub executable: String,
    pub args: Vec<String>,
    pub cwd: String,
}
pub struct ProviderSpec {
    pub adapter: String,
    pub locator: String,
    pub expected_native_session: String,
    pub credential_slot: String,
}
pub struct ExternalTarget {
    pub adapter: String,
    pub locator: String,
    pub expected_instance: String,
    pub expected_session: String,
    pub expected_generation: u64,
    pub credential_slot: String,
}
pub enum EndpointKind {
    OwnedTerminal,
    ExternalTerminal,
    StructuredProvider,
}
pub enum Liveness {
    Live,
    Suspect,
    Disconnected,
    Interrupted,
    Exited(i32),
}
pub enum AttachmentHealth {
    Alive,
    Suspect,
    Disconnected,
}
pub enum SessionIdentity {
    Terminal {
        endpoint: EndpointId,
        runtime_generation: u64,
        session_generation: u64,
    },
    Provider {
        endpoint: EndpointId,
        instance: String,
        native_session: String,
        session_generation: u64,
    },
}
pub struct Capabilities {
    pub tool_results: bool,
    pub approvals: bool,
    pub exact_cancel: bool,
}
pub struct EndpointView {
    pub id: EndpointId,
    pub kind: EndpointKind,
    pub session: SessionIdentity,
    pub capabilities: Capabilities,
    pub liveness: Liveness,
}
pub struct Endpoints {
    pub items: Vec<EndpointView>,
}
pub struct PoolEntry {
    pub identity: IdentityId,
    pub attachment_epoch: u64,
    pub health: AttachmentHealth,
}
pub struct SpaceView {
    pub id: SpaceId,
    pub name: String,
    pub presets: Vec<Preset>,
}
pub struct MemberView {
    pub identity: IdentityId,
    pub display_name: String,
    pub roles: Vec<Role>,
}
pub struct BoardMessage {
    pub recorded_at: String, // Runtime UTC timestamp, not caller-supplied
    pub action: ForumAction,
    pub revision_before: u64,
    pub revision_after: u64,
    pub state_before: Option<TaskState>,
    pub state_after: Option<TaskState>,
    pub assignee_after: Option<IdentityId>,
    pub submission: Option<Submission>,
    pub self_review: bool, // Runtime-derived; false does not prove independence

    pub space: SpaceId,
    pub topic: TopicId,
    pub message: MessageId,
    pub author: IdentityId,
    pub request: RequestId,
    pub reply_to: Option<MessageId>,
    pub text: String,
}
pub struct BoardReceipt {
    pub message: MessageId,
    pub cursor: Cursor,
    pub request: RequestId,
}
pub struct BoardPage {
    pub messages: Vec<BoardMessage>,
    pub next: Option<Cursor>,
}
pub enum BoardEvent {
    Committed(BoardMessage),
    Gap(Cursor),
}
pub struct OutputFrame<T> {
    pub cursor: OutputCursor,
    pub event: T,
}
pub struct ScreenSnapshot {
    pub through: OutputCursor,
    pub bytes: Vec<u8>,
}
pub enum TerminalEvent {
    Bytes(Vec<u8>),
    Gap { retained_from: OutputCursor },
    State(Liveness),
}
pub enum StructuredEvent {
    TurnStarted {
        request: RequestId,
        turn: TurnId,
    },
    Text {
        turn: TurnId,
        text: String,
    },
    ToolCall {
        turn: TurnId,
        call_id: String,
        name: String,
        arguments: String,
    },
    ToolResult {
        turn: TurnId,
        call_id: String,
        result: String,
    },
    ApprovalNeeded {
        turn: TurnId,
        approval_id: String,
        prompt: String,
    },
    TurnCompleted {
        request: RequestId,
        turn: TurnId,
    },
    Delivery {
        request: RequestId,
        state: DeliveryState,
    },
    Gap {
        retained_from: OutputCursor,
    },
}
pub enum DeliveryState {
    Accepted,
    Delivered,
    ModelStarted,
    ModelCompleted,
    Failed(String),
    Unknown,
}
/// Accepted may precede native turn creation; TurnStarted supplies the missing id later.
pub struct DeliveryReceipt {
    pub request: RequestId,
    pub state: DeliveryState,
    pub turn: Option<TurnId>,
}
pub enum RawWrite {
    Written { sequence: u64 },
    Unknown { sequence: u64 },
}
pub enum ControlChoice {
    Acquire,
    Takeover { reason: String },
}
pub enum CandidateState {
    Candidate,
    NeedsAuthentication,
    VerifiedAvailable,
    Stale,
    Disconnected,
    Exited,
}
pub struct CandidateView {
    pub id: CandidateId,
    pub label: String,
    pub nonsecret_locator: String,
    pub state: CandidateState,
}
pub struct AuthorizedSource {
    pub adapter: String,
    pub locator: String,
    pub credential_slot: String,
    pub autojoin: Autojoin,
}
/// Desired configuration, never proof of authority. Validation and each startup join
/// recheck the joining credential, identity and current membership in this exact space.
pub enum Autojoin {
    Disabled,
    Join {
        space: SpaceId,
        identity: IdentityId,
        identity_credential_slot: String,
    },
}
pub struct Settings {
    pub startup_discovery: bool,
    pub sources: Vec<AuthorizedSource>,
    pub heartbeat_ms: u64,
    pub reconnect_limit: u32,
    pub history_bytes: usize,
    pub subscriber_buffer: usize,
}
pub struct SettingsImpact {
    pub restart_required: bool,
    pub affected: Vec<EndpointId>,
    pub limits: Vec<String>,
}
pub struct ShutdownImpact {
    pub inventory_revision: u64,
    pub spaces: Vec<SpaceId>,
    pub owned_children: Vec<EndpointId>,
    pub external_disconnect_only: Vec<EndpointId>,
    pub expires_at_ms: u64,
}
pub enum ShutdownProgress {
    Pending {
        remaining: Vec<EndpointId>,
        durable_writes: usize,
    },
    Stopped(ExitEvidence),
    Failed {
        remaining: Vec<EndpointId>,
        reason: String,
    },
}
pub enum ExitObservation {
    StillRunning,
    Unconfirmed,
    ConfirmedExited(ExitEvidence),
}
pub enum StopProgress {
    Stopped,
    Pending { remaining: Vec<u32> },
    Failed { reason: String, remaining: Vec<u32> },
}

/// Forum contracts only. All operations must reauthorize at the daemon boundary.
pub enum TopicKind {
    Plan,
    Task,
    Question,
    Review,
}
pub enum TaskState {
    Unclaimed,
    InProgress,
    Blocked,
    InReview,
    InTest,
    Complete,
}
pub struct TopicSpec {
    pub title: String,
    pub body: String,
    pub kind: TopicKind,
}
pub struct TopicView {
    pub author: IdentityId,
    pub created_at: String,
    pub updated_at: String,
    pub current_submission: Option<Submission>,

    pub id: TopicId,
    pub space: SpaceId,
    pub title: String,
    pub body: String,
    pub kind: TopicKind,
    pub state: Option<TaskState>,
    pub assignee: Option<IdentityId>,
    pub revision: u64,
}
pub struct TopicPage {
    pub topics: Vec<TopicView>,
    pub next: Option<Cursor>,
}
pub enum ForumAction {
    Comment,
    Claim,
    Block,
    Resume,
    SubmitReview,
    ApproveReview,
    RequestChanges,
    PassTest,
    FailTest,
    Reclaim,
}
pub struct SubmissionId(pub String);
pub struct Submission {
    pub id: SubmissionId,
    pub artifact_ref: String, // Immutable external version locator; not proof of contents
    pub author: IdentityId,
    pub submitted_at: String,
}
pub struct ForumCommand {
    /// Required for SubmitReview; creates a new submission even if the locator repeats.
    pub artifact_ref: Option<String>,
    /// Required for review/test actions; must match current submission, plus revision check.
    pub expected_submission: Option<SubmissionId>,

    pub expected_revision: u64,
    pub action: ForumAction,
    pub reply_to: Option<MessageId>,
    pub text: String,
}
pub struct ForumReceipt {
    pub message: BoardReceipt,
    pub topic: TopicView,
}
