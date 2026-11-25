//! Builds a signed local update fixture for end-to-end checks of the real
//! app (see `docs/RELEASING.md`, "Testing updates locally"):
//!
//! ```text
//! ORBIT_FIXTURE_DIR=<out> ORBIT_FIXTURE_APP=<new orbit-desktop> ORBIT_FIXTURE_VERSION=0.1.1 \
//! ORBIT_FIXTURE_BASE=http://127.0.0.1:8765 cargo test -p orbit-app make_update_fixture -- --ignored
//! ```
//!
//! It writes `<channel>.json`, the bundle it names, and `public-key.txt`
//! (the test key, for `ORBIT_UPDATE_PUBLIC_KEY`). Serve the folder on
//! loopback and point `ORBIT_UPDATE_URL` at `<base>/{channel}.json`.

use std::io::Write as _;

use super::verify::testing::TestKey;
use super::verify::trusted_comment;

#[test]
#[ignore = "writes a fixture for manual end-to-end checks"]
fn make_update_fixture() {
    let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("set {name}"));
    let dir = std::path::PathBuf::from(var("ORBIT_FIXTURE_DIR"));
    let app = std::fs::read(var("ORBIT_FIXTURE_APP")).unwrap();
    let version = var("ORBIT_FIXTURE_VERSION");
    let base = var("ORBIT_FIXTURE_BASE");
    let channel = std::env::var("ORBIT_FIXTURE_CHANNEL").unwrap_or_else(|_| "stable".into());
    let target = env!("ORBIT_TARGET");
    let exe = if cfg!(windows) { "orbit-desktop.exe" } else { "orbit-desktop" };
    std::fs::create_dir_all(&dir).unwrap();

    let name = format!("orbit-{version}-{target}.zip");
    let bundle = dir.join(&name);
    let mut zip = zip::ZipWriter::new(std::fs::File::create(&bundle).unwrap());
    let options = zip::write::SimpleFileOptions::default().unix_permissions(0o755);
    zip.start_file(format!("orbit-{version}-{target}/{exe}"), options).unwrap();
    zip.write_all(&app).unwrap();
    zip.start_file(format!("orbit-{version}-{target}/README.md"), options).unwrap();
    zip.write_all(format!("Orbit {version} (test fixture)\n").as_bytes()).unwrap();
    zip.finish().unwrap();

    let bytes = std::fs::read(&bundle).unwrap();
    let key = TestKey::new(42);
    let signature = key.sign(&bytes, &trusted_comment(&version, target));
    let manifest = super::manifest::Manifest {
        version: version.clone(),
        channel: None,
        pub_date: Some("2026-11-25T12:00:00Z".into()),
        notes: format!(
            "## Orbit {version}\n\nA local test release.\n\n- **Faster startup**: the window opens before agents are probed.\n\
             - Race results now show which tests each agent ran.\n- Fixed: the review panel kept a stale diff after a revert.\n"
        ),
        platforms: [(
            target.to_owned(),
            super::manifest::Artifact {
                url: format!("{base}/{name}"),
                sha256: super::verify::sha256_file(&bundle),
                size: Some(bytes.len() as u64),
                signature,
            },
        )]
        .into(),
    };
    std::fs::write(dir.join(format!("{channel}.json")), sonic_rs::to_string_pretty(&manifest).unwrap()).unwrap();
    std::fs::write(dir.join("public-key.txt"), key.public_base64()).unwrap();
}
