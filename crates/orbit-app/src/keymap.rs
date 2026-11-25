//! Key bindings: Orbit's defaults, then the user's `keybindings.json`.
//!
//! The file lives in the data folder. It is a JSON array (comments and
//! trailing commas allowed) of rules in the shape VS Code and T3 Code use:
//!
//! ```json
//! [
//!   // `mod` is Cmd on macOS and Ctrl elsewhere.
//!   { "key": "mod-shift-n", "command": "orbit::NewThread" },
//!   // A leading `-` removes a default; `null` silences a key.
//!   { "key": "mod-n", "command": "-orbit::NewThread" },
//!   { "key": "j", "command": "orbit::DeckNext", "when": "Deck && !Input" }
//! ]
//! ```
//!
//! Every rule is checked on its own: an unknown command, a key that does not
//! parse or a `when` naming a context that does not exist is reported, and
//! the other rules still apply. The defaults always stay. The file is read
//! at start and again whenever the window comes back to the front after the
//! file changed, so editing it in another app takes effect on return.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::SystemTime;

use gpui_kit::*;

use crate::actions::{self, expand_mod};

pub const FILE_NAME: &str = "keybindings.json";

/// Marks the bindings that came from the user's file, so a reload can take
/// exactly those out again.
const USER_META: KeyBindingMetaIndex = KeyBindingMetaIndex(0x0B17_0001);

/// Contexts a `when` clause may name: Orbit's own and GPUI Kit's.
const KNOWN_CONTEXTS: &[&str] = &[
    "Workspace", "Thread", "Composer", "SettingsPage", "Terminal", "Deck", "Sidebar", "Tiles", "Palette",
    "Input", "List", "Diff", "Dialog", "Popover", "PopupMenu", "Command", "Root", "TextView", "Select",
    "Combobox", "Tree", "Sheet", "NumberInput", "TimeField", "ColorPicker", "Questionnaire", "SettingsHost",
    "GridLayout", "Carousel", "AppMenuBar",
];

/// Marks the bindings plugins add (`crate::plugins`): after Orbit's
/// defaults, before the user's file, so the user's rules still win.
const PLUGIN_META: KeyBindingMetaIndex = KeyBindingMetaIndex(0x0B17_0002);

/// Replaces the plugins' bindings.
pub fn set_plugin_bindings(bindings: Vec<KeyBinding>, cx: &mut App) {
    {
        let keymap = cx.key_bindings();
        let mut keymap = keymap.borrow_mut();
        let (user, rest): (Vec<KeyBinding>, Vec<KeyBinding>) =
            keymap.bindings().filter(|b| b.meta() != Some(PLUGIN_META)).cloned().partition(|b| b.meta() == Some(USER_META));
        keymap.clear();
        keymap.add_bindings(rest);
        keymap.add_bindings(bindings.into_iter().map(|b| b.with_meta(PLUGIN_META)));
        keymap.add_bindings(user);
    }
    cx.refresh_windows();
}

/// The outcome of reading the file.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Report {
    /// Rules that took effect.
    pub applied: usize,
    /// One line per rule that did not, plus warnings about overlaps.
    pub problems: Vec<String>,
}

struct KeymapState {
    path: PathBuf,
    modified: Option<SystemTime>,
}

impl Global for KeymapState {}

/// Binds Orbit's defaults. Call once at start; call [`reload`] after every
/// other module has bound its keys, so the user's rules come last and win.
pub fn install(data_dir: &Path, cx: &mut App) {
    let defaults: Vec<KeyBinding> = actions::defaults()
        .into_iter()
        .filter_map(|binding| {
            let context = binding.context.map(|c| Rc::new(KeyBindingContextPredicate::parse(c).expect("default context")));
            KeyBinding::load(&expand_mod(binding.keys), binding.action, context, false, None, cx)
                .inspect_err(|err| log::error!("default binding {} failed: {err}", binding.keys))
                .ok()
        })
        .collect();
    cx.bind_keys(defaults);
    cx.set_global(KeymapState { path: data_dir.join(FILE_NAME), modified: None });
}

/// The user's keybindings file.
pub fn path(cx: &App) -> PathBuf {
    cx.try_global::<KeymapState>().map(|s| s.path.clone()).unwrap_or_else(|| PathBuf::from(FILE_NAME))
}

/// Reads the file again if it changed since the last read; returns the new
/// report when it did.
pub fn reload_if_changed(cx: &mut App) -> Option<Report> {
    let state = cx.try_global::<KeymapState>()?;
    let modified = std::fs::metadata(&state.path).and_then(|m| m.modified()).ok();
    (modified != state.modified).then(|| reload(cx))
}

/// Reads the file and replaces the user bindings with what it holds now.
pub fn reload(cx: &mut App) -> Report {
    let Some(state) = cx.try_global::<KeymapState>() else { return Report::default() };
    let path = state.path.clone();
    let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    let (bindings, report) = match std::fs::read_to_string(&path) {
        Ok(text) => load(&text, cx),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => (Vec::new(), Report::default()),
        Err(err) => (Vec::new(), Report { applied: 0, problems: vec![format!("Couldn’t read the file: {err}")] }),
    };
    {
        let keymap = cx.key_bindings();
        let mut keymap = keymap.borrow_mut();
        let kept: Vec<KeyBinding> = keymap.bindings().filter(|b| b.meta() != Some(USER_META)).cloned().collect();
        keymap.clear();
        keymap.add_bindings(kept);
        keymap.add_bindings(bindings);
    }
    cx.refresh_windows();
    let state = cx.global_mut::<KeymapState>();
    state.modified = modified;
    report
}

/// Creates the file with a short guide if it does not exist yet.
pub fn ensure_file(path: &Path) -> std::io::Result<()> {
    if path.exists() {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, TEMPLATE)
}

const TEMPLATE: &str = r#"// Orbit key bindings. Rules here win over the defaults.
// `mod` is Cmd on macOS and Ctrl elsewhere. A leading "-" on a command
// removes that default from the key; "command": null silences the key.
// Contexts for "when": Workspace, Thread, Composer, Deck, Sidebar, Tiles,
// Terminal, Input. Combine them with &&, || and !.
// The keyboard shortcuts sheet (? or mod-/) lists every command.
[
  // { "key": "mod-shift-n", "command": "orbit::NewThread" },
  // { "key": "mod-n", "command": "-orbit::NewThread" },
]
"#;

/// The keystrokes that run `action` now (the binding menus show), as typed.
pub fn keystrokes_for(action: &dyn Action, cx: &App) -> Option<Vec<Keystroke>> {
    let keymap = cx.key_bindings();
    let keymap = keymap.borrow();
    let binding = keymap.bindings_for_action(action).last()?;
    Some(
        binding
            .keystrokes()
            .iter()
            .map(|k| Keystroke { modifiers: *k.modifiers(), key: k.key().into(), key_char: None })
            .collect(),
    )
}

// ── Parsing ──────────────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct RawRule {
    key: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    when: Option<String>,
}

/// A rule as written, checked for shape but not yet resolved.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Rule {
    pub keys: String,
    pub target: Target,
    pub when: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Target {
    Bind(String),
    /// `-command`: remove that command from the key.
    Unbind(String),
    /// `null`: the key does nothing (in `when`).
    Silence,
}

/// Parses the file's text into rules, and problems for entries that are not
/// rules at all.
pub(crate) fn parse(text: &str) -> (Vec<Rule>, Vec<String>) {
    let clean = strip_jsonc(text);
    if clean.trim().is_empty() {
        return (Vec::new(), Vec::new());
    }
    let raw: Vec<RawRule> = match sonic_rs::from_str(&clean) {
        Ok(raw) => raw,
        Err(err) => return (Vec::new(), vec![format!("The file is not a JSON array of rules: {err}")]),
    };
    let mut rules = Vec::new();
    let mut problems = Vec::new();
    for (ix, raw) in raw.into_iter().enumerate() {
        let n = ix + 1;
        let Some(keys) = raw.key.map(|k| k.trim().to_owned()).filter(|k| !k.is_empty()) else {
            problems.push(format!("Rule {n}: \"key\" is missing."));
            continue;
        };
        let target = match raw.command.as_deref().map(str::trim) {
            None => Target::Silence,
            Some("") => {
                problems.push(format!("Rule {n} ({keys}): \"command\" is empty."));
                continue;
            }
            Some(name) => match name.strip_prefix('-') {
                Some(name) => Target::Unbind(qualify(name)),
                None => Target::Bind(qualify(name)),
            },
        };
        let when = raw.when.map(|w| w.trim().to_owned()).filter(|w| !w.is_empty());
        rules.push(Rule { keys, target, when });
    }
    (rules, problems)
}

/// `NewThread` and `orbit::NewThread` both name Orbit's command.
fn qualify(name: &str) -> String {
    let name = name.trim();
    if name.contains("::") { name.to_owned() } else { format!("orbit::{name}") }
}

/// Checks a `when` clause: it must parse and name only known contexts.
pub(crate) fn check_when(when: &str) -> Result<KeyBindingContextPredicate, String> {
    let predicate = KeyBindingContextPredicate::parse(when).map_err(|err| format!("\"when\" does not parse: {err}"))?;
    let mut names = Vec::new();
    identifiers(&predicate, &mut names);
    match names.into_iter().find(|name| !KNOWN_CONTEXTS.contains(&name.as_ref())) {
        Some(unknown) => Err(format!("\"when\" names an unknown context “{unknown}”")),
        None => Ok(predicate),
    }
}

fn identifiers(predicate: &KeyBindingContextPredicate, out: &mut Vec<SharedString>) {
    use KeyBindingContextPredicate as P;
    match predicate {
        P::Identifier(name) => out.push(name.clone()),
        // `mode == editor` tests a key on a context; the key is not a context.
        P::Equal(..) | P::NotEqual(..) => {}
        P::Not(inner) => identifiers(inner, out),
        P::Descendant(a, b) | P::And(a, b) | P::Or(a, b) => {
            identifiers(a, out);
            identifiers(b, out);
        }
    }
}

/// Resolves rules into bindings against the registered commands.
fn load(text: &str, cx: &App) -> (Vec<KeyBinding>, Report) {
    let (rules, mut problems) = parse(text);
    let mut bindings = Vec::new();
    let mut seen: Vec<(String, Option<String>, String)> = Vec::new();
    for (ix, rule) in rules.iter().enumerate() {
        let n = ix + 1;
        let keys = expand_mod(&rule.keys);
        let context = match rule.when.as_deref().map(check_when).transpose() {
            Ok(predicate) => predicate.map(Rc::new),
            Err(message) => {
                problems.push(format!("Rule {n} ({}): {message}.", rule.keys));
                continue;
            }
        };
        let action: Box<dyn Action> = match &rule.target {
            Target::Bind(name) => match cx.build_action(name, None) {
                Ok(action) => action,
                Err(_) => {
                    problems.push(format!("Rule {n} ({}): unknown command “{name}”.", rule.keys));
                    continue;
                }
            },
            Target::Unbind(name) => {
                if cx.build_action(name, None).is_err() {
                    problems.push(format!("Rule {n} ({}): unknown command “{name}”.", rule.keys));
                    continue;
                }
                Box::new(Unbind(name.clone().into()))
            }
            Target::Silence => Box::new(NoAction),
        };
        match KeyBinding::load(&keys, action, context, false, None, cx) {
            Ok(binding) => bindings.push(binding.with_meta(USER_META)),
            Err(err) => {
                problems.push(format!("Rule {n}: “{}” is not a key: {err}.", rule.keys));
                continue;
            }
        }
        if let Target::Bind(name) = &rule.target {
            if let Some((_, _, other)) = seen.iter().find(|(k, w, _)| *k == keys && *w == rule.when)
                && other != name
            {
                problems.push(format!("Rule {n} ({}): also bound to {other} earlier in the file; this rule wins.", rule.keys));
            }
            seen.push((keys, rule.when.clone(), name.clone()));
        }
    }
    let applied = bindings.len();
    (bindings, Report { applied, problems })
}

/// JSON with comments and trailing commas, as editors write keybinding
/// files, made into plain JSON. Strings are left alone.
pub(crate) fn strip_jsonc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;
    while let Some(ch) = chars.next() {
        if in_string {
            out.push(ch);
            match ch {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match (ch, chars.peek()) {
            ('"', _) => {
                in_string = true;
                out.push(ch);
            }
            ('/', Some('/')) => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            ('/', Some('*')) => {
                chars.next();
                let mut last = ' ';
                for c in chars.by_ref() {
                    if last == '*' && c == '/' {
                        break;
                    }
                    last = c;
                }
            }
            _ => out.push(ch),
        }
    }
    remove_trailing_commas(&out)
}

fn remove_trailing_commas(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_string = false;
    let mut escaped = false;
    let chars: Vec<char> = text.chars().collect();
    for (ix, &ch) in chars.iter().enumerate() {
        if in_string {
            out.push(ch);
            match ch {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        if ch == '"' {
            in_string = true;
        }
        if ch == ',' && chars[ix + 1..].iter().find(|c| !c.is_whitespace()).is_some_and(|c| *c == ']' || *c == '}') {
            continue;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{Rule, Target, check_when, parse, strip_jsonc};

    #[test]
    fn comments_and_trailing_commas_are_accepted() {
        let text = r#"[
            // a comment with "quotes" and a // slash
            { "key": "mod-k", "command": "orbit::OpenPalette", }, /* block */
            { "key": "ctrl-/", "command": "Say \"hi\" // not a comment" },
        ]"#;
        let clean = strip_jsonc(text);
        assert!(clean.contains(r#""Say \"hi\" // not a comment""#));
        let (rules, problems) = parse(text);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0], Rule { keys: "mod-k".into(), target: Target::Bind("orbit::OpenPalette".into()), when: None });
    }

    #[test]
    fn rules_name_commands_three_ways() {
        let (rules, problems) = parse(
            r#"[{"key":"a","command":"NewThread"},{"key":"b","command":"-orbit::GoInbox"},{"key":"c","command":null},{"command":"x"}]"#,
        );
        assert_eq!(rules[0].target, Target::Bind("orbit::NewThread".into()));
        assert_eq!(rules[1].target, Target::Unbind("orbit::GoInbox".into()));
        assert_eq!(rules[2].target, Target::Silence);
        assert_eq!(problems.len(), 1, "the rule without a key is reported");
    }

    #[test]
    fn a_broken_file_is_one_problem_not_a_crash() {
        let (rules, problems) = parse("{ not json");
        assert!(rules.is_empty());
        assert_eq!(problems.len(), 1);
        assert_eq!(parse("  // only a comment\n").0.len(), 0);
    }

    #[test]
    fn when_clauses_must_name_real_contexts() {
        assert!(check_when("Deck && !Input").is_ok());
        assert!(check_when("Input && mode == editor").is_ok(), "keys on a context are not contexts");
        assert!(check_when("Workspace > Composer").is_ok());
        let err = check_when("Workpsace").unwrap_err();
        assert!(err.contains("Workpsace"), "{err}");
    }
}

#[cfg(test)]
mod ui_tests {
    //! The keymap in a real window: contexts, the user's file, unbinding.
    use std::cell::RefCell;
    use std::rc::Rc;

    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        AppContext as _, Bounds, Context, FocusHandle, InteractiveElement as _, IntoElement, ParentElement as _, Render, Styled as _,
        TestAppContext, Window, WindowBounds, WindowOptions, div, px, size,
    };

    use crate::actions::*;

    type Log = Rc<RefCell<Vec<String>>>;

    /// Workspace > Deck > Input, each with its own focus handle.
    struct Harness {
        deck: FocusHandle,
        input: FocusHandle,
        log: Log,
    }

    impl Render for Harness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let log = |name: &'static str, log: &Log| {
                let log = log.clone();
                move |_: &dyn std::any::Any| log.borrow_mut().push(name.to_owned())
            };
            let (next, shortcuts, pick) = (log("next", &self.log), log("shortcuts", &self.log), self.log.clone());
            div()
                .key_context("Workspace")
                .size_full()
                .on_action(move |a: &DeckNext, _, _| next(a))
                .on_action(move |a: &ShowShortcuts, _, _| shortcuts(a))
                .on_action(move |a: &DeckPick, _, _| pick.borrow_mut().push(format!("pick {}", a.index)))
                .child(
                    div()
                        .key_context("Deck")
                        .track_focus(&self.deck)
                        .size_full()
                        .child(div().key_context("Input").track_focus(&self.input).size_full()),
                )
        }
    }

    fn open(cx: &mut TestAppContext, user_file: &str) -> (gpui_kit::AnyWindowHandle, Log, FocusHandle, FocusHandle, super::Report) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(super::FILE_NAME), user_file).unwrap();
        let report = cx.update(|cx| {
            gpui_kit::init(cx);
            super::install(dir.path(), cx);
            super::reload(cx)
        });
        let log: Log = Rc::default();
        let (handle, deck, input) = cx.update(|cx| {
            let log = log.clone();
            let opened = gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::new(Default::default(), size(px(400.), px(300.))))),
                    ..Default::default()
                },
                cx,
                move |_, cx| cx.new(|cx| Harness { deck: cx.focus_handle(), input: cx.focus_handle(), log }),
            )
            .expect("open test window");
            let view = opened.1.clone();
            let (deck, input) = (view.read(cx).deck.clone(), view.read(cx).input.clone());
            (opened.0, deck, input)
        });
        cx.update_window(handle, |_, window, cx| window.render_frame(cx)).unwrap();
        (handle, log, deck, input, report)
    }

    #[gpui_kit::test]
    fn deck_keys_work_in_the_deck_and_never_while_typing(cx: &mut TestAppContext) {
        let (handle, log, deck, input, report) = open(cx, "[]");
        assert!(report.problems.is_empty());
        cx.update_window(handle, |_, window, cx| {
            deck.focus(window, cx);
            window.render_frame(cx);
            window.press("j", cx);
            window.press("3", cx);
            window.press("shift-/", cx);
            input.focus(window, cx);
            window.render_frame(cx);
            window.press("j", cx);
            window.press("shift-/", cx);
        })
        .unwrap();
        assert_eq!(*log.borrow(), vec!["next", "pick 2", "shortcuts"]);
    }

    #[gpui_kit::test]
    fn the_users_file_adds_removes_and_reports(cx: &mut TestAppContext) {
        let file = r#"[
            { "key": "x", "command": "DeckNext", "when": "Deck && !Input" },
            { "key": "j", "command": "-orbit::DeckNext" },
            { "key": "y", "command": "orbit::NoSuchThing" },
            { "key": "z", "command": "DeckNext", "when": "Dek" },
        ]"#;
        let (handle, log, deck, _, report) = open(cx, file);
        assert_eq!(report.applied, 2);
        assert_eq!(report.problems.len(), 2, "{:?}", report.problems);
        cx.update_window(handle, |_, window, cx| {
            deck.focus(window, cx);
            window.render_frame(cx);
            window.press("j", cx);
            window.press("x", cx);
        })
        .unwrap();
        assert_eq!(*log.borrow(), vec!["next"], "j was unbound and x took its place");
    }
}
