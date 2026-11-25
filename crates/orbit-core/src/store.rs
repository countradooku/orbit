//! Durable state that stays small.
//!
//! Layout of the data directory:
//!
//! ```text
//! meta.log            projects, threads, defaults, settings, drafts (durable envelopes)
//! threads/<id>.log    one thread's timeline records
//! blobs/<ab>/<hash>   attachments and large tool outputs, by BLAKE3 hash
//! ```
//!
//! Three rules keep it from growing the way T3 Code's event store did:
//!
//! 1. Timelines are loaded only when a thread is opened and dropped when it is
//!    closed, so memory follows what is on screen, not the whole history.
//! 2. Streaming deltas are never written; an item is written when it
//!    completes (and at most every few seconds while it streams).
//! 3. Every log is compacted once its dead records outweigh its live ones,
//!    and deleting a thread deletes its log, blobs and checkpoints.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use bitcode::{Decode, Encode};
use orbit_proto::legacy;
use orbit_proto::*;

/// A record in a thread's log.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
#[derive(wincode::SchemaWrite, wincode::SchemaRead)]
pub enum TimelineRecord {
    Upsert(Item),
    Remove(ItemId),
}

/// A timeline record as schema versions up to 3 wrote it (items as
/// [`legacy::v3::Item`]). Those timeline logs have no version header.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
enum TimelineRecordV3 {
    Upsert(legacy::v3::Item),
    Remove(ItemId),
}

impl From<TimelineRecordV3> for TimelineRecord {
    fn from(record: TimelineRecordV3) -> Self {
        match record {
            TimelineRecordV3::Upsert(item) => TimelineRecord::Upsert(item.into()),
            TimelineRecordV3::Remove(id) => TimelineRecord::Remove(id),
        }
    }
}

/// Decodes one timeline record written at schema `version`.
fn decode_timeline(version: u32, payload: &[u8]) -> Result<TimelineRecord, FrameError> {
    match version {
        ..=3 => bitcode_decode::<TimelineRecordV3>(payload).map(Into::into),
        legacy::LAST_BITCODE_VERSION => bitcode_decode::<TimelineRecord>(payload),
        _ => decode::<TimelineRecord>(payload),
    }
}

/// Append-only file of CRC-framed records with crash recovery.
struct Log {
    path: PathBuf,
    /// `None` only transiently while the file is being replaced.
    file: Option<File>,
    /// Records written since the last compaction (including replayed ones).
    records: usize,
    buf: Vec<u8>,
    codec: Codec,
    /// The version frame every rewrite starts with, for versioned logs.
    header: Option<Vec<u8>>,
}

fn header_version(header: &[u8]) -> u32 {
    u32::from_le_bytes(header[header.len() - 4..].try_into().unwrap())
}

/// Marks a meta log; followed by its schema version. Logs from before
/// versioning have no such frame and are version 1 (see
/// [`orbit_proto::legacy`]).
const META_MAGIC: &[u8] = b"orbit/meta\0";

fn meta_header() -> Vec<u8> {
    let mut header = META_MAGIC.to_vec();
    header.extend_from_slice(&legacy::META_VERSION.to_le_bytes());
    header
}

/// Marks a thread's timeline log; followed by the schema version, which
/// it shares with the meta log. Timeline logs from before version 4 have
/// no such frame.
const TIMELINE_MAGIC: &[u8] = b"orbit/thread\0";

fn timeline_header() -> Vec<u8> {
    let mut header = TIMELINE_MAGIC.to_vec();
    header.extend_from_slice(&legacy::META_VERSION.to_le_bytes());
    header
}

/// A timeline log's schema version and its records, from its bytes.
fn read_timeline(bytes: &[u8]) -> (u32, FrameReader<'_>) {
    let mut peek = FrameReader::new(bytes);
    if let Ok(first) = peek.next_payload()
        && first.len() == TIMELINE_MAGIC.len() + 4
        && first.starts_with(TIMELINE_MAGIC)
    {
        return (header_version(first), peek);
    }
    (1, FrameReader::new(bytes))
}

impl Log {
    /// Opens `path`, returning the log, every valid record in it and the
    /// schema version it was written with. A torn or corrupted tail (crash
    /// mid-write) is truncated away.
    ///
    /// With a `header` (magic bytes followed by a little-endian `u32`
    /// version), the log's first frame names its schema version; a log
    /// without one is version 1, from before logs were versioned. A new
    /// file starts with `header`. `decode` reads one record of a version.
    fn open<T>(
        path: &Path,
        header: Option<Vec<u8>>,
        mut decode: impl FnMut(u32, &[u8]) -> Result<T, FrameError>,
    ) -> io::Result<(Self, Vec<T>, u32)> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let mut bytes = Vec::new();
        if let Ok(mut f) = File::open(path) {
            f.read_to_end(&mut bytes)?;
        }
        let mut reader = FrameReader::new(&bytes);
        let mut version = 1;
        if let Some(header) = &header {
            let magic = &header[..header.len() - 4];
            let mut peek = FrameReader::new(&bytes);
            if let Ok(first) = peek.next_payload()
                && first.len() == header.len()
                && first.starts_with(magic)
            {
                version = header_version(first);
                reader = peek;
            }
        }
        let mut out = Vec::new();
        loop {
            let start = reader.position();
            match reader.next_payload().and_then(|payload| decode(version, payload)) {
                Ok(record) => out.push(record),
                Err(FrameError::Incomplete) if reader.position() == bytes.len() => break,
                Err(err) => {
                    // Keep everything before the bad record.
                    let valid = if matches!(err, FrameError::Decode(_)) { start } else { reader.position() };
                    log::warn!("{}: dropping {} bytes after a bad record ({err})", path.display(), bytes.len() - valid);
                    reader = FrameReader::new(&bytes[..valid]);
                    while reader.next_payload().is_ok() {}
                    break;
                }
            }
        }
        let valid = reader.position() as u64;
        if valid < bytes.len() as u64 {
            // An append-only handle cannot truncate on Windows; use a writer.
            OpenOptions::new().write(true).open(path)?.set_len(valid)?;
        }
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        if valid == 0
            && let Some(header) = &header
        {
            let mut frame = Vec::new();
            write_raw_frame(header, &mut frame);
            file.write_all(&frame)?;
            version = header_version(header);
        }
        let records = out.len();
        Ok((
            Self {
                path: path.to_owned(),
                file: Some(file),
                records,
                buf: Vec::with_capacity(4096),
                codec: Codec::new(),
                header,
            },
            out,
            version,
        ))
    }

    fn append<T: Wire>(&mut self, record: &T) -> io::Result<()> {
        self.buf.clear();
        self.codec.write_frame(record, &mut self.buf);
        self.file.as_mut().expect("log file is open").write_all(&self.buf)?;
        self.records += 1;
        Ok(())
    }

    /// Atomically replaces the log's contents with `records`.
    fn rewrite<'a, T: Wire + 'a>(&mut self, records: impl IntoIterator<Item = &'a T>) -> io::Result<()> {
        let tmp = self.path.with_extension("log.tmp");
        let mut out = Vec::new();
        if let Some(header) = &self.header {
            write_raw_frame(header, &mut out);
        }
        let mut count = 0;
        for record in records {
            self.codec.write_frame(record, &mut out);
            count += 1;
        }
        {
            let mut f = File::create(&tmp)?;
            f.write_all(&out)?;
            f.sync_all()?;
        }
        // Windows refuses to replace a file that is still open.
        self.file = None;
        let renamed = fs::rename(&tmp, &self.path);
        self.file = Some(OpenOptions::new().create(true).append(true).open(&self.path)?);
        renamed?;
        self.records = count;
        Ok(())
    }

    fn sync(&self) -> io::Result<()> {
        match &self.file {
            Some(file) => file.sync_data(),
            None => Ok(()),
        }
    }
}

/// A loaded timeline: items in display order with an id index.
#[derive(Default)]
pub struct Timeline {
    items: Vec<Item>,
    index: HashMap<ItemId, usize>,
}

impl Timeline {
    pub fn items(&self) -> &[Item] {
        &self.items
    }

    pub fn get(&self, id: ItemId) -> Option<&Item> {
        self.index.get(&id).map(|&ix| &self.items[ix])
    }

    pub fn get_mut(&mut self, id: ItemId) -> Option<&mut Item> {
        self.index.get(&id).map(|&ix| &mut self.items[ix])
    }

    fn upsert(&mut self, item: Item) {
        match self.index.get(&item.id) {
            Some(&ix) => self.items[ix] = item,
            None => {
                self.index.insert(item.id, self.items.len());
                self.items.push(item);
            }
        }
    }

    fn remove(&mut self, id: ItemId) {
        if let Some(ix) = self.index.remove(&id) {
            self.items.remove(ix);
            for item in &self.items[ix..] {
                *self.index.get_mut(&item.id).unwrap() -= 1;
            }
        }
    }

    fn apply(&mut self, record: TimelineRecord) {
        match record {
            TimelineRecord::Upsert(item) => self.upsert(item),
            TimelineRecord::Remove(id) => self.remove(id),
        }
    }
}

struct OpenTimeline {
    log: Log,
    timeline: Timeline,
}

/// All durable state of one engine.
pub struct Store {
    /// Held for the store's lifetime: one engine per data folder.
    _lock: File,
    dir: PathBuf,
    meta: Log,
    /// Identifies this process lifetime. Sequence numbers are only comparable
    /// within one epoch; a client that sees a new epoch resyncs from a
    /// snapshot instead of replaying `since`.
    epoch: u64,
    seq: u64,
    projects: Vec<Project>,
    threads: HashMap<ThreadId, Thread>,
    defaults: ThreadConfig,
    /// Unsent composer messages; never empty drafts.
    drafts: HashMap<DraftTarget, Draft>,
    settings: Settings,
    open: HashMap<ThreadId, OpenTimeline>,
    /// Automations and their recent runs.
    automations: crate::automations::records::Records,
    /// Projects' configurations (`engine/workspaces.rs`).
    project_configs: HashMap<ProjectId, ProjectConfig>,
}

impl Store {
    pub fn open(dir: impl Into<PathBuf>) -> io::Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(dir.join("threads"))?;
        let lock = OpenOptions::new().create(true).truncate(false).write(true).open(dir.join("lock"))?;
        if let Err(err) = lock.try_lock() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!("another Orbit is already using {} ({err})", dir.display()),
            ));
        }
        fs::create_dir_all(dir.join("blobs"))?;
        let (meta, records, version) = Log::open(&dir.join("meta.log"), Some(meta_header()), legacy::decode_envelope)?;
        if version > legacy::META_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} was written by a newer version of Orbit; update Orbit to open it.", dir.display()),
            ));
        }
        let mut store = Self {
            _lock: lock,
            dir,
            meta,
            epoch: next_raw_id(),
            seq: 0,
            projects: Vec::new(),
            threads: HashMap::new(),
            defaults: ThreadConfig::default(),
            drafts: HashMap::new(),
            settings: Settings::default(),
            open: HashMap::new(),
            automations: Default::default(),
            project_configs: HashMap::new(),
        };
        for env in records {
            store.seq = store.seq.max(env.seq);
            store.apply_meta(env.event);
        }
        // Threads that were busy when the process died are not busy now.
        for thread in store.threads.values_mut() {
            if thread.status.is_busy() {
                thread.status = ThreadStatus::Idle;
            }
            observe_raw_id(thread.id.0);
        }
        for project in &store.projects {
            observe_raw_id(project.id.0);
        }
        if version < legacy::META_VERSION {
            // Written by an older Orbit: rewrite it in the current layout
            // once, so appends never mix layouts.
            log::info!("migrating {} from schema {version}", store.meta.path.display());
            store.rewrite_meta()?;
        }
        Ok(store)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn seq(&self) -> u64 {
        self.seq
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn projects(&self) -> &[Project] {
        &self.projects
    }

    pub fn project(&self, id: ProjectId) -> Option<&Project> {
        self.projects.iter().find(|p| p.id == id)
    }

    pub fn threads(&self) -> impl Iterator<Item = &Thread> {
        self.threads.values()
    }

    pub fn thread(&self, id: ThreadId) -> Option<&Thread> {
        self.threads.get(&id)
    }

    pub fn defaults(&self) -> &ThreadConfig {
        &self.defaults
    }

    pub fn draft(&self, target: DraftTarget) -> Option<&Draft> {
        self.drafts.get(&target)
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub fn automations(&self) -> &crate::automations::records::Records {
        &self.automations
    }

    /// A project's configuration; `None` when it was never set.
    pub fn project_config(&self, id: ProjectId) -> Option<&ProjectConfig> {
        self.project_configs.get(&id)
    }

    pub fn snapshot(&self, providers: Vec<ProviderInfo>) -> Snapshot {
        let mut threads: Vec<Thread> = self.threads.values().cloned().collect();
        threads.sort_by_key(|t| std::cmp::Reverse(t.updated_ms));
        let (automations, automation_runs) = self.automations.snapshot();
        Snapshot {
            epoch: self.epoch,
            seq: self.seq,
            projects: self.projects.clone(),
            threads,
            providers,
            defaults: self.defaults.clone(),
            drafts: self.drafts.iter().map(|(k, v)| (*k, v.clone())).collect(),
            settings: Box::new(self.settings.clone()),
            live: Vec::new(),
            automations,
            automation_runs,
            teams: Default::default(),
            project_configs: self.project_configs.iter().map(|(id, c)| (*id, c.clone())).collect(),
            git: Vec::new(),
            runs: Vec::new(),
            quota: Default::default(),
        }
    }

    fn apply_meta(&mut self, event: Event) {
        match event {
            Event::ProjectUpserted(project) => match self.projects.iter_mut().find(|p| p.id == project.id) {
                Some(slot) => *slot = *project,
                None => {
                    self.projects.push(*project);
                    self.projects.sort_by_key(|p| p.order);
                }
            },
            Event::ProjectRemoved(id) => {
                self.projects.retain(|p| p.id != id);
                self.project_configs.remove(&id);
            }
            Event::ProjectConfigChanged { project, config } => {
                if self.projects.iter().any(|p| p.id == project) {
                    self.project_configs.insert(project, *config);
                }
            }
            Event::ThreadUpserted(thread) => {
                self.threads.insert(thread.id, *thread);
            }
            Event::ThreadRemoved(id) => {
                self.threads.remove(&id);
                self.drafts.remove(&DraftTarget::Thread(id));
            }
            Event::DefaultsChanged(config) => self.defaults = *config,
            Event::DraftSaved { target, draft } => {
                if draft.is_empty() {
                    self.drafts.remove(&target);
                } else {
                    self.drafts.insert(target, *draft);
                }
            }
            Event::SettingsChanged(settings) => self.settings = *settings,
            event @ (Event::AutomationUpserted(_) | Event::AutomationRemoved(_) | Event::AutomationRun(_)) => {
                self.automations.apply(event)
            }
            _ => {}
        }
    }

    /// Persists a durable event and returns its envelope with a fresh
    /// sequence number. Item events go to the thread's own log.
    pub fn commit(&mut self, event: Event) -> io::Result<Envelope> {
        debug_assert!(event.is_durable());
        self.seq += 1;
        let env = Envelope { seq: self.seq, event };
        match &env.event {
            Event::ItemUpserted { thread, item } => {
                let thread = *thread;
                let item = item.clone();
                self.timeline_log(thread)?.append(&TimelineRecord::Upsert((*item).clone()))?;
                if let Some(open) = self.open.get_mut(&thread) {
                    open.timeline.upsert(*item);
                }
                self.maybe_compact_timeline(thread)?;
            }
            Event::ItemRemoved { thread, item } => {
                let (thread, item) = (*thread, *item);
                self.timeline_log(thread)?.append(&TimelineRecord::Remove(item))?;
                if let Some(open) = self.open.get_mut(&thread) {
                    open.timeline.remove(item);
                }
            }
            Event::ThreadRemoved(id) => {
                let id = *id;
                self.meta.append(&env)?;
                self.apply_meta(env.event.clone());
                self.open.remove(&id);
                let _ = fs::remove_file(self.timeline_path(id));
                self.maybe_compact_meta()?;
            }
            _ => {
                self.meta.append(&env)?;
                self.apply_meta(env.event.clone());
                self.maybe_compact_meta()?;
            }
        }
        Ok(env)
    }

    fn timeline_path(&self, id: ThreadId) -> PathBuf {
        self.dir.join("threads").join(format!("{id}.log"))
    }

    fn timeline_log(&mut self, id: ThreadId) -> io::Result<&mut Log> {
        self.load_timeline(id)?;
        Ok(&mut self.open.get_mut(&id).unwrap().log)
    }

    /// Loads a thread's timeline into memory (no-op when already loaded).
    pub fn load_timeline(&mut self, id: ThreadId) -> io::Result<&Timeline> {
        if !self.open.contains_key(&id) {
            let (mut log, records, version) = Log::open(&self.timeline_path(id), Some(timeline_header()), decode_timeline)?;
            if version > legacy::META_VERSION {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "This thread was written by a newer version of Orbit; update Orbit to open it.",
                ));
            }
            let mut timeline = Timeline::default();
            for record in records {
                timeline.apply(record);
            }
            if version < legacy::META_VERSION {
                // Written by an older Orbit: rewrite it in the current layout
                // once, so appends never mix layouts.
                let records: Vec<TimelineRecord> = timeline.items.iter().cloned().map(TimelineRecord::Upsert).collect();
                log.rewrite(records.iter())?;
            }
            // A thread that is not running cannot have streaming items: they
            // are leftovers of a turn cut off by a crash or a forced quit.
            let busy = self.threads.get(&id).is_some_and(|t| t.status.is_busy());
            for item in &mut timeline.items {
                observe_raw_id(item.id.0);
                if !busy && item.status == ItemStatus::Streaming {
                    item.status = ItemStatus::Cancelled;
                }
            }
            self.open.insert(id, OpenTimeline { log, timeline });
        }
        Ok(&self.open[&id].timeline)
    }

    pub fn timeline(&self, id: ThreadId) -> Option<&Timeline> {
        self.open.get(&id).map(|o| &o.timeline)
    }

    /// Mutable access for in-memory streaming updates that are not yet
    /// persisted. Call [`Store::commit`] with the final item when it completes.
    pub fn timeline_mut(&mut self, id: ThreadId) -> Option<&mut Timeline> {
        self.open.get_mut(&id).map(|o| &mut o.timeline)
    }

    /// Reads a thread's timeline straight from disk without loading it into
    /// the store (search). Safe to call from any thread.
    pub fn scan_timeline(dir: &Path, id: ThreadId) -> Vec<Item> {
        let path = dir.join("threads").join(format!("{id}.log"));
        let Ok(bytes) = fs::read(&path) else { return Vec::new() };
        let (version, mut reader) = read_timeline(&bytes);
        let mut timeline = Timeline::default();
        while let Ok(record) = reader.next_payload().and_then(|payload| decode_timeline(version, payload)) {
            timeline.apply(record);
        }
        timeline.items
    }

    /// Releases a timeline's memory. Busy threads stay loaded.
    pub fn unload_timeline(&mut self, id: ThreadId) {
        if self.threads.get(&id).is_some_and(|t| !t.status.is_busy()) {
            self.open.remove(&id);
        }
    }

    fn maybe_compact_timeline(&mut self, id: ThreadId) -> io::Result<()> {
        let open = self.open.get_mut(&id).unwrap();
        let live = open.timeline.items.len();
        if open.log.records > 64 && open.log.records > live * 2 {
            let records: Vec<TimelineRecord> =
                open.timeline.items.iter().cloned().map(TimelineRecord::Upsert).collect();
            open.log.rewrite(records.iter())?;
        }
        Ok(())
    }

    fn maybe_compact_meta(&mut self) -> io::Result<()> {
        let live = self.projects.len()
            + self.project_configs.len()
            + self.threads.len()
            + self.drafts.len()
            + self.automations.len()
            + 2;
        if self.meta.records > 256 && self.meta.records > live * 4 {
            self.rewrite_meta()?;
        }
        Ok(())
    }

    /// Replaces the meta log with one record per live object, in the
    /// current schema.
    fn rewrite_meta(&mut self) -> io::Result<()> {
        let seq = self.seq;
        let mut records = Vec::with_capacity(self.projects.len() + self.threads.len() + self.drafts.len() + 2);
        for p in &self.projects {
            records.push(Envelope { seq, event: Event::ProjectUpserted(Box::new(p.clone())) });
            if let Some(config) = self.project_configs.get(&p.id) {
                let event = Event::ProjectConfigChanged { project: p.id, config: Box::new(config.clone()) };
                records.push(Envelope { seq, event });
            }
        }
        for t in self.threads.values() {
            records.push(Envelope { seq, event: Event::ThreadUpserted(Box::new(t.clone())) });
        }
        records.push(Envelope { seq, event: Event::DefaultsChanged(Box::new(self.defaults.clone())) });
        records.push(Envelope { seq, event: Event::SettingsChanged(Box::new(self.settings.clone())) });
        for (target, draft) in &self.drafts {
            records.push(Envelope { seq, event: Event::DraftSaved { target: *target, draft: Box::new(draft.clone()) } });
        }
        records.extend(self.automations.records(seq));
        self.meta.rewrite(records.iter())
    }

    /// Flushes logs to stable storage. Called on a timer while dirty and at
    /// shutdown, not on every write.
    pub fn sync(&self) -> io::Result<()> {
        self.meta.sync()?;
        for open in self.open.values() {
            open.log.sync()?;
        }
        Ok(())
    }

    // ── Blobs ────────────────────────────────────────────────────────────

    pub fn blob_path(&self, hash: &str) -> PathBuf {
        self.dir.join("blobs").join(&hash[..2]).join(hash)
    }

    /// Stores `data` once by content hash and returns the attachment record.
    pub fn put_blob(&self, name: String, mime: String, data: &[u8]) -> io::Result<Attachment> {
        Self::put_blob_in(&self.dir, name, mime, data)
    }

    /// [`put_blob`](Self::put_blob) for background tasks that hold only the
    /// data directory.
    pub fn put_blob_in(dir: &Path, name: String, mime: String, data: &[u8]) -> io::Result<Attachment> {
        let hash = blake3::hash(data).to_hex().to_string();
        let path = dir.join("blobs").join(&hash[..2]).join(&hash);
        if !path.exists() {
            fs::create_dir_all(path.parent().unwrap())?;
            let tmp = path.with_extension("tmp");
            fs::write(&tmp, data)?;
            fs::rename(&tmp, &path)?;
        }
        Ok(Attachment { name, mime, hash, size: data.len() as u64 })
    }

    pub fn read_blob(&self, hash: &str) -> io::Result<Vec<u8>> {
        if hash.len() < 3 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "bad blob hash"));
        }
        fs::read(self.blob_path(hash))
    }

    /// Deletes blobs that no remaining timeline or draft references. Runs
    /// after a thread is deleted; scans logs without keeping them loaded.
    pub fn collect_garbage(&mut self) -> io::Result<usize> {
        let mut referenced = std::collections::HashSet::new();
        for draft in self.drafts.values() {
            referenced.extend(draft.attachments.iter().map(|a| a.hash.clone()));
        }
        let ids: Vec<ThreadId> = self.threads.keys().copied().collect();
        for id in ids {
            let loaded = self.open.contains_key(&id);
            self.load_timeline(id)?;
            for item in self.open[&id].timeline.items() {
                match &item.body {
                    ItemBody::User(m) => referenced.extend(m.attachments.iter().map(|a| a.hash.clone())),
                    ItemBody::Tool(t) => referenced.extend(t.output_blob.clone()),
                    _ => {}
                }
            }
            if !loaded {
                self.open.remove(&id);
            }
        }
        let mut removed = 0;
        for shard in fs::read_dir(self.dir.join("blobs"))? {
            let shard = shard?;
            if !shard.file_type()?.is_dir() {
                continue;
            }
            for blob in fs::read_dir(shard.path())? {
                let blob = blob?;
                let name = blob.file_name().to_string_lossy().into_owned();
                if !referenced.contains(&name) {
                    fs::remove_file(blob.path())?;
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thread(id: ThreadId) -> Thread {
        Thread { id, title: "Fix the build".into(), ..Default::default() }
    }

    fn text_item(turn: u32, text: &str) -> Item {
        Item {
            id: ItemId::new(),
            turn,
            status: ItemStatus::Done,
            body: ItemBody::Assistant { text: text.into() },
        }
    }

    #[test]
    fn state_survives_reopen_and_timelines_load_lazily() {
        let dir = tempfile::tempdir().unwrap();
        let id = ThreadId::new();
        let item = text_item(1, "done");
        {
            let mut store = Store::open(dir.path()).unwrap();
            store.commit(Event::ThreadUpserted(Box::new(thread(id)))).unwrap();
            store.commit(Event::ItemUpserted { thread: id, item: Box::new(item.clone()) }).unwrap();
            store.unload_timeline(id);
            assert!(store.timeline(id).is_none());
        }
        let mut store = Store::open(dir.path()).unwrap();
        assert_eq!(store.thread(id).unwrap().title, "Fix the build");
        assert!(store.timeline(id).is_none(), "timeline must not load at startup");
        assert_eq!(store.load_timeline(id).unwrap().items(), &[item]);
    }

    #[test]
    fn settings_survive_reopen_and_compaction() {
        let dir = tempfile::tempdir().unwrap();
        let mut settings = Settings::default();
        settings.appearance.ui_scale = 125;
        settings.provider_mut(ProviderKind::Claude).binary = "/opt/claude/bin/claude".into();
        {
            let mut store = Store::open(dir.path()).unwrap();
            store.commit(Event::SettingsChanged(Box::new(settings.clone()))).unwrap();
            // Enough unrelated records to force a meta-log rewrite.
            let id = ThreadId::new();
            for n in 0..300 {
                let mut t = thread(id);
                t.turns = n;
                store.commit(Event::ThreadUpserted(Box::new(t))).unwrap();
            }
            assert!(store.meta.records < 300, "the meta log was compacted");
        }
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.settings(), &settings);
        assert_eq!(*store.snapshot(Vec::new()).settings, settings);
    }

    #[test]
    fn torn_write_is_truncated_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let id = ThreadId::new();
        {
            let mut store = Store::open(dir.path()).unwrap();
            store.commit(Event::ThreadUpserted(Box::new(thread(id)))).unwrap();
        }
        let meta = dir.path().join("meta.log");
        let mut bytes = fs::read(&meta).unwrap();
        bytes.extend_from_slice(&[9, 0, 0, 0, 1, 2]); // half a frame
        fs::write(&meta, &bytes).unwrap();

        let mut store = Store::open(dir.path()).unwrap();
        assert!(store.thread(id).is_some());
        store.commit(Event::ThreadUpserted(Box::new(Thread { title: "Renamed".into(), ..thread(id) }))).unwrap();
        drop(store);
        assert_eq!(Store::open(dir.path()).unwrap().thread(id).unwrap().title, "Renamed");
    }

    #[test]
    fn rewriting_one_item_compacts_instead_of_growing() {
        let dir = tempfile::tempdir().unwrap();
        let id = ThreadId::new();
        let mut store = Store::open(dir.path()).unwrap();
        store.commit(Event::ThreadUpserted(Box::new(thread(id)))).unwrap();
        let mut item = text_item(1, "");
        for i in 0..1_000 {
            item.body = ItemBody::Assistant { text: "x".repeat(i) };
            store.commit(Event::ItemUpserted { thread: id, item: Box::new(item.clone()) }).unwrap();
        }
        let size = fs::metadata(store.timeline_path(id)).unwrap().len();
        assert!(size < 200_000, "log grew to {size} bytes");
        assert_eq!(store.load_timeline(id).unwrap().items().len(), 1);
    }

    #[test]
    fn deleting_a_thread_removes_its_log_and_unreferenced_blobs() {
        let dir = tempfile::tempdir().unwrap();
        let id = ThreadId::new();
        let mut store = Store::open(dir.path()).unwrap();
        store.commit(Event::ThreadUpserted(Box::new(thread(id)))).unwrap();
        let blob = store.put_blob("a.png".into(), "image/png".into(), b"pixels").unwrap();
        let again = store.put_blob("b.png".into(), "image/png".into(), b"pixels").unwrap();
        assert_eq!(blob.hash, again.hash, "identical content is stored once");
        let msg = Item {
            id: ItemId::new(),
            turn: 1,
            status: ItemStatus::Done,
            body: ItemBody::User(UserMessage { text: "look".into(), attachments: vec![blob.clone()], steer: false }),
        };
        store.commit(Event::ItemUpserted { thread: id, item: Box::new(msg) }).unwrap();
        assert_eq!(store.collect_garbage().unwrap(), 0);

        store.commit(Event::ThreadRemoved(id)).unwrap();
        assert!(!store.timeline_path(id).exists());
        assert_eq!(store.collect_garbage().unwrap(), 1);
        assert!(store.read_blob(&blob.hash).is_err());
    }

    #[test]
    fn drafts_survive_reopen_keep_their_blobs_and_go_with_their_thread() {
        let dir = tempfile::tempdir().unwrap();
        let id = ThreadId::new();
        let blob;
        {
            let mut store = Store::open(dir.path()).unwrap();
            store.commit(Event::ThreadUpserted(Box::new(thread(id)))).unwrap();
            blob = store.put_blob("notes.txt".into(), "text/plain".into(), b"half a thought").unwrap();
            let draft = Draft { text: "Refactor the".into(), attachments: vec![blob.clone()], updated_ms: 1 };
            store.commit(Event::DraftSaved { target: DraftTarget::Thread(id), draft: Box::new(draft) }).unwrap();
            let home = Draft { text: "New idea".into(), ..Default::default() };
            store.commit(Event::DraftSaved { target: DraftTarget::Home, draft: Box::new(home) }).unwrap();
            assert_eq!(store.collect_garbage().unwrap(), 0, "a draft keeps its attachments");
        }
        let mut store = Store::open(dir.path()).unwrap();
        assert_eq!(store.draft(DraftTarget::Thread(id)).unwrap().text, "Refactor the");
        assert_eq!(store.draft(DraftTarget::Home).unwrap().text, "New idea");
        assert_eq!(store.snapshot(Vec::new()).drafts.len(), 2);
        store.commit(Event::DraftSaved { target: DraftTarget::Home, draft: Box::new(Draft::default()) }).unwrap();
        assert!(store.draft(DraftTarget::Home).is_none(), "an empty draft removes it");
        store.commit(Event::ThreadRemoved(id)).unwrap();
        assert!(store.draft(DraftTarget::Thread(id)).is_none());
        assert_eq!(store.collect_garbage().unwrap(), 1);
        assert!(store.read_blob(&blob.hash).is_err());
    }

    #[test]
    fn streaming_leftovers_of_a_crash_load_as_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        let id = ThreadId::new();
        {
            let mut store = Store::open(dir.path()).unwrap();
            store.commit(Event::ThreadUpserted(Box::new(Thread { status: ThreadStatus::Running, ..thread(id) }))).unwrap();
            let item = Item { status: ItemStatus::Streaming, ..text_item(1, "half a reply") };
            store.commit(Event::ItemUpserted { thread: id, item: Box::new(item) }).unwrap();
        }
        let mut store = Store::open(dir.path()).unwrap();
        assert_eq!(store.load_timeline(id).unwrap().items()[0].status, ItemStatus::Cancelled);
    }

    /// A copy of a data folder written by an older Orbit (fixtures produced
    /// by that version's own `Store`).
    fn fixture(name: &str) -> tempfile::TempDir {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("threads")).unwrap();
        fs::copy(src.join("meta.log"), dir.path().join("meta.log")).unwrap();
        if let Ok(entries) = fs::read_dir(src.join("threads")) {
            for entry in entries.flatten() {
                fs::copy(entry.path(), dir.path().join("threads").join(entry.file_name())).unwrap();
            }
        }
        dir
    }

    #[test]
    fn the_first_data_folder_layout_opens_and_is_migrated_once() {
        let dir = fixture("store-v1");
        let codex = ThreadId(0x2222);
        {
            let mut store = Store::open(dir.path()).unwrap();
            let thread = store.thread(codex).expect("thread survives").clone();
            assert_eq!(thread.title, "Fix the build");
            assert_eq!(thread.config.provider, ProviderKind::Codex);
            assert_eq!(thread.config.instance_id(), "codex");
            assert_eq!(thread.config.model, "gpt-5.1-codex");
            assert_eq!((thread.config.mode, thread.config.access), (Mode::Plan, Access::AutoEdit));
            assert_eq!(thread.session.as_deref(), Some("thr_abc"));
            assert_eq!(store.thread(ThreadId(0x3333)).unwrap().config.provider, ProviderKind::Claude);
            assert_eq!(store.defaults().provider, ProviderKind::Codex);
            assert_eq!(store.projects()[0].base_branch.as_deref(), Some("main"));
            let items = store.load_timeline(codex).unwrap().items().to_vec();
            assert_eq!(items.len(), 3);
            assert_eq!(items[1].body, ItemBody::Assistant { text: "Fixed the missing import.".into() });
            // New writes land in the migrated log.
            let config = ThreadConfig { instance: "codex-work".into(), ..thread.config.clone() };
            store.commit(Event::ThreadUpserted(Box::new(Thread { config, ..thread }))).unwrap();
        }
        let bytes = fs::read(dir.path().join("meta.log")).unwrap();
        let first = FrameReader::new(&bytes).next_payload().unwrap().to_vec();
        assert!(first.starts_with(META_MAGIC), "the migrated log starts with its version");
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.thread(codex).unwrap().config.instance_id(), "codex-work");
        assert_eq!(store.threads().count(), 2);
    }

    #[test]
    fn a_data_folder_with_settings_but_no_version_opens() {
        let dir = fixture("store-v1-settings");
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.thread(ThreadId(0x4444)).unwrap().config.provider, ProviderKind::Codex);
        let settings = store.settings();
        assert_eq!(settings.appearance.ui_scale, 125);
        assert_eq!(settings.provider(ProviderKind::Claude).binary, "/opt/claude");
        assert_eq!(settings.provider(ProviderKind::Codex).env[0].value, "/home/u/.codex-work");
        assert_eq!(settings.provider(ProviderKind::Claude).env[0].name, "CLAUDE_CONFIG_DIR");
    }

    #[test]
    fn a_version_2_data_folder_opens_and_then_keeps_drafts() {
        // Written by the schema-2 Store (provider instances, before drafts).
        let dir = fixture("store-v2");
        let id = ThreadId(0x5555);
        {
            let mut store = Store::open(dir.path()).unwrap();
            let thread = store.thread(id).expect("thread survives").clone();
            assert_eq!(thread.title, "Queue the migration");
            assert_eq!((thread.config.provider, thread.config.instance_id()), (ProviderKind::Codex, "codex-work"));
            assert_eq!(thread.session.as_deref(), Some("thr_v2"));
            assert_eq!(store.defaults().instance_id(), "codex-work");
            assert_eq!(store.settings().appearance.ui_scale, 110);
            assert_eq!(store.projects()[0].name, "orbit");
            let items = store.load_timeline(id).unwrap().items().to_vec();
            assert_eq!(items.len(), 1);
            assert!(matches!(&items[0].body, ItemBody::User(m) if m.text == "Plan the migration"));
            assert!(store.draft(DraftTarget::Thread(id)).is_none());
            let draft = Draft { text: "Then the rollback".into(), ..Default::default() };
            store.commit(Event::DraftSaved { target: DraftTarget::Thread(id), draft: Box::new(draft) }).unwrap();
        }
        let bytes = fs::read(dir.path().join("meta.log")).unwrap();
        let first = FrameReader::new(&bytes).next_payload().unwrap().to_vec();
        assert_eq!(header_version(&first), legacy::META_VERSION, "migrated to the current schema");
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.draft(DraftTarget::Thread(id)).unwrap().text, "Then the rollback");
        assert_eq!(store.thread(id).unwrap().config.instance_id(), "codex-work");
    }

    /// Schema 4 was the last written in bitcode, through the current types.
    /// Opening such a folder migrates the meta log and, when a thread is
    /// opened, its timeline to wincode (schema 5); both read back after.
    #[test]
    fn a_version_4_bitcode_data_folder_migrates_to_wincode() {
        let dir = tempfile::tempdir().unwrap();
        let (project, id) = (ProjectId(0x44), ThreadId(0x4444));
        let thread = Thread { id, project: Some(project), title: "Bitcode era".into(), turns: 2, ..Default::default() };
        let mut settings = Settings::default();
        settings.appearance.ui_scale = 125;
        let mut meta = Vec::new();
        let mut header = META_MAGIC.to_vec();
        header.extend_from_slice(&legacy::LAST_BITCODE_VERSION.to_le_bytes());
        write_raw_frame(&header, &mut meta);
        let project_record = Project {
            id: project,
            name: "orbit".into(),
            path: "C:/code/orbit".into(),
            order: 0,
            scripts: Default::default(),
            base_branch: Some("main".into()),
        };
        for (seq, event) in [
            (1, Event::ProjectUpserted(Box::new(project_record))),
            (2, Event::ThreadUpserted(Box::new(thread))),
            (3, Event::SettingsChanged(Box::new(settings))),
        ] {
            write_raw_frame(&bitcode::encode(&Envelope { seq, event }), &mut meta);
        }
        fs::write(dir.path().join("meta.log"), &meta).unwrap();
        fs::create_dir_all(dir.path().join("threads")).unwrap();
        let mut timeline = Vec::new();
        let mut header = TIMELINE_MAGIC.to_vec();
        header.extend_from_slice(&legacy::LAST_BITCODE_VERSION.to_le_bytes());
        write_raw_frame(&header, &mut timeline);
        let item = Item {
            id: ItemId(0x99),
            turn: 1,
            status: ItemStatus::Done,
            body: ItemBody::Assistant { text: "Written by bitcode".into() },
        };
        write_raw_frame(&bitcode::encode(&TimelineRecord::Upsert(item.clone())), &mut timeline);
        let log = dir.path().join("threads").join(format!("{id}.log"));
        fs::write(&log, &timeline).unwrap();

        let check = |store: &mut Store| {
            assert_eq!(store.thread(id).unwrap().title, "Bitcode era");
            assert_eq!(store.projects()[0].base_branch.as_deref(), Some("main"));
            assert_eq!(store.settings().appearance.ui_scale, 125);
            assert_eq!(store.load_timeline(id).unwrap().items(), std::slice::from_ref(&item));
        };
        check(&mut Store::open(dir.path()).unwrap());
        for path in [dir.path().join("meta.log"), log.clone()] {
            let bytes = fs::read(&path).unwrap();
            let first = FrameReader::new(&bytes).next_payload().unwrap().to_vec();
            assert_eq!(header_version(&first), legacy::META_VERSION, "{} migrated", path.display());
        }
        check(&mut Store::open(dir.path()).unwrap());
    }

    #[test]
    fn a_version_3_data_folder_keeps_its_questions_and_gains_option_descriptions() {
        use legacy::v3;
        // Written the way the schema-3 Store wrote it: a versioned meta log,
        // and a timeline log without a header.
        let dir = tempfile::tempdir().unwrap();
        let id = ThreadId(0x6666);
        let question = v3::AgentRequest {
            id: RequestId(0x77),
            provider_ref: "toolu_q".into(),
            kind: v3::RequestKind::Question {
                question: "Which database?".into(),
                options: vec!["Postgres".into(), "SQLite".into()],
                multi_select: true,
            },
            resolution: None,
        };
        let t = Thread::default();
        let thread = v3::Thread {
            id,
            project: None,
            title: "Pick a database".into(),
            config: t.config,
            workspace: t.workspace,
            status: ThreadStatus::Waiting,
            created_ms: 1,
            updated_ms: 2,
            session: Some("s".into()),
            parent: None,
            archived: false,
            pinned: false,
            unread: false,
            usage: t.usage,
            turns: 1,
            queued: 0,
            pending: vec![v3::PendingRequest { item: ItemId(0x88), request: question.clone(), created_ms: 3 }],
            settled_ms: None,
            snoozed_until: None,
            woke: false,
        };
        let mut meta = Vec::new();
        let mut header = META_MAGIC.to_vec();
        header.extend_from_slice(&3u32.to_le_bytes());
        write_raw_frame(&header, &mut meta);
        write_raw_frame(&bitcode::encode(&v3::Envelope { seq: 1, event: v3::Event::ThreadUpserted(thread) }), &mut meta);
        fs::write(dir.path().join("meta.log"), &meta).unwrap();
        fs::create_dir_all(dir.path().join("threads")).unwrap();
        let mut timeline = Vec::new();
        let user = v3::Item {
            id: ItemId(0x87),
            turn: 1,
            status: ItemStatus::Done,
            body: v3::ItemBody::User(UserMessage { text: "Set up storage".into(), ..Default::default() }),
        };
        write_raw_frame(&bitcode::encode(&TimelineRecordV3::Upsert(user)), &mut timeline);
        let asked = v3::Item { id: ItemId(0x88), turn: 1, status: ItemStatus::Done, body: v3::ItemBody::Request(question) };
        write_raw_frame(&bitcode::encode(&TimelineRecordV3::Upsert(asked)), &mut timeline);
        let log = dir.path().join("threads").join(format!("{id}.log"));
        fs::write(&log, &timeline).unwrap();

        let labels = |kind: &RequestKind| match kind {
            RequestKind::Question { options, multi_select: true, .. } => {
                options.iter().map(|o| (o.label.clone(), o.description.clone())).collect::<Vec<_>>()
            }
            other => panic!("{other:?}"),
        };
        let expected = vec![("Postgres".to_owned(), String::new()), ("SQLite".to_owned(), String::new())];
        // Search reads the old log without loading it.
        let scanned = Store::scan_timeline(dir.path(), id);
        assert!(matches!(&scanned[1].body, ItemBody::Request(r) if labels(&r.kind) == expected));
        {
            let mut store = Store::open(dir.path()).unwrap();
            assert_eq!(labels(&store.thread(id).unwrap().pending[0].request.kind), expected);
            let items = store.load_timeline(id).unwrap().items().to_vec();
            assert_eq!(items.len(), 2);
            assert!(matches!(&items[1].body, ItemBody::Request(r) if labels(&r.kind) == expected));
            // A new item with a described option lands in the migrated log.
            let kind = RequestKind::Question {
                question: "Which cache?".into(),
                options: vec![QuestionOption::new("Redis", "In memory, shared across workers")],
                multi_select: true,
            };
            let request = AgentRequest { id: RequestId(0x99), provider_ref: "toolu_r".into(), kind, resolution: None };
            let item = Item { id: ItemId(0x89), turn: 1, status: ItemStatus::Done, body: ItemBody::Request(request) };
            store.commit(Event::ItemUpserted { thread: id, item: Box::new(item) }).unwrap();
        }
        let bytes = fs::read(&log).unwrap();
        assert_eq!(read_timeline(&bytes).0, legacy::META_VERSION, "the timeline log is migrated and versioned");
        let bytes = fs::read(dir.path().join("meta.log")).unwrap();
        assert_eq!(header_version(FrameReader::new(&bytes).next_payload().unwrap()), legacy::META_VERSION);
        let mut store = Store::open(dir.path()).unwrap();
        assert_eq!(labels(&store.thread(id).unwrap().pending[0].request.kind), expected);
        let items = store.load_timeline(id).unwrap().items().to_vec();
        assert_eq!(items.len(), 3);
        assert!(matches!(&items[2].body, ItemBody::Request(r) if labels(&r.kind) == vec![("Redis".to_owned(), "In memory, shared across workers".to_owned())]));
        assert_eq!(Store::scan_timeline(dir.path(), id).len(), 3);
    }

    #[test]
    fn a_log_from_a_newer_orbit_is_refused_not_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let mut header = META_MAGIC.to_vec();
        header.extend_from_slice(&(legacy::META_VERSION + 1).to_le_bytes());
        let mut bytes = Vec::new();
        write_raw_frame(&header, &mut bytes);
        fs::write(dir.path().join("meta.log"), &bytes).unwrap();
        assert!(Store::open(dir.path()).is_err());
        assert_eq!(fs::read(dir.path().join("meta.log")).unwrap(), bytes);
    }

    #[test]
    fn a_data_folder_has_one_owner() {
        let dir = tempfile::tempdir().unwrap();
        let first = Store::open(dir.path()).unwrap();
        let second = Store::open(dir.path());
        assert!(second.is_err(), "a second engine must not share the folder");
        drop(first);
        assert!(Store::open(dir.path()).is_ok(), "the lock is released with the store");
    }
}
