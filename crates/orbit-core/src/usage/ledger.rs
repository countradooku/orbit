//! The usage ledger: one [`TurnUsage`] per finished turn, appended to
//! `<data>/usage.log`, and the stats and CSV built from it.
//!
//! Turn totals already live in each thread's timeline (`TurnEnd` items), but
//! reading every timeline to draw a chart is what made T3 Code's usage page
//! slow. The ledger keeps the few numbers stats need, about 60 bytes a turn,
//! in one small file that loads at startup. A data folder from before the
//! ledger is backfilled once from its timelines, off the engine thread;
//! until that lands, new turns are held in memory, so a crash mid-way
//! simply backfills again.
//!
//! Records survive their thread's deletion: the tokens were spent.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};

use jiff::tz::TimeZone;
use orbit_proto::*;

const FILE: &str = "usage.log";
const MAGIC: &[u8] = b"orbit/usage\0";
const VERSION: u32 = 1;
/// Days listed in stats at most (the totals still cover the whole range).
pub const MAX_DAYS: usize = 400;
/// Threads listed in stats at most.
pub const MAX_THREADS: usize = 50;

pub struct Ledger {
    path: PathBuf,
    rows: Vec<TurnUsage>,
    /// `None` until the backfill has been written (no file yet).
    file: Option<File>,
}

fn header() -> Vec<u8> {
    let mut h = MAGIC.to_vec();
    h.extend_from_slice(&VERSION.to_le_bytes());
    h
}

impl Ledger {
    /// Opens the ledger in `dir`. Returns whether it has to be backfilled
    /// from the timelines first (see [`Ledger::backfilled`]).
    pub fn open(dir: &Path) -> (Self, bool) {
        let path = dir.join(FILE);
        let mut bytes = Vec::new();
        let exists = match File::open(&path) {
            Ok(mut f) => f.read_to_end(&mut bytes).is_ok(),
            Err(_) => false,
        };
        if !exists {
            return (Self { path, rows: Vec::new(), file: None }, true);
        }
        let mut reader = FrameReader::new(&bytes);
        let header_ok = reader.next_payload().is_ok_and(|h| h == header().as_slice());
        let mut rows = Vec::new();
        if header_ok {
            while let Ok(row) = reader.read::<TurnUsage>() {
                rows.push(row);
            }
        } else {
            log::warn!("{}: not a usage ledger; starting a new one", path.display());
            reader = FrameReader::new(&[]);
        }
        let valid = reader.position() as u64;
        let file = (|| {
            if valid < bytes.len() as u64 || !header_ok {
                // A torn tail (crash mid-write) is cut off.
                OpenOptions::new().write(true).open(&path)?.set_len(valid)?;
            }
            let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
            if valid == 0 {
                let mut frame = Vec::new();
                write_raw_frame(&header(), &mut frame);
                file.write_all(&frame)?;
            }
            io::Result::Ok(file)
        })();
        let file = file.inspect_err(|e| log::warn!("{}: {e}", path.display())).ok();
        (Self { path, rows, file }, false)
    }

    pub fn rows(&self) -> &[TurnUsage] {
        &self.rows
    }

    /// Records a finished turn.
    pub fn append(&mut self, row: TurnUsage) {
        if let Some(file) = &mut self.file {
            let mut frame = Vec::with_capacity(96);
            write_frame(&row, &mut frame);
            if let Err(e) = file.write_all(&frame) {
                log::warn!("{}: {e}", self.path.display());
            }
        }
        self.rows.push(row);
    }

    /// Whether the ledger still waits for its backfill.
    pub fn backfilling(&self) -> bool {
        self.file.is_none()
    }

    /// Installs the backfill (turns that ended before `started_ms`) under
    /// the turns recorded since, and writes the whole ledger.
    pub fn backfilled(&mut self, mut rows: Vec<TurnUsage>, started_ms: u64) -> io::Result<()> {
        rows.retain(|r| r.ended_ms < started_ms);
        rows.sort_by_key(|r| r.ended_ms);
        rows.append(&mut self.rows);
        self.rows = rows;
        let mut out = Vec::with_capacity(64 + self.rows.len() * 80);
        write_raw_frame(&header(), &mut out);
        for row in &self.rows {
            write_frame(row, &mut out);
        }
        let tmp = self.path.with_extension("log.tmp");
        {
            let mut f = File::create(&tmp)?;
            f.write_all(&out)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &self.path)?;
        self.file = Some(OpenOptions::new().append(true).open(&self.path)?);
        Ok(())
    }
}

/// The turns a data folder's timelines hold, for the one-time backfill.
/// `threads` gives each thread's project and config; runs off the engine
/// thread.
pub fn backfill(dir: &Path, threads: &[(ThreadId, Option<ProjectId>, ThreadConfig)]) -> Vec<TurnUsage> {
    let mut rows = Vec::new();
    for (id, project, config) in threads {
        for item in crate::store::Store::scan_timeline(dir, *id) {
            if let ItemBody::TurnEnd(summary) = &item.body
                && summary.usage != Usage::default()
            {
                rows.push(TurnUsage {
                    ended_ms: item.id.created_ms(),
                    thread: *id,
                    project: *project,
                    kind: config.provider,
                    instance: config.instance_id().to_owned(),
                    model: config.model.clone(),
                    usage: summary.usage,
                });
            }
        }
    }
    rows
}

// ── Queries ──────────────────────────────────────────────────────────────

/// Interface copy for the keys stats group by.
#[derive(Default)]
pub struct Labels {
    pub threads: HashMap<ThreadId, String>,
    pub projects: HashMap<ProjectId, String>,
    /// Instance id → display name.
    pub instances: HashMap<String, String>,
    /// Model slug → display name.
    pub models: HashMap<String, String>,
}

impl Labels {
    fn thread(&self, id: ThreadId) -> String {
        self.threads.get(&id).cloned().unwrap_or_else(|| "Deleted thread".into())
    }

    fn project(&self, id: Option<ProjectId>) -> (String, String) {
        match id {
            Some(id) => (id.to_string(), self.projects.get(&id).cloned().unwrap_or_else(|| "Removed project".into())),
            None => (String::new(), "No project".into()),
        }
    }

    fn instance(&self, row: &TurnUsage) -> String {
        self.instances.get(&row.instance).cloned().unwrap_or_else(|| {
            if row.instance == row.kind.id() { row.kind.label().to_owned() } else { row.instance.clone() }
        })
    }

    fn model(&self, slug: &str) -> String {
        match slug {
            "" => "Default model".into(),
            slug => self.models.get(slug).cloned().unwrap_or_else(|| slug.to_owned()),
        }
    }
}

fn matches(row: &TurnUsage, query: &UsageQuery, to: u64) -> bool {
    row.ended_ms >= query.from_ms
        && row.ended_ms < to
        && query.project.is_none_or(|p| row.project == Some(p))
        && query.instance.as_deref().is_none_or(|i| row.instance == i)
}

fn time_zone(name: &str) -> TimeZone {
    if name.is_empty() {
        return TimeZone::system();
    }
    TimeZone::get(name).unwrap_or_else(|_| TimeZone::system())
}

fn local_date(tz: &TimeZone, ms: u64) -> jiff::civil::Date {
    jiff::Timestamp::from_millisecond(ms as i64).map(|t| tz.to_datetime(t).date()).unwrap_or_default()
}

fn sorted(groups: HashMap<String, UsageGroup>) -> Vec<UsageGroup> {
    let mut groups: Vec<UsageGroup> = groups.into_values().collect();
    groups.sort_by(|a, b| b.totals.tokens().cmp(&a.totals.tokens()).then_with(|| a.label.cmp(&b.label)));
    groups
}

fn bump(groups: &mut HashMap<String, UsageGroup>, key: String, label: impl FnOnce() -> String, usage: &Usage) {
    groups.entry(key.clone()).or_insert_with(|| UsageGroup { key, label: label(), totals: UsageTotals::default() }).totals.add(usage);
}

/// Totals for `query`, by day, model, account, project and thread.
pub fn stats(rows: &[TurnUsage], query: &UsageQuery, labels: &Labels, now_ms: u64) -> UsageStats {
    let to = if query.to_ms == 0 { now_ms + 1 } else { query.to_ms };
    let tz = time_zone(&query.tz);
    let mut out = UsageStats::default();
    let (mut models, mut accounts, mut projects, mut threads) =
        (HashMap::new(), HashMap::new(), HashMap::new(), HashMap::new());
    let mut days: HashMap<jiff::civil::Date, UsageTotals> = HashMap::new();
    let mut first: Option<u64> = None;
    for row in rows.iter().filter(|r| matches(r, query, to)) {
        out.totals.add(&row.usage);
        first = Some(first.map_or(row.ended_ms, |f| f.min(row.ended_ms)));
        days.entry(local_date(&tz, row.ended_ms)).or_default().add(&row.usage);
        bump(&mut models, row.model.clone(), || labels.model(&row.model), &row.usage);
        bump(&mut accounts, row.instance.clone(), || labels.instance(row), &row.usage);
        let (key, label) = labels.project(row.project);
        bump(&mut projects, key, || label, &row.usage);
        bump(&mut threads, row.thread.to_string(), || labels.thread(row.thread), &row.usage);
    }
    // Every day of the range, empty ones included, so a chart's bars are
    // evenly spaced in time.
    let start = if query.from_ms > 0 { query.from_ms } else { first.unwrap_or(now_ms) };
    let (start, end) = (local_date(&tz, start), local_date(&tz, to.saturating_sub(1).max(start)));
    let mut day = end;
    let mut list = Vec::new();
    while day >= start && list.len() < MAX_DAYS {
        list.push(DayUsage { date: day.to_string(), totals: days.get(&day).copied().unwrap_or_default() });
        match day.yesterday() {
            Ok(previous) => day = previous,
            Err(_) => break,
        }
    }
    list.reverse();
    out.days = list;
    out.models = sorted(models);
    out.accounts = sorted(accounts);
    out.projects = sorted(projects);
    out.threads = sorted(threads);
    out.threads.truncate(MAX_THREADS);
    out
}

/// The threads that used `instance` most since `since_ms`, by tokens.
pub fn consumers(rows: &[TurnUsage], instance: &str, since_ms: u64, labels: &Labels, limit: usize) -> Vec<Consumer> {
    let mut by_thread: HashMap<ThreadId, (u64, u64)> = HashMap::new();
    for row in rows.iter().rev().take_while(|r| r.ended_ms >= since_ms).filter(|r| r.instance == instance) {
        let entry = by_thread.entry(row.thread).or_default();
        entry.0 += row.usage.input_tokens + row.usage.output_tokens + row.usage.cache_read_tokens + row.usage.cache_write_tokens;
        entry.1 += row.usage.cost_micros;
    }
    let mut out: Vec<Consumer> = by_thread
        .into_iter()
        .map(|(thread, (tokens, cost_micros))| Consumer { thread, title: labels.thread(thread), tokens, cost_micros })
        .collect();
    out.sort_by(|a, b| b.tokens.cmp(&a.tokens).then(a.thread.cmp(&b.thread)));
    out.truncate(limit);
    out
}

/// The query's turns as CSV (RFC 4180), oldest first, times in the query's
/// time zone.
pub fn csv(rows: &[TurnUsage], query: &UsageQuery, labels: &Labels, now_ms: u64) -> String {
    let to = if query.to_ms == 0 { now_ms + 1 } else { query.to_ms };
    let tz = time_zone(&query.tz);
    let mut out = String::from(
        "date,time,thread_id,thread,project,agent,account,model,input_tokens,output_tokens,cache_read_tokens,cache_write_tokens,cost_usd\r\n",
    );
    for row in rows.iter().filter(|r| matches(r, query, to)) {
        let (date, time) = jiff::Timestamp::from_millisecond(row.ended_ms as i64)
            .map(|t| {
                let dt = tz.to_datetime(t);
                (dt.date().to_string(), format!("{:02}:{:02}:{:02}", dt.hour(), dt.minute(), dt.second()))
            })
            .unwrap_or_default();
        let u = &row.usage;
        let fields = [
            date,
            time,
            row.thread.to_string(),
            labels.thread(row.thread),
            labels.project(row.project).1,
            row.kind.label().to_owned(),
            labels.instance(row),
            labels.model(&row.model),
            u.input_tokens.to_string(),
            u.output_tokens.to_string(),
            u.cache_read_tokens.to_string(),
            u.cache_write_tokens.to_string(),
            format!("{}.{:06}", u.cost_micros / 1_000_000, u.cost_micros % 1_000_000),
        ];
        let line: Vec<String> = fields.iter().map(|f| csv_field(f)).collect();
        out.push_str(&line.join(","));
        out.push_str("\r\n");
    }
    out
}

fn csv_field(field: &str) -> String {
    // A leading = + - @ would run as a formula in a spreadsheet.
    let formula = field.starts_with(['=', '+', '@']) || (field.starts_with('-') && field.parse::<f64>().is_err());
    let field = if formula { format!("'{field}") } else { field.to_owned() };
    if field.contains([',', '"', '\n', '\r']) { format!("\"{}\"", field.replace('"', "\"\"")) } else { field }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 86_400_000;
    /// 2026-11-20 12:00 UTC.
    const NOON: u64 = 1_795_176_000_000;

    fn row(ended_ms: u64, thread: u64, instance: &str, model: &str, tokens: u64, cost: u64) -> TurnUsage {
        TurnUsage {
            ended_ms,
            thread: ThreadId(thread),
            project: Some(ProjectId(1)),
            kind: if instance.starts_with("codex") { ProviderKind::Codex } else { ProviderKind::Claude },
            instance: instance.into(),
            model: model.into(),
            usage: Usage { input_tokens: tokens, output_tokens: tokens / 10, cost_micros: cost, ..Default::default() },
        }
    }

    fn labels() -> Labels {
        let mut l = Labels::default();
        l.threads.insert(ThreadId(7), "Fix the login, \"fast\"".into());
        l.projects.insert(ProjectId(1), "api".into());
        l.instances.insert("claude-work".into(), "Claude (work)".into());
        l.models.insert("opus".into(), "Opus 5.5".into());
        l
    }

    #[test]
    fn stats_group_by_day_model_account_project_and_thread() {
        let rows = vec![
            row(NOON - 2 * DAY, 7, "claude", "opus", 1_000, 50_000),
            row(NOON, 7, "claude-work", "opus", 2_000, 100_000),
            row(NOON + 3_600_000, 8, "codex", "gpt-5-codex", 4_000, 0),
            row(NOON + 10 * DAY, 8, "codex", "gpt-5-codex", 9_000, 0),
        ];
        let query = UsageQuery { from_ms: NOON - 3 * DAY, to_ms: NOON + DAY, tz: "UTC".into(), ..Default::default() };
        let stats = stats(&rows, &query, &labels(), NOON + 20 * DAY);
        assert_eq!(stats.totals.turns, 3, "the row after the range is left out");
        assert_eq!(stats.totals.input_tokens, 7_000);
        assert_eq!(stats.totals.cost_micros, 150_000);
        let days: Vec<(&str, u32)> = stats.days.iter().map(|d| (d.date.as_str(), d.totals.turns)).collect();
        assert_eq!(days, [("2026-11-17", 0), ("2026-11-18", 1), ("2026-11-19", 0), ("2026-11-20", 2), ("2026-11-21", 0)]);
        assert_eq!(stats.models[0].label, "gpt-5-codex", "most tokens first");
        assert_eq!(stats.models[1].label, "Opus 5.5");
        let accounts: Vec<&str> = stats.accounts.iter().map(|a| a.label.as_str()).collect();
        assert_eq!(accounts, ["Codex", "Claude (work)", "Claude Code"]);
        assert_eq!(stats.projects.len(), 1);
        assert_eq!(stats.projects[0].label, "api");
        assert_eq!(stats.threads[1].label, "Fix the login, \"fast\"");
        assert_eq!(stats.threads[0].label, "Deleted thread");

        let only_work = UsageQuery { instance: Some("claude-work".into()), ..query.clone() };
        assert_eq!(super::stats(&rows, &only_work, &labels(), NOON).totals.turns, 1);
    }

    #[test]
    fn days_follow_the_time_zone() {
        // 23:30 UTC is already the next day in Bucharest.
        let late = NOON + 11 * 3_600_000 + 30 * 60_000;
        let rows = vec![row(late, 7, "claude", "opus", 10, 0)];
        let at = |tz: &str| {
            let q = UsageQuery { from_ms: late - 1, to_ms: late + 1, tz: tz.into(), ..Default::default() };
            stats(&rows, &q, &labels(), late).days.iter().map(|d| d.date.clone()).collect::<Vec<_>>()
        };
        assert_eq!(at("UTC"), ["2026-11-20"]);
        assert_eq!(at("Europe/Bucharest"), ["2026-11-21"]);
    }

    #[test]
    fn top_consumers_cover_the_window_of_one_account() {
        let rows = vec![
            row(NOON - DAY, 9, "claude", "opus", 99_000, 0),
            row(NOON, 7, "claude", "opus", 1_000, 10),
            row(NOON + 1, 8, "claude", "opus", 3_000, 20),
            row(NOON + 2, 7, "claude", "opus", 1_500, 30),
            row(NOON + 3, 6, "codex", "gpt", 50_000, 0),
        ];
        let top = consumers(&rows, "claude", NOON, &labels(), 5);
        let got: Vec<(u64, u64, u64)> = top.iter().map(|c| (c.thread.0, c.tokens, c.cost_micros)).collect();
        assert_eq!(got, [(8, 3_300, 20), (7, 2_750, 40)]);
    }

    #[test]
    fn csv_quotes_and_defuses_fields() {
        let rows = vec![row(NOON, 7, "claude-work", "opus", 1_000, 1_234_567)];
        let mut l = labels();
        l.projects.insert(ProjectId(1), "=HYPERLINK(\"x\")".into());
        let text = csv(&rows, &UsageQuery { tz: "UTC".into(), ..Default::default() }, &l, NOON + 1);
        let mut lines = text.lines();
        assert!(lines.next().unwrap().starts_with("date,time,thread_id,thread,"));
        let line = lines.next().unwrap();
        assert!(line.starts_with("2026-11-20,12:00:00,thr_"), "{line}");
        assert!(line.contains(",\"Fix the login, \"\"fast\"\"\","), "{line}");
        assert!(line.contains(",\"'=HYPERLINK(\"\"x\"\")\","), "formulas are defused: {line}");
        assert!(line.ends_with(",Claude Code,Claude (work),Opus 5.5,1000,100,0,0,1.234567"), "{line}");
    }

    #[test]
    fn the_ledger_survives_restarts_torn_tails_and_backfills_once() {
        let dir = tempfile::tempdir().unwrap();
        let (mut ledger, backfill) = Ledger::open(dir.path());
        assert!(backfill && ledger.backfilling());
        ledger.append(row(NOON + 5, 7, "claude", "opus", 10, 0));
        assert!(!dir.path().join(FILE).exists(), "nothing is written before the backfill");
        let old = vec![row(NOON - DAY, 7, "claude", "opus", 20, 0), row(NOON + 9, 7, "claude", "opus", 30, 0)];
        ledger.backfilled(old, NOON).unwrap();
        assert_eq!(ledger.rows().len(), 2, "a backfilled turn after the start is already recorded live");
        ledger.append(row(NOON + 10, 8, "codex", "gpt", 40, 0));
        drop(ledger);

        let (ledger, backfill) = Ledger::open(dir.path());
        assert!(!backfill);
        let tokens: Vec<u64> = ledger.rows().iter().map(|r| r.usage.input_tokens).collect();
        assert_eq!(tokens, [20, 10, 40]);
        drop(ledger);

        // A crash mid-write leaves a torn record; it is cut off.
        let path = dir.path().join(FILE);
        let len = fs::metadata(&path).unwrap().len();
        OpenOptions::new().append(true).open(&path).unwrap().write_all(&[9, 0, 0, 0, 1, 2]).unwrap();
        let (mut ledger, _) = Ledger::open(dir.path());
        assert_eq!(ledger.rows().len(), 3);
        assert_eq!(fs::metadata(&path).unwrap().len(), len);
        ledger.append(row(NOON + 11, 8, "codex", "gpt", 50, 0));
        drop(ledger);
        assert_eq!(Ledger::open(dir.path()).0.rows().len(), 4);
    }
}
