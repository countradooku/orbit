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
//!   schema version and a frozen copy of the old struct here.

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
        AdvancedSettings, AppearanceSettings, ComposerSettings, EnvVar, GeneralSettings, NotificationSettings,
        ReviewSettings, WorkspaceSettings,
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
        pub pending: Vec<PendingRequest>,
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
        pub appearance: AppearanceSettings,
        pub composer: ComposerSettings,
        pub agents: AgentSettings,
        pub workspace: WorkspaceSettings,
        pub review: ReviewSettings,
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
                pending: t.pending,
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
                appearance: s.appearance,
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
                review: s.review,
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
                Event::ItemUpserted { thread, item } => E::ItemUpserted { thread, item },
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
/// rather than trusting the new one to agree. The payload types are
/// unchanged.
pub mod v2 {
    use super::*;
    use crate::settings::Settings;

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
                Event::ThreadUpserted(t) => E::ThreadUpserted(t),
                Event::ThreadRemoved(id) => E::ThreadRemoved(id),
                Event::ItemUpserted { thread, item } => E::ItemUpserted { thread, item },
                Event::ItemDelta { thread, item, delta } => E::ItemDelta { thread, item, delta },
                Event::ItemRemoved { thread, item } => E::ItemRemoved { thread, item },
                Event::Providers(list) => E::Providers(list),
                Event::DefaultsChanged(c) => E::DefaultsChanged(c),
                Event::SettingsChanged(s) => E::SettingsChanged(s),
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

/// The current schema version of the meta log.
pub const META_VERSION: u32 = 3;

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
                appearance: base.appearance.clone(),
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
                review: base.review.clone(),
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

    #[test]
    fn a_version_2_log_decodes_after_drafts_were_added() {
        let thread = Thread { id: ThreadId(9), title: "Before drafts".into(), turns: 2, ..Default::default() };
        let old = v2::Envelope { seq: 5, event: v2::Event::ThreadUpserted(thread.clone()) };
        let bytes = bitcode::encode(&old);
        let env = decode_envelope(2, &bytes).unwrap();
        assert_eq!((env.seq, env.event), (5, Event::ThreadUpserted(thread)));
        let exited = v2::Envelope { seq: 6, event: v2::Event::TerminalExited { term: TermId(1), code: Some(0) } };
        let env = decode_envelope(2, &bitcode::encode(&exited)).unwrap();
        assert_eq!(env.event, Event::TerminalExited { term: TermId(1), code: Some(0) });
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
