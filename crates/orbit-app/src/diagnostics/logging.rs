//! The app's log: what `env_logger` prints (filtered by `RUST_LOG`, warnings
//! by default), also kept in `<data folder>/logs/orbit-desktop.log` and in
//! memory (the last lines go into a crash report).
//!
//! The file is opened for appending and written one line at a time, so a
//! crash loses nothing already logged; past 2 MB it moves to
//! `orbit-desktop.1.log` and starts again, so logs never take more than
//! about 4 MB.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const LOG_FILE: &str = "orbit-desktop.log";
const ROTATED_FILE: &str = "orbit-desktop.1.log";
const MAX_BYTES: u64 = 2 * 1024 * 1024;
/// Lines kept in memory for a crash report.
const RECENT_LINES: usize = 200;

static RECENT: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());

pub fn logs_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("logs")
}

struct LogFile {
    path: PathBuf,
    file: File,
    written: u64,
}

impl LogFile {
    fn open(path: PathBuf) -> Option<Self> {
        let file = OpenOptions::new().create(true).append(true).open(&path).ok()?;
        let written = file.metadata().map(|m| m.len()).unwrap_or(0);
        Some(Self { path, file, written })
    }

    fn write_line(&mut self, line: &str) {
        if self.written > MAX_BYTES {
            let rotated = self.path.with_file_name(ROTATED_FILE);
            let _ = std::fs::rename(&self.path, rotated);
            match OpenOptions::new().create(true).append(true).truncate(false).open(&self.path) {
                Ok(file) => {
                    self.file = file;
                    self.written = 0;
                }
                Err(_) => return,
            }
        }
        if self.file.write_all(line.as_bytes()).is_ok() {
            self.written += line.len() as u64;
        }
    }
}

struct OrbitLogger {
    console: env_logger::Logger,
    file: Mutex<Option<LogFile>>,
}

impl log::Log for OrbitLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        self.console.enabled(metadata)
    }

    fn log(&self, record: &log::Record) {
        if !self.console.matches(record) {
            return;
        }
        self.console.log(record);
        let line = format!(
            "{} {:<5} {}: {}\n",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
            record.level(),
            record.target(),
            record.args()
        );
        if let Ok(mut recent) = RECENT.lock() {
            if recent.len() == RECENT_LINES {
                recent.pop_front();
            }
            recent.push_back(line.trim_end().to_owned());
        }
        if let Ok(mut file) = self.file.lock()
            && let Some(file) = file.as_mut()
        {
            file.write_line(&line);
        }
    }

    fn flush(&self) {
        self.console.flush();
        if let Ok(mut file) = self.file.lock()
            && let Some(file) = file.as_mut()
        {
            let _ = file.file.flush();
        }
    }
}

/// Starts logging: to stderr as before, and to the data folder's log file.
pub fn init(data_dir: &Path) {
    let console = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).build();
    let dir = logs_dir(data_dir);
    let file = std::fs::create_dir_all(&dir).ok().and_then(|_| LogFile::open(dir.join(LOG_FILE)));
    let max = console.filter();
    if log::set_boxed_logger(Box::new(OrbitLogger { console, file: Mutex::new(file) })).is_ok() {
        log::set_max_level(max);
    }
}

/// The last lines logged, oldest first. Never blocks: a crash while the
/// log was being written gets no lines rather than a deadlock.
pub fn recent_lines() -> Vec<String> {
    match RECENT.try_lock() {
        Ok(recent) => recent.iter().cloned().collect(),
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_log_file_rotates_past_its_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOG_FILE);
        let mut file = LogFile::open(path.clone()).unwrap();
        let line = format!("{}\n", "x".repeat(1023));
        for _ in 0..(MAX_BYTES / 1024 + 2) {
            file.write_line(&line);
        }
        let rotated = dir.path().join(ROTATED_FILE);
        assert!(rotated.exists());
        assert!(std::fs::metadata(&rotated).unwrap().len() > MAX_BYTES);
        assert!(std::fs::metadata(&path).unwrap().len() < MAX_BYTES);
    }
}
