//! What a file-changing step changed, as a diff, from what the agent sent:
//! Claude's `Edit`, `MultiEdit` and `Write` inputs, Codex's patches, and the
//! patches in change-approval requests. The timeline shows these inline, so
//! an edit reads as an edit (red and green lines), not as JSON.
//!
//! Patches carry the file's real line numbers. An `Edit` only carries the
//! replaced text, so its diff is of that snippet and its line numbers are
//! the snippet's: the view hides them ([`EditDiff::numbered`]).

use gpui_kit::component::{Theme, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_proto::diff::{FileDiff, LineKind};
use std::path::Path;

use orbit_proto::{ItemId, ToolCall};

/// Lines shown before "Show N more lines".
pub const PREVIEW_LINES: usize = 12;
/// Lines shown at most when opened; longer diffs go to the inspector.
const MAX_LINES: usize = 400;

#[derive(Debug)]
pub struct EditDiff {
    pub files: Vec<FileDiff>,
    /// Line numbers are the file's own (from a patch), not a snippet's.
    pub numbered: bool,
}

impl EditDiff {
    pub fn lines(&self) -> usize {
        self.files.iter().flat_map(|f| &f.hunks).map(|h| h.lines.len()).sum()
    }

    pub fn additions(&self) -> u32 {
        self.files.iter().map(|f| f.additions).sum()
    }

    pub fn deletions(&self) -> u32 {
        self.files.iter().map(|f| f.deletions).sum()
    }
}

/// The diff a step made, when its input says. `root` makes paths relative.
pub fn from_call(call: &ToolCall, root: Option<&Path>) -> Option<EditDiff> {
    let patch = orbit_core::edits::patch(call, root)?;
    let files = orbit_proto::diff::parse(&patch.text);
    (!files.is_empty()).then_some(EditDiff { files, numbered: patch.numbered })
}

/// The diff in a change-approval request (a unified patch).
pub fn from_patch(path: &str, patch: &str, root: Option<&Path>) -> Option<EditDiff> {
    let p = Path::new(path);
    let rel = root.and_then(|r| p.strip_prefix(r).ok()).unwrap_or(p).to_string_lossy().replace('\\', "/");
    let files = orbit_proto::diff::parse(&orbit_core::edits::with_headers(&rel, patch));
    (!files.is_empty()).then_some(EditDiff { files, numbered: true })
}

/// The diff, line by line, as the timeline shows it. Collapsed to
/// [`PREVIEW_LINES`] unless `open`; `on_toggle` opens or closes it.
pub fn render(
    key: (&'static str, ItemId),
    diff: &EditDiff,
    open: bool,
    on_toggle: impl Fn(&mut Window, &mut App) + 'static,
    theme: &Theme,
) -> AnyElement {
    let total = diff.lines();
    let shown = if open { total.min(MAX_LINES) } else { total.min(PREVIEW_LINES) };
    let mut budget = shown;
    let several = diff.files.len() > 1;
    let mono = theme.mono_font_family.clone();
    let number_width = rems(2.6);
    let mut body = v_flex().w_full().font_family(mono.clone()).text_xs();
    for (fi, file) in diff.files.iter().enumerate() {
        if budget == 0 {
            break;
        }
        if several {
            body = body.child(
                h_flex()
                    .px_2()
                    .py_0p5()
                    .gap_2()
                    .when(fi > 0, |el| el.border_t_1().border_color(theme.border))
                    .text_color(theme.muted_foreground)
                    .child(div().flex_1().truncate().child(file.path().to_owned()))
                    .child(div().text_color(theme.success).child(format!("+{}", file.additions)))
                    .child(div().text_color(theme.danger).child(format!("−{}", file.deletions))),
            );
        }
        for (hi, hunk) in file.hunks.iter().enumerate() {
            if budget == 0 {
                break;
            }
            if hi > 0 {
                body = body.child(div().h(px(1.)).mx_2().my_0p5().bg(theme.border.opacity(0.6)));
            }
            for line in &hunk.lines {
                if budget == 0 {
                    break;
                }
                budget -= 1;
                let (marker, tint, color) = match line.kind {
                    LineKind::Added => ("+", Some(theme.success), theme.foreground),
                    LineKind::Removed => ("−", Some(theme.danger), theme.foreground),
                    LineKind::Context => (" ", None, theme.muted_foreground),
                    LineKind::NoNewline => ("", None, theme.muted_foreground),
                };
                let text = if line.kind == LineKind::NoNewline {
                    "No newline at end of file".to_owned()
                } else {
                    file.line_text(line).to_owned()
                };
                let number = match line.kind {
                    LineKind::Removed => line.old_no,
                    _ => line.new_no,
                };
                body = body.child(
                    h_flex()
                        .w_full()
                        .when_some(tint, |el, tint| el.bg(tint.opacity(0.12)))
                        .when(diff.numbered, |el| {
                            el.child(
                                div()
                                    .flex_none()
                                    .w(number_width)
                                    .pr_1()
                                    .text_right()
                                    .text_color(theme.muted_foreground.opacity(0.7))
                                    .child(number.map(|n| n.to_string()).unwrap_or_default()),
                            )
                        })
                        .child(
                            div()
                                .flex_none()
                                .w(rems(1.1))
                                .text_center()
                                .text_color(tint.unwrap_or(theme.muted_foreground))
                                .child(marker),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .pr_2()
                                .text_color(color)
                                .when(line.kind == LineKind::NoNewline, |el| el.italic())
                                .child(text),
                        ),
                );
            }
        }
    }
    let hidden = total - shown;
    let toggle = (total > PREVIEW_LINES).then(|| {
        let label = if open {
            if hidden > 0 { format!("{hidden} more lines in the inspector · Show less") } else { "Show less".to_owned() }
        } else {
            format!("Show {} more lines", total - shown)
        };
        h_flex()
            .id((key.0, key.1.0))
            .px_2()
            .py_0p5()
            .text_xs()
            .text_color(theme.muted_foreground)
            .cursor_default()
            .hover(|s| s.text_color(theme.foreground))
            .border_t_1()
            .border_color(theme.border)
            .on_click(move |_, window, cx| on_toggle(window, cx))
            .child(label)
    });
    v_flex()
        .w_full()
        .overflow_hidden()
        .rounded(theme.radius)
        .border_1()
        .border_color(theme.border)
        .bg(theme.background)
        .child(body)
        .children(toggle)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::{from_call, from_patch};
    use orbit_proto::diff::LineKind;
    use orbit_proto::{ToolCall, ToolKind};

    #[test]
    fn steps_and_approvals_become_line_diffs() {
        let call = ToolCall {
            call_id: "c".into(),
            kind: ToolKind::Edit,
            name: "Edit".into(),
            title: "README.md".into(),
            input: r#"{"file_path":"README.md","old_string":"one\ntwo\nthree\n","new_string":"one\n2\nthree\n"}"#.into(),
            output: String::new(),
            output_blob: None,
            exit_code: None,
            files: vec![],
        };
        let diff = from_call(&call, None).unwrap();
        assert!(!diff.numbered);
        assert_eq!((diff.additions(), diff.deletions()), (1, 1));
        let file = &diff.files[0];
        let kinds: Vec<_> = file.hunks[0].lines.iter().map(|l| l.kind).collect();
        assert_eq!(kinds, [LineKind::Context, LineKind::Removed, LineKind::Added, LineKind::Context]);

        let root = std::path::Path::new(if cfg!(windows) { "C:\\p" } else { "/p" });
        let full = root.join("src").join("a.rs");
        let approval = from_patch(&full.to_string_lossy(), "@@ -4,2 +4,2 @@\n a\n-b\n+c\n", Some(root)).unwrap();
        assert!(approval.numbered);
        assert_eq!(approval.files[0].path(), "src/a.rs");
    }
}
