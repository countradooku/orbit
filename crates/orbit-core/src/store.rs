//! Durable state that stays small.
//!
//! Layout of the data directory:
//!
//! ```text
//! meta.log            projects, threads, defaults (durable envelopes)
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
use orbit_proto::*;

/// A record in a thread's log.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum TimelineRecord {
    Upsert(Item),
    Remove(ItemId),
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
}

impl Log {
    /// Opens `path`, returning the log and every valid record in it. A torn or
    /// corrupted tail (crash mid-write) is truncated away.
    fn open<T: for<'de> Decode<'de>>(path: &Path) -> io::Result<(Self, Vec<T>)> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let mut bytes = Vec::new();
        if let Ok(mut f) = File::open(path) {
            f.read_to_end(&mut bytes)?;
        }
        let mut reader = FrameReader::new(&bytes);
        let mut out = Vec::new();
        loop {
            match reader.read::<T>() {
                Ok(record) => out.push(record),
                Err(FrameError::Incomplete) if reader.position() == bytes.len() => break,
                Err(err) => {
                    log::warn!(
                        "{}: dropping {} bytes after a bad record ({err})",
                        path.display(),
                        bytes.len() - reader.position()
                    );
                    break;
                }
            }
        }
        let valid = reader.position() as u64;
        if valid < bytes.len() as u64 {
            // An append-only handle cannot truncate on Windows; use a writer.
            OpenOptions::new().write(true).open(path)?.set_len(valid)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let records = out.len();
        Ok((
            Self { path: path.to_owned(), file: Some(file), records, buf: Vec::with_capacity(4096), codec: Codec::new() },
            out,
        ))
    }

    fn append<T: Encode>(&mut self, record: &T) -> io::Result<()> {
        self.buf.clear();
        self.codec.write_frame(record, &mut self.buf);
        self.file.as_mut().expect("log file is open").write_all(&self.buf)?;
        self.records += 1;
        Ok(())
    }

    /// Atomically replaces the log's contents with `records`.
    fn rewrite<'a, T: Encode + 'a>(&mut self, records: impl IntoIterator<Item = &'a T>) -> io::Result<()> {
        let tmp = self.path.with_extension("log.tmp");
        let mut out = Vec::new();
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
    open: HashMap<ThreadId, OpenTimeline>,
}

impl Store {
    pub fn open(dir: impl Into<PathBuf>) -> io::Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(dir.join("threads"))?;
        fs::create_dir_all(dir.join("blobs"))?;
        let (meta, records) = Log::open::<Envelope>(&dir.join("meta.log"))?;
        let mut store = Self {
            dir,
            meta,
            epoch: next_raw_id(),
            seq: 0,
            projects: Vec::new(),
            threads: HashMap::new(),
            defaults: ThreadConfig::default(),
            open: HashMap::new(),
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

    pub fn snapshot(&self, providers: Vec<ProviderInfo>) -> Snapshot {
        let mut threads: Vec<Thread> = self.threads.values().cloned().collect();
        threads.sort_by_key(|t| std::cmp::Reverse(t.updated_ms));
        Snapshot {
            epoch: self.epoch,
            seq: self.seq,
            projects: self.projects.clone(),
            threads,
            providers,
            defaults: self.defaults.clone(),
        }
    }

    fn apply_meta(&mut self, event: Event) {
        match event {
            Event::ProjectUpserted(project) => match self.projects.iter_mut().find(|p| p.id == project.id) {
                Some(slot) => *slot = project,
                None => {
                    self.projects.push(project);
                    self.projects.sort_by_key(|p| p.order);
                }
            },
            Event::ProjectRemoved(id) => self.projects.retain(|p| p.id != id),
            Event::ThreadUpserted(thread) => {
                self.threads.insert(thread.id, thread);
            }
            Event::ThreadRemoved(id) => {
                self.threads.remove(&id);
            }
            Event::DefaultsChanged(config) => self.defaults = config,
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
                self.timeline_log(thread)?.append(&TimelineRecord::Upsert(item.clone()))?;
                if let Some(open) = self.open.get_mut(&thread) {
                    open.timeline.upsert(item);
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
            let (log, records) = Log::open::<TimelineRecord>(&self.timeline_path(id))?;
            let mut timeline = Timeline::default();
            for record in records {
                timeline.apply(record);
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
        let mut reader = FrameReader::new(&bytes);
        let mut timeline = Timeline::default();
        while let Ok(record) = reader.read::<TimelineRecord>() {
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
        let live = self.projects.len() + self.threads.len() + 1;
        if self.meta.records > 256 && self.meta.records > live * 4 {
            let seq = self.seq;
            let mut records = Vec::with_capacity(live);
            for p in &self.projects {
                records.push(Envelope { seq, event: Event::ProjectUpserted(p.clone()) });
            }
            for t in self.threads.values() {
                records.push(Envelope { seq, event: Event::ThreadUpserted(t.clone()) });
            }
            records.push(Envelope { seq, event: Event::DefaultsChanged(self.defaults.clone()) });
            self.meta.rewrite(records.iter())?;
        }
        Ok(())
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
        let hash = blake3::hash(data).to_hex().to_string();
        let path = self.blob_path(&hash);
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

    /// Deletes blobs that no remaining timeline references. Runs after a
    /// thread is deleted; scans logs without keeping them loaded.
    pub fn collect_garbage(&mut self) -> io::Result<usize> {
        let mut referenced = std::collections::HashSet::new();
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
            store.commit(Event::ThreadUpserted(thread(id))).unwrap();
            store.commit(Event::ItemUpserted { thread: id, item: item.clone() }).unwrap();
            store.unload_timeline(id);
            assert!(store.timeline(id).is_none());
        }
        let mut store = Store::open(dir.path()).unwrap();
        assert_eq!(store.thread(id).unwrap().title, "Fix the build");
        assert!(store.timeline(id).is_none(), "timeline must not load at startup");
        assert_eq!(store.load_timeline(id).unwrap().items(), &[item]);
    }

    #[test]
    fn torn_write_is_truncated_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let id = ThreadId::new();
        {
            let mut store = Store::open(dir.path()).unwrap();
            store.commit(Event::ThreadUpserted(thread(id))).unwrap();
        }
        let meta = dir.path().join("meta.log");
        let mut bytes = fs::read(&meta).unwrap();
        bytes.extend_from_slice(&[9, 0, 0, 0, 1, 2]); // half a frame
        fs::write(&meta, &bytes).unwrap();

        let mut store = Store::open(dir.path()).unwrap();
        assert!(store.thread(id).is_some());
        store.commit(Event::ThreadUpserted(Thread { title: "Renamed".into(), ..thread(id) })).unwrap();
        drop(store);
        assert_eq!(Store::open(dir.path()).unwrap().thread(id).unwrap().title, "Renamed");
    }

    #[test]
    fn rewriting_one_item_compacts_instead_of_growing() {
        let dir = tempfile::tempdir().unwrap();
        let id = ThreadId::new();
        let mut store = Store::open(dir.path()).unwrap();
        store.commit(Event::ThreadUpserted(thread(id))).unwrap();
        let mut item = text_item(1, "");
        for i in 0..1_000 {
            item.body = ItemBody::Assistant { text: "x".repeat(i) };
            store.commit(Event::ItemUpserted { thread: id, item: item.clone() }).unwrap();
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
        store.commit(Event::ThreadUpserted(thread(id))).unwrap();
        let blob = store.put_blob("a.png".into(), "image/png".into(), b"pixels").unwrap();
        let again = store.put_blob("b.png".into(), "image/png".into(), b"pixels").unwrap();
        assert_eq!(blob.hash, again.hash, "identical content is stored once");
        let msg = Item {
            id: ItemId::new(),
            turn: 1,
            status: ItemStatus::Done,
            body: ItemBody::User(UserMessage { text: "look".into(), attachments: vec![blob.clone()], steer: false }),
        };
        store.commit(Event::ItemUpserted { thread: id, item: msg }).unwrap();
        assert_eq!(store.collect_garbage().unwrap(), 0);

        store.commit(Event::ThreadRemoved(id)).unwrap();
        assert!(!store.timeline_path(id).exists());
        assert_eq!(store.collect_garbage().unwrap(), 1);
        assert!(store.read_blob(&blob.hash).is_err());
    }

    #[test]
    fn streaming_leftovers_of_a_crash_load_as_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        let id = ThreadId::new();
        {
            let mut store = Store::open(dir.path()).unwrap();
            store.commit(Event::ThreadUpserted(Thread { status: ThreadStatus::Running, ..thread(id) })).unwrap();
            let item = Item { status: ItemStatus::Streaming, ..text_item(1, "half a reply") };
            store.commit(Event::ItemUpserted { thread: id, item }).unwrap();
        }
        let mut store = Store::open(dir.path()).unwrap();
        assert_eq!(store.load_timeline(id).unwrap().items()[0].status, ItemStatus::Cancelled);
    }
}
