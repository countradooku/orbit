//! "Collect diagnostics": a zip of the logs, crash reports and version
//! information, with secrets removed ([`super::redact`]), saved where the
//! user chooses. Nothing is uploaded.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use super::{build, crash, logging, redact};

/// What goes in, gathered on the UI thread.
#[derive(Clone, Debug, Default)]
pub struct BundleInput {
    pub data_dir: PathBuf,
    /// Exact values to remove (agent environment values, tokens).
    pub secrets: Vec<String>,
    /// `agents.txt`: each agent's version and sign-in state.
    pub agents: String,
    /// Where the engine runs.
    pub engine: String,
}

/// The files a bundle holds: (name in the zip, source path or `None` for
/// generated text).
fn sources(data_dir: &Path) -> Vec<(String, PathBuf)> {
    let mut files = Vec::new();
    let logs = logging::logs_dir(data_dir);
    for name in ["orbit-desktop.1.log", logging::LOG_FILE] {
        let path = logs.join(name);
        if path.is_file() {
            files.push((format!("logs/{name}"), path));
        }
    }
    for path in crash::all_reports(data_dir) {
        let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        files.push((format!("crashes/{name}"), path));
    }
    files
}

/// The names of the files [`write_bundle`] would write, for the dialog.
pub fn preview(data_dir: &Path) -> Vec<String> {
    let mut names = vec!["README.txt".to_owned(), "system.txt".to_owned(), "agents.txt".to_owned()];
    names.extend(sources(data_dir).into_iter().map(|(name, _)| name));
    names
}

fn system_text(input: &BundleInput) -> String {
    format!(
        "Orbit {}\nTarget: {}\nProtocol: OWP/{}\nSystem: {}\nEngine: {}\nData folder: {}\nCollected: {}\n",
        build::summary(),
        build::TARGET,
        orbit_proto::PROTOCOL_VERSION,
        build::system(),
        input.engine,
        input.data_dir.display(),
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S %z"),
    )
}

const README: &str = "Orbit diagnostics\n\n\
This archive was made by Orbit's Collect diagnostics command, on request. It holds Orbit's \
own logs, crash reports, its version and your system's, and which agent CLIs it found.\n\n\
Before it was written, tokens, API keys, passwords, environment variable values, credentials in \
URLs and your home folder's path were replaced with [redacted] or ~. Your threads, prompts, code \
and settings are not included.\n\n\
Orbit sends nothing by itself: share this file only with someone you choose.\n";

/// Writes the bundle to `dest`; returns the names written.
pub fn write_bundle(input: &BundleInput, dest: &Path) -> std::io::Result<Vec<String>> {
    let clean = |text: &str| redact::redact(text, &input.secrets);
    let tmp = dest.with_extension("zip.part");
    let mut zip = zip::ZipWriter::new(std::fs::File::create(&tmp)?);
    let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let mut names = Vec::new();
    let mut add = |zip: &mut zip::ZipWriter<std::fs::File>, name: &str, text: &str| -> std::io::Result<()> {
        zip.start_file(name, options).map_err(std::io::Error::other)?;
        zip.write_all(text.as_bytes())?;
        names.push(name.to_owned());
        Ok(())
    };
    add(&mut zip, "README.txt", README)?;
    add(&mut zip, "system.txt", &clean(&system_text(input)))?;
    add(&mut zip, "agents.txt", &clean(&input.agents))?;
    for (name, path) in sources(&input.data_dir) {
        let bytes = std::fs::read(&path)?;
        add(&mut zip, &name, &clean(&String::from_utf8_lossy(&bytes)))?;
    }
    zip.finish().map_err(std::io::Error::other)?;
    std::fs::rename(&tmp, dest)?;
    Ok(names)
}

/// `orbit-diagnostics-20261125-143005.zip`.
pub fn default_file_name() -> String {
    format!("orbit-diagnostics-{}.zip", chrono::Local::now().format("%Y%m%d-%H%M%S"))
}

#[cfg(test)]
mod tests {
    use std::io::Read as _;

    use super::*;

    #[test]
    fn the_bundle_holds_redacted_logs_and_reports() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let logs = logging::logs_dir(&data);
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(
            logs.join(logging::LOG_FILE),
            "WARN provider: started with ANTHROPIC_API_KEY=sk-ant-api03-secretsecret123 and proxy-pass-9876\n\
             WARN forge: GET https://bob:hunter22@git.example/x?token=t0k3n failed\n",
        )
        .unwrap();
        std::fs::create_dir_all(crash::crashes_dir(&data)).unwrap();
        std::fs::write(crash::crashes_dir(&data).join("crash-20261125-120000-1.txt"), "Panic: boom ghp_0123456789abcdefABCDEF0123456789abcd").unwrap();
        let input = BundleInput {
            data_dir: data.clone(),
            secrets: vec!["proxy-pass-9876".into()],
            agents: "Claude Code 2.0.1, signed in\nCodex: not found\n".into(),
            engine: "In this app".into(),
        };
        let dest = dir.path().join("out.zip");
        let names = write_bundle(&input, &dest).unwrap();
        assert_eq!(names, preview(&data));
        assert!(names.contains(&"logs/orbit-desktop.log".to_owned()));
        assert!(names.contains(&"crashes/crash-20261125-120000-1.txt".to_owned()));

        let mut zip = zip::ZipArchive::new(std::fs::File::open(&dest).unwrap()).unwrap();
        let mut everything = String::new();
        for i in 0..zip.len() {
            let mut entry = zip.by_index(i).unwrap();
            entry.read_to_string(&mut everything).unwrap();
        }
        for secret in ["sk-ant-api03-secretsecret123", "proxy-pass-9876", "hunter22", "t0k3n", "ghp_0123456789"] {
            assert!(!everything.contains(secret), "{secret} leaked:\n{everything}");
        }
        assert!(everything.contains("[redacted]"));
        assert!(everything.contains("Claude Code 2.0.1"));
        assert!(everything.contains(build::VERSION));
        assert!(!dest.with_extension("zip.part").exists());
    }
}
