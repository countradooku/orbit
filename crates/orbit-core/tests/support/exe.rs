//! Putting a test's fake agent binary in place.

use std::path::Path;

/// Puts the program `from` at `to`, ready to run.
///
/// On Unix it is a hard link, never a copy. A copy is written through a
/// file descriptor, and any test thread that forks meanwhile (they all
/// start agents) hands that descriptor to its child until the child
/// execs; running the copy in that window fails with ETXTBSY ("Text file
/// busy") on Linux. A link opens nothing for writing. The fake still finds
/// its script beside `to`: a process's own path is the one it was started
/// by. A copy is the fallback across file systems.
pub fn install(from: &Path, to: &Path) {
    #[cfg(unix)]
    if std::fs::hard_link(from, to).is_ok() {
        return;
    }
    std::fs::copy(from, to).unwrap_or_else(|e| panic!("copying {} to {}: {e}", from.display(), to.display()));
}
