//! Content hashes: one BLAKE3 digest over a plugin's files.
//!
//! The digest covers every file's relative path (forward slashes) and
//! bytes, in path order, so it is the same on every OS and for every way a
//! plugin arrives (folder, archive, git checkout). `.git` is left out;
//! symbolic links are refused, so a plugin cannot point outside itself.

use std::path::Path;

use crate::manifest::limits;

/// The prefix of every content hash.
pub const PREFIX: &str = "blake3:";

/// The files of a plugin folder, relative path → absolute path, sorted.
pub fn files(root: &Path) -> Result<Vec<(String, std::path::PathBuf)>, String> {
    let mut out = Vec::new();
    let mut total = 0u64;
    walk(root, root, &mut out, &mut total)?;
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, std::path::PathBuf)>, total: &mut u64) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("Couldn’t read {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("Couldn’t read {}: {e}", dir.display()))?;
        let name = entry.file_name();
        let name = name.to_str().ok_or_else(|| format!("{}: file names must be UTF-8.", dir.display()))?;
        if name == ".git" {
            continue;
        }
        let path = entry.path();
        let meta = std::fs::symlink_metadata(&path).map_err(|e| format!("Couldn’t read {}: {e}", path.display()))?;
        if meta.file_type().is_symlink() {
            return Err(format!("{} is a symbolic link; plugins can’t contain links.", path.display()));
        }
        if meta.is_dir() {
            walk(root, &path, out, total)?;
        } else if meta.is_file() {
            *total += meta.len();
            if *total > limits::PLUGIN_BYTES {
                return Err(format!("The plugin is larger than {} MiB.", limits::PLUGIN_BYTES / 1024 / 1024));
            }
            let rel = path.strip_prefix(root).map_err(|_| "A file is outside the plugin.".to_owned())?;
            let rel: Vec<&str> = rel.components().filter_map(|c| c.as_os_str().to_str()).collect();
            out.push((rel.join("/"), path));
            if out.len() > limits::PLUGIN_FILES {
                return Err(format!("The plugin has more than {} files.", limits::PLUGIN_FILES));
            }
        }
    }
    Ok(())
}

/// `blake3:<hex>` of a plugin folder.
pub fn content_hash(root: &Path) -> Result<String, String> {
    let mut hasher = blake3::Hasher::new();
    for (rel, path) in files(root)? {
        let bytes = std::fs::read(&path).map_err(|e| format!("Couldn’t read {rel}: {e}"))?;
        hasher.update(rel.as_bytes());
        hasher.update(&[0]);
        hasher.update(&(bytes.len() as u64).to_le_bytes());
        hasher.update(&bytes);
    }
    Ok(format!("{PREFIX}{}", hasher.finalize().to_hex()))
}

/// Whether `expected` names the same content as `actual` (case-insensitive
/// hex, prefix required).
pub fn matches(expected: &str, actual: &str) -> bool {
    expected.trim().starts_with(PREFIX) && expected.trim().eq_ignore_ascii_case(actual)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hash_follows_content_not_place() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        for dir in [a.path(), b.path()] {
            std::fs::create_dir_all(dir.join("themes")).unwrap();
            std::fs::write(dir.join("orbit-plugin.toml"), "id = \"x\"").unwrap();
            std::fs::write(dir.join("themes/a.json"), "{}").unwrap();
        }
        std::fs::create_dir_all(b.path().join(".git")).unwrap();
        std::fs::write(b.path().join(".git/HEAD"), "ref").unwrap();
        let ha = content_hash(a.path()).unwrap();
        assert_eq!(ha, content_hash(b.path()).unwrap(), ".git is left out");
        assert!(matches(&ha.to_uppercase().replace("BLAKE3:", "blake3:"), &ha));
        std::fs::write(b.path().join("themes/a.json"), "{ }").unwrap();
        assert_ne!(ha, content_hash(b.path()).unwrap());
        // A file moved to another name is another plugin.
        std::fs::rename(a.path().join("themes/a.json"), a.path().join("themes/b.json")).unwrap();
        assert_ne!(ha, content_hash(a.path()).unwrap());
        assert!(!matches("deadbeef", &ha));
    }
}
