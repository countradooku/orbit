//! Frame and load timing for the review sheet, for measuring large reviews.
//! Off unless `ORBIT_REVIEW_PERF=<file>` is set; then the sheet renders
//! every frame and appends frame-time percentiles (per 300 frames) and load
//! times to that file.

use std::io::Write as _;
use std::path::PathBuf;
use std::time::Instant;

pub struct PerfLog {
    path: PathBuf,
    last: Option<Instant>,
    frames: Vec<f32>,
}

impl PerfLog {
    pub fn from_env() -> Option<Self> {
        let path = std::env::var_os("ORBIT_REVIEW_PERF")?;
        Some(Self { path: path.into(), last: None, frames: Vec::with_capacity(300) })
    }

    /// Call once per render; returns true to keep frames coming.
    pub fn frame(&mut self) {
        let now = Instant::now();
        if let Some(last) = self.last.replace(now) {
            self.frames.push((now - last).as_secs_f32() * 1000.);
        }
        if self.frames.len() >= 300 {
            let mut f = std::mem::take(&mut self.frames);
            f.sort_by(|a, b| a.total_cmp(b));
            let p = |q: f32| f[((f.len() - 1) as f32 * q) as usize];
            let slow = f.iter().filter(|&&ms| ms > 1000. / 55.).count();
            self.write(&format!(
                "frames n={} p50={:.2}ms p95={:.2}ms p99={:.2}ms max={:.2}ms over18ms={}",
                f.len(),
                p(0.5),
                p(0.95),
                p(0.99),
                f[f.len() - 1],
                slow
            ));
        }
    }

    pub fn write(&self, line: &str) {
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&self.path) {
            let _ = writeln!(file, "{line}");
        }
    }
}
