//! Readers for records written by older versions of Orbit.
//!
//! `bitcode` is not self-describing: a struct that gains a field, or an enum
//! whose variant count crosses a packing boundary, no longer decodes from old
//! bytes. The durable logs therefore carry a schema version (see
//! `orbit-core`'s store), and each version that changed a logged type keeps a
//! frozen copy of the old layout here with a conversion to the current one.
//!
//! Rules for changing logged types (`Event`, `Thread`, `ThreadConfig`,
//! `Project`, `Item` and everything inside them):
//!
//! * Appending an enum variant is compatible while the variant count stays
//!   within its packing class (2, 3, 4, 6, 16, 256 variants). `ItemBody` and
//!   `Event` have room; crossing a boundary needs a new version.
//! * Adding, removing or reordering a struct field always needs a new
//!   schema version and a frozen copy of the old struct here, and of
//!   everything that contains it (an `Item` field freezes `Item`,
//!   `ItemBody`, `Event`…). The schema version covers thread timeline logs
//!   too (their records are `Item`s); older versions' copies refer to the
//!   newest frozen copy of what they contain.

use bitcode::{Decode, Encode};

use crate::codec::FrameError;
use crate::ids::*;
use crate::model::*;
use crate::wire::*;

/// Version 1: logs without a version header, written before provider
/// instances. Its `Event` is the layout with `SettingsChanged`; logs from
/// before settings existed decode the same, except for records that were
/// never meant to be logged (see [`decode_envelope`]).
pub mod v1 {
    use super::*;
    use crate::settings::{
        AdvancedSettings, ComposerSettings, EnvVar, GeneralSettings, NotificationSettings, WorkspaceSettings,
    };

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
    pub enum ProviderKind {
        Claude,
        Codex,
    }

    #[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
    pub struct ThreadConfig {
        pub provider: ProviderKind,
        pub model: String,
        pub options: Vec<OptionValue>,
        pub mode: Mode,
        pub access: Access,
    }

    #[derive(Clone, Debug, PartialEq, Encode, Decode)]
    pub struct Thread {
        pub id: ThreadId,
        pub project: Option<ProjectId>,
        pub title: String,
        pub config: ThreadConfig,
        pub workspace: Workspace,
        pub status: ThreadStatus,
        pub created_ms: u64,
        pub updated_ms: u64,
        pub session: Option<String>,
        pub parent: Option<ThreadId>,
        pub archived: bool,
        pub pinned: bool,
        pub unread: bool,
        pub usage: Usage,
        pub turns: u32,
        pub queued: u32,
        pub pending: Vec<v3::PendingRequest>,
        pub settled_ms: Option<u64>,
        pub snoozed_until: Option<u64>,
        pub woke: bool,
    }

    #[derive(Clone, Debug, PartialEq, Encode, Decode)]
    pub struct ProviderInfo {
        pub kind: ProviderKind,
        pub installed: bool,
        pub version: Option<String>,
        pub signed_in: Option<bool>,
        pub account: Option<String>,
        pub binary: Option<String>,
        pub models: Vec<ModelInfo>,
        pub modes: Vec<ModeInfo>,
        pub slash_commands: Vec<SlashCommand>,
        pub message: Option<String>,
        pub quota: Vec<QuotaWindow>,
    }

    #[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
    pub struct ProviderSettings {
        pub kind: ProviderKind,
        pub binary: String,
        pub env: Vec<EnvVar>,
    }

    #[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
    pub struct AgentSettings {
        pub default_access: Access,
        pub providers: Vec<ProviderSettings>,
    }

    #[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
    pub struct Settings {
        pub general: GeneralSettings,
        pub appearance: v3::AppearanceSettings,
        pub composer: ComposerSettings,
        pub agents: AgentSettings,
        pub workspace: WorkspaceSettings,
        pub review: v3::ReviewSettings,
        pub notifications: NotificationSettings,
        pub advanced: AdvancedSettings,
    }

    /// `Event` as version 1 wrote it. Variants that never reach the log are
    /// kept so variant indices and layout match.
    #[derive(Clone, Debug, PartialEq, Encode, Decode)]
    pub enum Event {
        ProjectUpserted(Project),
        ProjectRemoved(ProjectId),
        ThreadUpserted(Thread),
        ThreadRemoved(ThreadId),
        ItemUpserted { thread: ThreadId, item: v3::Item },
        ItemDelta { thread: ThreadId, item: ItemId, delta: Delta },
        ItemRemoved { thread: ThreadId, item: ItemId },
        Providers(Vec<ProviderInfo>),
        DefaultsChanged(ThreadConfig),
        SettingsChanged(Settings),
        WorkspaceChanged { thread: ThreadId, files: Vec<FileChange> },
        UsageChanged { thread: ThreadId, usage: Usage },
        Conflicts(Vec<Conflict>),
        TerminalOutput { term: TermId, data: Vec<u8> },
        TimelineMark { thread: ThreadId, mark: u64 },
        TerminalExited { term: TermId, code: Option<i32> },
    }

    #[derive(Clone, Debug, PartialEq, Encode, Decode)]
    pub struct Envelope {
        pub seq: u64,
        pub event: Event,
    }

    impl From<ProviderKind> for crate::model::ProviderKind {
        fn from(kind: ProviderKind) -> Self {
            match kind {
                ProviderKind::Claude => crate::model::ProviderKind::Claude,
                ProviderKind::Codex => crate::model::ProviderKind::Codex,
            }
        }
    }

    impl From<ThreadConfig> for crate::model::ThreadConfig {
        fn from(c: ThreadConfig) -> Self {
            crate::model::ThreadConfig {
                provider: c.provider.into(),
                model: c.model,
                options: c.options,
                mode: c.mode,
                access: c.access,
                instance: String::new(),
            }
        }
    }

    impl From<Thread> for crate::model::Thread {
        fn from(t: Thread) -> Self {
            crate::model::Thread {
                id: t.id,
                project: t.project,
                title: t.title,
                config: t.config.into(),
                workspace: t.workspace,
                status: t.status,
                created_ms: t.created_ms,
                updated_ms: t.updated_ms,
                session: t.session,
                parent: t.parent,
                archived: t.archived,
                pinned: t.pinned,
                unread: t.unread,
                usage: t.usage,
                turns: t.turns,
                queued: t.queued,
                pending: t.pending.into_iter().map(Into::into).collect(),
                settled_ms: t.settled_ms,
                snoozed_until: t.snoozed_until,
                woke: t.woke,
            }
        }
    }

    impl From<ProviderInfo> for crate::model::ProviderInfo {
        fn from(p: ProviderInfo) -> Self {
            crate::model::ProviderInfo {
                kind: p.kind.into(),
                installed: p.installed,
                version: p.version,
                signed_in: p.signed_in,
                account: p.account,
                binary: p.binary,
                models: p.models,
                modes: p.modes,
                slash_commands: p.slash_commands,
                message: p.message,
                quota: p.quota,
                instance: String::new(),
                name: String::new(),
            }
        }
    }

    impl From<Settings> for crate::settings::Settings {
        fn from(s: Settings) -> Self {
            crate::settings::Settings {
                general: s.general,
                appearance: s.appearance.into(),
                composer: s.composer,
                agents: crate::settings::AgentSettings {
                    default_access: s.agents.default_access,
                    providers: s
                        .agents
                        .providers
                        .into_iter()
                        .map(|p| crate::settings::ProviderSettings { kind: p.kind.into(), binary: p.binary, env: p.env })
                        .collect(),
                    instances: Vec::new(),
                    generate_titles: true,
                },
                workspace: s.workspace,
                review: s.review.into(),
                notifications: s.notifications,
                advanced: s.advanced,
            }
        }
    }

    impl From<Event> for crate::wire::Event {
        fn from(event: Event) -> Self {
            use crate::wire::Event as E;
            match event {
                Event::ProjectUpserted(p) => E::ProjectUpserted(p),
                Event::ProjectRemoved(id) => E::ProjectRemoved(id),
                Event::ThreadUpserted(t) => E::ThreadUpserted(t.into()),
                Event::ThreadRemoved(id) => E::ThreadRemoved(id),
                Event::ItemUpserted { thread, item } => E::ItemUpserted { thread, item: item.into() },
                Event::ItemDelta { thread, item, delta } => E::ItemDelta { thread, item, delta },
                Event::ItemRemoved { thread, item } => E::ItemRemoved { thread, item },
                Event::Providers(list) => E::Providers(list.into_iter().map(Into::into).collect()),
                Event::DefaultsChanged(c) => E::DefaultsChanged(c.into()),
                Event::SettingsChanged(s) => E::SettingsChanged(s.into()),
                Event::WorkspaceChanged { thread, files } => E::WorkspaceChanged { thread, files },
                Event::UsageChanged { thread, usage } => E::UsageChanged { thread, usage },
                Event::Conflicts(c) => E::Conflicts(c),
                Event::TerminalOutput { term, data } => E::TerminalOutput { term, data },
                Event::TimelineMark { thread, mark } => E::TimelineMark { thread, mark },
                Event::TerminalExited { term, code } => E::TerminalExited { term, code },
            }
        }
    }
}

/// Version 2: versioned logs with provider instances, before drafts.
/// `Event` had 16 variants; `DraftSaved` made it 17, past the 16-variant
/// packing class, so version 2 records are read with their own layout
/// rather than trusting the new one to agree. Its threads and items are
/// version 3's.
pub mod v2 {
    use super::*;
    use v3::Settings;

    #[derive(Clone, Debug, PartialEq, Encode, Decode)]
    pub enum Event {
        ProjectUpserted(Project),
        ProjectRemoved(ProjectId),
        ThreadUpserted(v3::Thread),
        ThreadRemoved(ThreadId),
        ItemUpserted { thread: ThreadId, item: v3::Item },
        ItemDelta { thread: ThreadId, item: ItemId, delta: Delta },
        ItemRemoved { thread: ThreadId, item: ItemId },
        Providers(Vec<ProviderInfo>),
        DefaultsChanged(ThreadConfig),
        SettingsChanged(Settings),
        WorkspaceChanged { thread: ThreadId, files: Vec<FileChange> },
        UsageChanged { thread: ThreadId, usage: Usage },
        Conflicts(Vec<Conflict>),
        TerminalOutput { term: TermId, data: Vec<u8> },
        TimelineMark { thread: ThreadId, mark: u64 },
        TerminalExited { term: TermId, code: Option<i32> },
    }

    #[derive(Clone, Debug, PartialEq, Encode, Decode)]
    pub struct Envelope {
        pub seq: u64,
        pub event: Event,
    }

    impl From<Event> for crate::wire::Event {
        fn from(event: Event) -> Self {
            use crate::wire::Event as E;
            match event {
                Event::ProjectUpserted(p) => E::ProjectUpserted(p),
                Event::ProjectRemoved(id) => E::ProjectRemoved(id),
                Event::ThreadUpserted(t) => E::ThreadUpserted(t.into()),
                Event::ThreadRemoved(id) => E::ThreadRemoved(id),
                Event::ItemUpserted { thread, item } => E::ItemUpserted { thread, item: item.into() },
                Event::ItemDelta { thread, item, delta } => E::ItemDelta { thread, item, delta },
                Event::ItemRemoved { thread, item } => E::ItemRemoved { thread, item },
                Event::Providers(list) => E::Providers(list),
                Event::DefaultsChanged(c) => E::DefaultsChanged(c),
                Event::SettingsChanged(s) => E::SettingsChanged(s.into()),
                Event::WorkspaceChanged { thread, files } => E::WorkspaceChanged { thread, files },
                Event::UsageChanged { thread, usage } => E::UsageChanged { thread, usage },
                Event::Conflicts(c) => E::Conflicts(c),
                Event::TerminalOutput { term, data } => E::TerminalOutput { term, data },
                Event::TimelineMark { thread, mark } => E::TimelineMark { thread, mark },
                Event::TerminalExited { term, code } => E::TerminalExited { term, code },
            }
        }
    }
}

/// Version 3: drafts, before question options carried descriptions and
/// before the clock and review-sheet settings. `RequestKind::Question`
/// listed its options as bare labels; everything that holds a request (a
/// thread's pending requests, a timeline item, and the events that carry
/// them) and the settings are frozen here with that layout. Thread
/// timeline logs written before version 4 have no version header and use
/// these items too.
pub mod v3 {
    use super::*;
    use crate::settings::{
        AdvancedSettings, AgentSettings, ChatWidth, ColorMode, ComposerSettings, GeneralSettings, Motion,
        NotificationSettings, WorkspaceSettings,
    };

    /// Before the clock setting.
    #[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
    pub struct AppearanceSettings {
        pub mode: ColorMode,
        pub light_theme: String,
        pub dark_theme: String,
        pub ui_scale: u16,
        pub code_font_size: u16,
        pub chat_width: ChatWidth,
        pub motion: Motion,
    }

    /// Before the review sheet's wrap, palette and test-file toggles were
    /// saved.
    #[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
    pub struct ReviewSettings {
        pub split: bool,
        pub collapse_files: bool,
    }

    #[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
    pub struct Settings {
        pub general: GeneralSettings,
        pub appearance: AppearanceSettings,
        pub composer: ComposerSettings,
        pub agents: AgentSettings,
        pub workspace: WorkspaceSettings,
        pub review: ReviewSettings,
        pub notifications: NotificationSettings,
        pub advanced: AdvancedSettings,
    }

    impl From<AppearanceSettings> for crate::settings::AppearanceSettings {
        fn from(a: AppearanceSettings) -> Self {
            crate::settings::AppearanceSettings {
                mode: a.mode,
                light_theme: a.light_theme,
                dark_theme: a.dark_theme,
                ui_scale: a.ui_scale,
                code_font_size: a.code_font_size,
                chat_width: a.chat_width,
                motion: a.motion,
                ..Default::default()
            }
        }
    }

    impl From<ReviewSettings> for crate::settings::ReviewSettings {
        fn from(r: ReviewSettings) -> Self {
            crate::settings::ReviewSettings { split: r.split, collapse_files: r.collapse_files, ..Default::default() }
        }
    }

    impl From<Settings> for crate::settings::Settings {
        fn from(s: Settings) -> Self {
            crate::settings::Settings {
                general: s.general,
                appearance: s.appearance.into(),
                composer: s.composer,
                agents: s.agents,
                workspace: s.workspace,
                review: s.review.into(),
                notifications: s.notifications,
                advanced: s.advanced,
            }
        }
    }

    #[derive(Clone, Debug, PartialEq, Encode, Decode)]
    pub enum RequestKind {
        Command { command: String, cwd: String, reason: String },
        FileChange { path: String, diff: String },
        Tool { name: String, input: String },
        Question { question: String, options: Vec<String>, multi_select: bool },
        Plan { plan: String },
    }

    #[derive(Clone, Debug, PartialEq, Encode, Decode)]
    pub struct AgentRequest {
        pub id: RequestId,
        pub provider_ref: String,
        pub kind: RequestKind,
        pub resolution: Option<Decision>,
    }

    #[derive(Clone, Debug, PartialEq, Encode, Decode)]
    pub struct PendingRequest {
        pub item: ItemId,
        pub request: AgentRequest,
        pub created_ms: u64,
    }

    #[derive(Clone, Debug, PartialEq, Encode, Decode)]
    pub enum ItemBody {
        User(UserMessage),
        Assistant { text: String },
        Reasoning { text: String },
        Tool(ToolCall),
        Plan { steps: Vec<PlanStep> },
        Request(AgentRequest),
        Notice { level: NoticeLevel, text: String },
        TurnEnd(TurnSummary),
        Handoff(Handoff),
    }

    #[derive(Clone, Debug, PartialEq, Encode, Decode)]
    pub struct Item {
        pub id: ItemId,
        pub turn: u32,
        pub status: ItemStatus,
        pub body: ItemBody,
    }

    #[derive(Clone, Debug, PartialEq, Encode, Decode)]
    pub struct Thread {
        pub id: ThreadId,
        pub project: Option<ProjectId>,
        pub title: String,
        pub config: ThreadConfig,
        pub workspace: Workspace,
        pub status: ThreadStatus,
        pub created_ms: u64,
        pub updated_ms: u64,
        pub session: Option<String>,
        pub parent: Option<ThreadId>,
        pub archived: bool,
        pub pinned: bool,
        pub unread: bool,
        pub usage: Usage,
        pub turns: u32,
        pub queued: u32,
        pub pending: Vec<PendingRequest>,
        pub settled_ms: Option<u64>,
        pub snoozed_until: Option<u64>,
        pub woke: bool,
    }

    #[derive(Clone, Debug, PartialEq, Encode, Decode)]
    pub enum Event {
        ProjectUpserted(Project),
        ProjectRemoved(ProjectId),
        ThreadUpserted(Thread),
        ThreadRemoved(ThreadId),
        ItemUpserted { thread: ThreadId, item: Item },
        ItemDelta { thread: ThreadId, item: ItemId, delta: Delta },
        ItemRemoved { thread: ThreadId, item: ItemId },
        Providers(Vec<ProviderInfo>),
        DefaultsChanged(ThreadConfig),
        SettingsChanged(Settings),
        WorkspaceChanged { thread: ThreadId, files: Vec<FileChange> },
        UsageChanged { thread: ThreadId, usage: Usage },
        Conflicts(Vec<Conflict>),
        TerminalOutput { term: TermId, data: Vec<u8> },
        TimelineMark { thread: ThreadId, mark: u64 },
        TerminalExited { term: TermId, code: Option<i32> },
        DraftSaved { target: DraftTarget, draft: Draft },
    }

    #[derive(Clone, Debug, PartialEq, Encode, Decode)]
    pub struct Envelope {
        pub seq: u64,
        pub event: Event,
    }

    impl From<RequestKind> for crate::model::RequestKind {
        fn from(kind: RequestKind) -> Self {
            use crate::model::RequestKind as K;
            match kind {
                RequestKind::Command { command, cwd, reason } => K::Command { command, cwd, reason },
                RequestKind::FileChange { path, diff } => K::FileChange { path, diff },
                RequestKind::Tool { name, input } => K::Tool { name, input },
                RequestKind::Question { question, options, multi_select } => {
                    K::Question { question, options: options.into_iter().map(Into::into).collect(), multi_select }
                }
                RequestKind::Plan { plan } => K::Plan { plan },
            }
        }
    }

    impl From<AgentRequest> for crate::model::AgentRequest {
        fn from(r: AgentRequest) -> Self {
            crate::model::AgentRequest { id: r.id, provider_ref: r.provider_ref, kind: r.kind.into(), resolution: r.resolution }
        }
    }

    impl From<PendingRequest> for crate::model::PendingRequest {
        fn from(p: PendingRequest) -> Self {
            crate::model::PendingRequest { item: p.item, request: p.request.into(), created_ms: p.created_ms }
        }
    }

    impl From<ItemBody> for crate::model::ItemBody {
        fn from(body: ItemBody) -> Self {
            use crate::model::ItemBody as B;
            match body {
                ItemBody::User(m) => B::User(m),
                ItemBody::Assistant { text } => B::Assistant { text },
                ItemBody::Reasoning { text } => B::Reasoning { text },
                ItemBody::Tool(call) => B::Tool(call),
                ItemBody::Plan { steps } => B::Plan { steps },
                ItemBody::Request(r) => B::Request(r.into()),
                ItemBody::Notice { level, text } => B::Notice { level, text },
                ItemBody::TurnEnd(summary) => B::TurnEnd(summary),
                ItemBody::Handoff(handoff) => B::Handoff(handoff),
            }
        }
    }

    impl From<Item> for crate::model::Item {
        fn from(i: Item) -> Self {
            crate::model::Item { id: i.id, turn: i.turn, status: i.status, body: i.body.into() }
        }
    }

    impl From<Thread> for crate::model::Thread {
        fn from(t: Thread) -> Self {
            crate::model::Thread {
                id: t.id,
                project: t.project,
                title: t.title,
                config: t.config,
                workspace: t.workspace,
                status: t.status,
                created_ms: t.created_ms,
                updated_ms: t.updated_ms,
                session: t.session,
                parent: t.parent,
                archived: t.archived,
                pinned: t.pinned,
                unread: t.unread,
                usage: t.usage,
                turns: t.turns,
                queued: t.queued,
                pending: t.pending.into_iter().map(Into::into).collect(),
                settled_ms: t.settled_ms,
                snoozed_until: t.snoozed_until,
                woke: t.woke,
            }
        }
    }

    impl From<Event> for crate::wire::Event {
        fn from(event: Event) -> Self {
            use crate::wire::Event as E;
            match event {
                Event::ProjectUpserted(p) => E::ProjectUpserted(p),
                Event::ProjectRemoved(id) => E::ProjectRemoved(id),
                Event::ThreadUpserted(t) => E::ThreadUpserted(t.into()),
                Event::ThreadRemoved(id) => E::ThreadRemoved(id),
                Event::ItemUpserted { thread, item } => E::ItemUpserted { thread, item: item.into() },
                Event::ItemDelta { thread, item, delta } => E::ItemDelta { thread, item, delta },
                Event::ItemRemoved { thread, item } => E::ItemRemoved { thread, item },
                Event::Providers(list) => E::Providers(list),
                Event::DefaultsChanged(c) => E::DefaultsChanged(c),
                Event::SettingsChanged(s) => E::SettingsChanged(s.into()),
                Event::WorkspaceChanged { thread, files } => E::WorkspaceChanged { thread, files },
                Event::UsageChanged { thread, usage } => E::UsageChanged { thread, usage },
                Event::Conflicts(c) => E::Conflicts(c),
                Event::TerminalOutput { term, data } => E::TerminalOutput { term, data },
                Event::TimelineMark { thread, mark } => E::TimelineMark { thread, mark },
                Event::TerminalExited { term, code } => E::TerminalExited { term, code },
                Event::DraftSaved { target, draft } => E::DraftSaved { target, draft },
            }
        }
    }
}

/// The current schema version of the meta log.
pub const META_VERSION: u32 = 4;

/// Decodes one meta-log envelope written at schema `version`.
///
/// A version 1 record that does not decode is skipped rather than treated
/// as corruption (its frame checksum already passed): the first logs also
/// recorded `TerminalExited`, whose variant index later moved, and those
/// records carry nothing the store keeps.
pub fn decode_envelope(version: u32, payload: &[u8]) -> Result<Envelope, FrameError> {
    match version {
        1 => match bitcode::decode::<v1::Envelope>(payload) {
            Ok(old) => Ok(Envelope { seq: old.seq, event: old.event.into() }),
            Err(_) => Ok(Envelope { seq: 0, event: Event::Providers(Vec::new()) }),
        },
        2 => bitcode::decode::<v2::Envelope>(payload)
            .map(|old| Envelope { seq: old.seq, event: old.event.into() })
            .map_err(FrameError::Decode),
        3 => bitcode::decode::<v3::Envelope>(payload)
            .map(|old| Envelope { seq: old.seq, event: old.event.into() })
            .map_err(FrameError::Decode),
        _ => bitcode::decode(payload).map_err(FrameError::Decode),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_version_1_thread_decodes_with_its_provider() {
        let old = v1::Envelope {
            seq: 4,
            event: v1::Event::ThreadUpserted(v1::Thread {
                id: ThreadId(7),
                project: None,
                title: "Old".into(),
                config: v1::ThreadConfig {
                    provider: v1::ProviderKind::Codex,
                    model: "gpt-5".into(),
                    options: vec![],
                    mode: Mode::Plan,
                    access: Access::AutoEdit,
                },
                workspace: Workspace::default(),
                status: ThreadStatus::Idle,
                created_ms: 1,
                updated_ms: 2,
                session: Some("thr".into()),
                parent: None,
                archived: false,
                pinned: true,
                unread: false,
                usage: Usage::default(),
                turns: 3,
                queued: 0,
                pending: vec![],
                settled_ms: None,
                snoozed_until: None,
                woke: false,
            }),
        };
        let bytes = bitcode::encode(&old);
        assert!(bitcode::decode::<Envelope>(&bytes).is_err(), "the current layout cannot read version 1");
        let env = decode_envelope(1, &bytes).unwrap();
        let Event::ThreadUpserted(thread) = env.event else { panic!("{env:?}") };
        assert_eq!(thread.config.provider, ProviderKind::Codex);
        assert_eq!(thread.config.instance_id(), "codex");
        assert_eq!((thread.turns, thread.pinned, thread.config.mode), (3, true, Mode::Plan));
    }

    #[test]
    fn version_1_settings_keep_every_provider() {
        use crate::settings::EnvVar;
        let base = crate::settings::Settings::default();
        let old = v1::Envelope {
            seq: 9,
            event: v1::Event::SettingsChanged(v1::Settings {
                general: base.general.clone(),
                appearance: v3_appearance(),
                composer: base.composer.clone(),
                agents: v1::AgentSettings {
                    default_access: Access::AutoEdit,
                    providers: vec![
                        v1::ProviderSettings { kind: v1::ProviderKind::Claude, binary: "/opt/claude".into(), env: vec![] },
                        v1::ProviderSettings {
                            kind: v1::ProviderKind::Codex,
                            binary: String::new(),
                            env: vec![EnvVar::new("CODEX_HOME", "/w")],
                        },
                    ],
                },
                workspace: base.workspace.clone(),
                review: v3::ReviewSettings { split: true, collapse_files: false },
                notifications: base.notifications.clone(),
                advanced: base.advanced.clone(),
            }),
        };
        let env = decode_envelope(1, &bitcode::encode(&old)).unwrap();
        let Event::SettingsChanged(settings) = env.event else { panic!("{env:?}") };
        assert_eq!(settings.provider(ProviderKind::Claude).binary, "/opt/claude");
        assert_eq!(settings.provider(ProviderKind::Codex).env[0].value, "/w");
        assert_eq!(settings.agents.default_access, Access::AutoEdit);
    }

    fn v3_appearance() -> v3::AppearanceSettings {
        let a = crate::settings::AppearanceSettings::default();
        v3::AppearanceSettings {
            mode: a.mode,
            light_theme: a.light_theme,
            dark_theme: a.dark_theme,
            ui_scale: 125,
            code_font_size: a.code_font_size,
            chat_width: a.chat_width,
            motion: a.motion,
        }
    }

    #[test]
    fn version_3_settings_gain_the_clock_and_review_toggles_with_defaults() {
        let base = crate::settings::Settings::default();
        let old = v3::Envelope {
            seq: 3,
            event: v3::Event::SettingsChanged(v3::Settings {
                general: base.general.clone(),
                appearance: v3_appearance(),
                composer: base.composer.clone(),
                agents: base.agents.clone(),
                workspace: base.workspace.clone(),
                review: v3::ReviewSettings { split: true, collapse_files: true },
                notifications: base.notifications.clone(),
                advanced: base.advanced.clone(),
            }),
        };
        let env = decode_envelope(3, &bitcode::encode(&old)).unwrap();
        let Event::SettingsChanged(settings) = env.event else { panic!("{env:?}") };
        assert_eq!(settings.appearance.ui_scale, 125);
        assert_eq!(settings.appearance.clock, crate::settings::ClockFormat::H24);
        let r = &settings.review;
        assert_eq!((r.split, r.collapse_files, r.wrap, r.colorblind, r.hide_tests), (true, true, false, false, false));
        // Version 2 settings come through the same frozen layout.
        let v2 = v2::Envelope { seq: 4, event: v2::Event::SettingsChanged(match old.event {
            v3::Event::SettingsChanged(s) => s,
            _ => unreachable!(),
        }) };
        let env = decode_envelope(2, &bitcode::encode(&v2)).unwrap();
        assert!(matches!(env.event, Event::SettingsChanged(s) if s.review.split && s.appearance.ui_scale == 125));
    }

    /// A version 3 thread, as version 3 wrote it.
    fn v3_thread(id: u64, title: &str, pending: Vec<v3::PendingRequest>) -> v3::Thread {
        let t = Thread::default();
        v3::Thread {
            id: ThreadId(id),
            project: None,
            title: title.into(),
            config: t.config,
            workspace: t.workspace,
            status: t.status,
            created_ms: 0,
            updated_ms: 0,
            session: None,
            parent: None,
            archived: false,
            pinned: false,
            unread: false,
            usage: t.usage,
            turns: 2,
            queued: 0,
            pending,
            settled_ms: None,
            snoozed_until: None,
            woke: false,
        }
    }

    fn v3_question(id: u64) -> v3::AgentRequest {
        v3::AgentRequest {
            id: RequestId(id),
            provider_ref: "toolu_1".into(),
            kind: v3::RequestKind::Question {
                question: "Which database?".into(),
                options: vec!["Postgres".into(), "SQLite".into()],
                multi_select: false,
            },
            resolution: None,
        }
    }

    #[test]
    fn a_version_2_log_decodes_after_drafts_were_added() {
        let old = v2::Envelope { seq: 5, event: v2::Event::ThreadUpserted(v3_thread(9, "Before drafts", vec![])) };
        let bytes = bitcode::encode(&old);
        let env = decode_envelope(2, &bytes).unwrap();
        let thread = Thread { id: ThreadId(9), title: "Before drafts".into(), turns: 2, ..Default::default() };
        assert_eq!((env.seq, env.event), (5, Event::ThreadUpserted(thread)));
        let exited = v2::Envelope { seq: 6, event: v2::Event::TerminalExited { term: TermId(1), code: Some(0) } };
        let env = decode_envelope(2, &bitcode::encode(&exited)).unwrap();
        assert_eq!(env.event, Event::TerminalExited { term: TermId(1), code: Some(0) });
    }

    #[test]
    fn a_version_3_question_decodes_with_options_without_descriptions() {
        let pending = vec![v3::PendingRequest { item: ItemId(3), request: v3_question(4), created_ms: 7 }];
        let old = v3::Envelope { seq: 11, event: v3::Event::ThreadUpserted(v3_thread(5, "Asking", pending)) };
        let bytes = bitcode::encode(&old);
        assert!(bitcode::decode::<Envelope>(&bytes).is_err(), "the current layout cannot read version 3");
        let env = decode_envelope(3, &bytes).unwrap();
        let Event::ThreadUpserted(thread) = env.event else { panic!("{env:?}") };
        assert_eq!(thread.title, "Asking");
        let RequestKind::Question { question, options, multi_select } = &thread.pending[0].request.kind else {
            panic!("{:?}", thread.pending)
        };
        assert_eq!(question, "Which database?");
        assert_eq!(options, &vec![QuestionOption::new("Postgres", ""), QuestionOption::new("SQLite", "")]);
        assert!(!multi_select);

        // Items, and the drafts version 3 introduced, come through too.
        let item = v3::Item { id: ItemId(3), turn: 1, status: ItemStatus::Done, body: v3::ItemBody::Request(v3_question(4)) };
        let old = v3::Envelope { seq: 12, event: v3::Event::ItemUpserted { thread: ThreadId(5), item } };
        let env = decode_envelope(3, &bitcode::encode(&old)).unwrap();
        let Event::ItemUpserted { item, .. } = env.event else { panic!("{env:?}") };
        assert!(matches!(item.body, ItemBody::Request(AgentRequest { kind: RequestKind::Question { ref options, .. }, .. }) if options[1].label == "SQLite"));
        let draft = Draft { text: "unsent".into(), ..Default::default() };
        let old = v3::Envelope { seq: 13, event: v3::Event::DraftSaved { target: DraftTarget::Thread(ThreadId(5)), draft: draft.clone() } };
        let env = decode_envelope(3, &bitcode::encode(&old)).unwrap();
        assert_eq!(env.event, Event::DraftSaved { target: DraftTarget::Thread(ThreadId(5)), draft });
    }

    #[test]
    fn unreadable_version_1_records_are_skipped_not_fatal() {
        let env = decode_envelope(1, &[0xff, 0xff, 0xff]).unwrap();
        assert_eq!(env.event, Event::Providers(Vec::new()));
        assert!(decode_envelope(2, &[0xff, 0xff, 0xff]).is_err());
    }

    #[test]
    fn appended_item_variants_keep_old_items_readable() {
        // ItemBody gained `Handoff`; old items still decode unchanged.
        #[derive(Encode)]
        #[allow(dead_code)]
        enum OldBody {
            User(UserMessage),
            Assistant { text: String },
            Reasoning { text: String },
            Tool(ToolCall),
            Plan { steps: Vec<PlanStep> },
            Request(AgentRequest),
            Notice { level: NoticeLevel, text: String },
            TurnEnd(TurnSummary),
        }
        let bytes = bitcode::encode(&vec![OldBody::Assistant { text: "hi".into() }, OldBody::TurnEnd(TurnSummary::default())]);
        let back: Vec<ItemBody> = bitcode::decode(&bytes).unwrap();
        assert_eq!(back[0], ItemBody::Assistant { text: "hi".into() });
        assert!(matches!(back[1], ItemBody::TurnEnd(_)));
    }
}
