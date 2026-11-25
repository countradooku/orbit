//! Fuzzy matching for pickers: `@` file mentions, `$` skills, `/` commands.
//!
//! One scorer for every picker so ranking feels the same everywhere. It is
//! linear in the candidate's length (two greedy passes, no dynamic
//! programming), allocation-free when only a score is needed, and ASCII
//! case-insensitive, so ranking 200,000 paths stays in the low milliseconds.
//!
//! A match is a subsequence of the query in the candidate. Its score rewards
//! what people mean when they type a few letters: characters at the start of
//! a word or path segment, runs of consecutive characters, matches in a
//! file's own name rather than its folders, prefixes and exact names; and it
//! charges for gaps and long candidates.

/// Points for each matched character.
const MATCH: i32 = 16;
/// The character starts a word (after `/`, `_`, `-`, `.`, a space, or a
/// lowercase-to-uppercase step).
const BOUNDARY: i32 = 10;
/// The character starts a path segment.
const SEGMENT: i32 = 14;
/// The character follows the previous match directly.
const CONSECUTIVE: i32 = 12;
/// Opening a gap between two matched characters, and each skipped one.
const GAP_OPEN: i32 = 3;
const GAP_EXTEND: i32 = 1;

/// A match and where it is, for highlighting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Match {
    pub score: i32,
    /// Byte offsets of the matched characters in the candidate.
    pub positions: Vec<usize>,
}

/// Scores `candidate` (a name, label or command) against `query`. Empty
/// queries match everything with score 0. Whitespace separates terms that
/// must all match.
pub fn score(query: &str, candidate: &str) -> Option<i32> {
    let mut total = 0;
    let hay = candidate.as_bytes();
    for term in query.split_whitespace() {
        let q = term.as_bytes();
        let (s, positions) = best_window(hay, q)?;
        total += s + text_bonus(hay, q, &positions);
    }
    Some(total)
}

/// Like [`score`], with the matched positions.
pub fn find(query: &str, candidate: &str) -> Option<Match> {
    let mut total = 0;
    let mut all = Vec::new();
    let hay = candidate.as_bytes();
    for term in query.split_whitespace() {
        let q = term.as_bytes();
        let (s, positions) = best_window(hay, q)?;
        total += s + text_bonus(hay, q, &positions);
        all.extend(positions);
    }
    all.sort_unstable();
    all.dedup();
    Some(Match { score: total, positions: all })
}

/// Scores a `/`-separated relative path. Matches inside the file's own name
/// beat matches spread over folders; a query with a `/` matches the whole
/// path. Folders (paths ending in `/`) rank slightly below files unless the
/// query ends in `/`.
pub fn score_path(query: &str, path: &str) -> Option<i32> {
    path_match(query, path, false).map(|m| m.score)
}

/// Like [`score_path`], with the matched positions.
pub fn find_path(query: &str, path: &str) -> Option<Match> {
    path_match(query, path, true)
}

fn path_match(query: &str, path: &str, want_positions: bool) -> Option<Match> {
    let hay = path.as_bytes();
    let is_dir = path.ends_with('/');
    let trimmed = path.trim_end_matches('/');
    let name_start = trimmed.rfind('/').map_or(0, |i| i + 1);
    let name = &trimmed[name_start..];
    let mut total = 0;
    let mut all = Vec::new();
    for term in query.split_whitespace() {
        let q = term.as_bytes();
        let wants_dir = term.ends_with('/');
        let q_core = if wants_dir && q.len() > 1 { &q[..q.len() - 1] } else { q };
        // The file name alone first: that is what people type.
        let in_name = if term.contains('/') { None } else { best_window(name.as_bytes(), q_core) };
        let (mut s, positions) = match in_name {
            Some((s, positions)) => {
                let positions: Vec<usize> = positions.into_iter().map(|p| p + name_start).collect();
                (s + 40 + name_bonus(name, term), positions)
            }
            None => {
                let (s, positions) = best_window(hay, q_core)?;
                (s, positions)
            }
        };
        if is_dir && !wants_dir {
            s -= 6;
        } else if is_dir && wants_dir {
            s += 10;
        }
        total += s;
        if want_positions {
            all.extend(positions);
        }
    }
    // Shorter paths win ties: `src/lib.rs` over `vendor/x/y/src/lib.rs`.
    total -= (path.len() as i32) / 6 + path.matches('/').count() as i32 * 2;
    all.sort_unstable();
    all.dedup();
    Some(Match { score: total, positions: all })
}

/// Exact and prefix matches of a whole name.
fn name_bonus(name: &str, term: &str) -> i32 {
    let stem = name.split('.').next().unwrap_or(name);
    if name.eq_ignore_ascii_case(term) {
        120
    } else if stem.eq_ignore_ascii_case(term) {
        100
    } else if starts_with_ignore_case(name, term) {
        60
    } else {
        0
    }
}

fn text_bonus(hay: &[u8], q: &[u8], positions: &[usize]) -> i32 {
    let whole = if hay.len() == q.len() && positions.first() == Some(&0) && eq_bytes(hay, q) {
        120
    } else if positions.first() == Some(&0) && positions.windows(2).all(|w| w[1] == w[0] + 1) {
        60
    } else {
        0
    };
    whole - hay.len() as i32 / 10
}

fn starts_with_ignore_case(s: &str, prefix: &str) -> bool {
    s.len() >= prefix.len() && s.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

fn eq_bytes(a: &[u8], b: &[u8]) -> bool {
    a.eq_ignore_ascii_case(b)
}

#[inline]
fn same(a: u8, b: u8) -> bool {
    a.eq_ignore_ascii_case(&b)
}

/// The better of the leftmost and the rightmost tight windows.
fn best_window(hay: &[u8], q: &[u8]) -> Option<(i32, Vec<usize>)> {
    if q.is_empty() {
        return Some((0, Vec::new()));
    }
    if q.len() > hay.len() {
        return None;
    }
    let left = leftmost(hay, q)?;
    let right = rightmost(hay, q);
    let a = score_positions(hay, &left);
    match right {
        Some(right) if right != left => {
            let b = score_positions(hay, &right);
            if b > a { Some((b, right)) } else { Some((a, left)) }
        }
        _ => Some((a, left)),
    }
}

/// Earliest-ending match, tightened backwards.
fn leftmost(hay: &[u8], q: &[u8]) -> Option<Vec<usize>> {
    let mut j = 0;
    let mut end = None;
    for (i, &b) in hay.iter().enumerate() {
        if same(b, q[j]) {
            j += 1;
            if j == q.len() {
                end = Some(i);
                break;
            }
        }
    }
    let end = end?;
    // Walk back from the end to the latest possible start.
    let mut k = q.len();
    let mut start = end;
    let mut i = end + 1;
    while i > 0 && k > 0 {
        i -= 1;
        if same(hay[i], q[k - 1]) {
            k -= 1;
            start = i;
        }
    }
    Some(forward_from(hay, q, start))
}

/// Latest-starting match, tightened forwards.
fn rightmost(hay: &[u8], q: &[u8]) -> Option<Vec<usize>> {
    let mut k = q.len();
    let mut i = hay.len();
    while i > 0 && k > 0 {
        i -= 1;
        if same(hay[i], q[k - 1]) {
            k -= 1;
        }
    }
    (k == 0).then(|| forward_from(hay, q, i))
}

fn forward_from(hay: &[u8], q: &[u8], start: usize) -> Vec<usize> {
    let mut positions = Vec::with_capacity(q.len());
    let mut j = 0;
    for (i, &b) in hay.iter().enumerate().skip(start) {
        if j < q.len() && same(b, q[j]) {
            positions.push(i);
            j += 1;
        }
    }
    positions
}

fn score_positions(hay: &[u8], positions: &[usize]) -> i32 {
    let mut score = 0;
    let mut prev: Option<usize> = None;
    for &p in positions {
        score += MATCH;
        let before = p.checked_sub(1).map(|i| hay[i]);
        score += match before {
            None | Some(b'/') | Some(b'\\') => SEGMENT,
            Some(b'_' | b'-' | b'.' | b' ' | b':') => BOUNDARY,
            Some(b) if b.is_ascii_lowercase() && hay[p].is_ascii_uppercase() => BOUNDARY - 2,
            _ => 0,
        };
        if let Some(prev) = prev {
            if p == prev + 1 {
                score += CONSECUTIVE;
            } else {
                score -= GAP_OPEN + GAP_EXTEND * (p - prev - 2) as i32;
            }
        }
        prev = Some(p);
    }
    score
}

/// The best `limit` of `candidates` by `score`, best first; ties keep the
/// candidates' order. Bounded memory: a heap of `limit` entries.
pub fn top<T>(candidates: impl IntoIterator<Item = (i32, T)>, limit: usize) -> Vec<T> {
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;
    if limit == 0 {
        return Vec::new();
    }
    // Min-heap on (score, -order): the weakest entry is on top.
    let mut heap: BinaryHeap<Reverse<Ranked<T>>> = BinaryHeap::with_capacity(limit + 1);
    for (order, (score, value)) in candidates.into_iter().enumerate() {
        if heap.len() == limit
            && let Some(Reverse((weakest, Reverse(weak_order), _))) = heap.peek()
            && (score, Reverse(order)) <= (*weakest, Reverse(*weak_order))
        {
            continue;
        }
        heap.push(Reverse((score, Reverse(order), Slot(value))));
        if heap.len() > limit {
            heap.pop();
        }
    }
    let mut out: Vec<(i32, usize, T)> = heap.into_iter().map(|Reverse((s, Reverse(o), Slot(v)))| (s, o, v)).collect();
    out.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    out.into_iter().map(|(_, _, v)| v).collect()
}

/// (score, earlier-is-greater order, value): a heap entry.
type Ranked<T> = (i32, std::cmp::Reverse<usize>, Slot<T>);

/// Lets any value ride in the heap; ordering ignores it.
struct Slot<T>(T);

impl<T> PartialEq for Slot<T> {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}
impl<T> Eq for Slot<T> {}
impl<T> PartialOrd for Slot<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl<T> Ord for Slot<T> {
    fn cmp(&self, _: &Self) -> std::cmp::Ordering {
        std::cmp::Ordering::Equal
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rank<'a>(query: &str, paths: &[&'a str]) -> Vec<&'a str> {
        top(paths.iter().filter_map(|p| score_path(query, p).map(|s| (s, *p))), paths.len())
    }

    #[test]
    fn subsequences_match_case_insensitively() {
        assert!(score("cmp", "Composer").is_some());
        assert!(score("CMP", "composer").is_some());
        assert!(score("xyz", "composer").is_none());
        assert!(score("composers", "composer").is_none());
        assert_eq!(score("", "anything"), Some(0));
    }

    #[test]
    fn file_names_beat_folders() {
        let paths = ["src/composer/mod.rs", "crates/orbit-app/src/composer.rs", "docs/compose-notes/readme.md"];
        assert_eq!(rank("composer", &paths)[0], "crates/orbit-app/src/composer.rs");
        assert_eq!(rank("mod", &paths)[0], "src/composer/mod.rs");
    }

    #[test]
    fn exact_and_prefix_names_win() {
        let paths = ["src/main_loop.rs", "src/domain.rs", "src/main.rs"];
        assert_eq!(rank("main", &paths), vec!["src/main.rs", "src/main_loop.rs", "src/domain.rs"]);
    }

    #[test]
    fn word_starts_beat_scattered_letters() {
        let paths = ["src/orchestrator.rs", "src/app_state.rs"];
        assert_eq!(rank("as", &paths)[0], "src/app_state.rs");
        assert!(score("ap", "AppState").unwrap() > score("ap", "wrap").unwrap());
    }

    #[test]
    fn slashes_match_the_whole_path_and_folders_need_asking() {
        let paths = ["crates/orbit-core/src/engine/files.rs", "crates/orbit-app/src/files.rs", "crates/orbit-core/src/engine/"];
        assert_eq!(rank("engine/files", &paths)[0], "crates/orbit-core/src/engine/files.rs");
        assert_eq!(rank("engine/", &paths)[0], "crates/orbit-core/src/engine/");
        assert_eq!(rank("files", &paths)[0], "crates/orbit-app/src/files.rs", "shorter path wins a tie");
    }

    #[test]
    fn terms_must_all_match() {
        assert!(score_path("core files", "crates/orbit-core/src/engine/files.rs").is_some());
        assert!(score_path("core nothing", "crates/orbit-core/src/engine/files.rs").is_none());
    }

    #[test]
    fn positions_mark_the_matched_characters() {
        let m = find_path("files", "crates/orbit-core/src/engine/files.rs").unwrap();
        let path = "crates/orbit-core/src/engine/files.rs";
        let matched: String = m.positions.iter().map(|&p| path.as_bytes()[p] as char).collect();
        assert_eq!(matched, "files");
        assert_eq!(m.positions[0], path.rfind('/').unwrap() + 1, "the file name, not a folder");
        let m = find("rv", "review").unwrap();
        assert_eq!(m.positions, vec![0, 2]);
    }

    #[test]
    fn top_keeps_the_best_in_order() {
        let best = top(vec![(1, "a"), (5, "b"), (3, "c"), (5, "d"), (0, "e")], 3);
        assert_eq!(best, vec!["b", "d", "c"]);
        assert!(top(vec![(1, "a")], 0).is_empty());
    }

    #[test]
    fn ranking_a_large_tree_is_fast() {
        let paths: Vec<String> = (0..200_000).map(|i| format!("crates/pkg{}/src/module_{i}/file_{i}.rs", i % 97)).collect();
        let start = std::time::Instant::now();
        let hits = top(paths.iter().filter_map(|p| score_path("mod12fil", p).map(|s| (s, p))), 50);
        let elapsed = start.elapsed();
        assert!(!hits.is_empty());
        // Generous for debug builds on slow CI; release is ~10x faster.
        assert!(elapsed < std::time::Duration::from_secs(3), "{elapsed:?}");
    }
}
