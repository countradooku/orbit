//! Opening files and folders in the user's own tools: an editor at
//! `file:line`, a terminal, or the file manager.
//!
//! Installed editors are found once, off the UI thread, on PATH and in their
//! usual install folders. The choice is this computer's (it is kept in the
//! shell preferences, not on the host): VS Code, Cursor, Windsurf, Zed,
//! Sublime Text, the JetBrains IDEs, the system's default app, or a custom
//! command with `{file}`, `{line}`, `{column}` and `{folder}`.
//!
//! On another computer (the window shows an SSH host or a WSL distro),
//! paths are that computer's: VS Code and its forks open them through their
//! remote extensions (`--remote ssh-remote+<host>`, `--remote wsl+<distro>`),
//! Zed through `ssh://`, and a terminal is an SSH session (or `wsl.exe`)
//! in the folder. Other editors can't reach them, which is said.

use std::path::{Path, PathBuf};

use gpui_kit::*;

/// How an editor is told where to go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Goto {
    /// `--goto file:line:column` (VS Code and its forks).
    Flag,
    /// `file:line:column` (Zed, Sublime Text).
    Suffix,
    /// `--line line file` (JetBrains).
    LineFlag,
}

struct Known {
    id: &'static str,
    label: &'static str,
    /// Command names on PATH.
    commands: &'static [&'static str],
    goto: Goto,
    /// Install locations to try when the command is not on PATH, relative
    /// to `%LOCALAPPDATA%`, `%ProgramFiles%` or `/Applications`.
    #[allow(dead_code)]
    installs: &'static [&'static str],
}

const KNOWN: &[Known] = &[
    Known {
        id: "vscode",
        label: "VS Code",
        commands: &["code"],
        goto: Goto::Flag,
        installs: &[
            "Programs/Microsoft VS Code/Code.exe",
            "Microsoft VS Code/Code.exe",
            "Visual Studio Code.app/Contents/Resources/app/bin/code",
        ],
    },
    Known {
        id: "cursor",
        label: "Cursor",
        commands: &["cursor"],
        goto: Goto::Flag,
        installs: &["Programs/cursor/Cursor.exe", "Cursor.app/Contents/Resources/app/bin/cursor"],
    },
    Known {
        id: "windsurf",
        label: "Windsurf",
        commands: &["windsurf"],
        goto: Goto::Flag,
        installs: &["Programs/Windsurf/Windsurf.exe", "Windsurf.app/Contents/Resources/app/bin/windsurf"],
    },
    Known {
        id: "zed",
        label: "Zed",
        commands: &["zed", "zeditor"],
        goto: Goto::Suffix,
        installs: &["Programs/Zed/bin/zed.exe", "Zed.app/Contents/MacOS/cli"],
    },
    Known {
        id: "sublime",
        label: "Sublime Text",
        commands: &["subl"],
        goto: Goto::Suffix,
        installs: &["Sublime Text/subl.exe", "Sublime Text.app/Contents/SharedSupport/bin/subl"],
    },
    Known { id: "rustrover", label: "RustRover", commands: &["rustrover"], goto: Goto::LineFlag, installs: &[] },
    Known { id: "idea", label: "IntelliJ IDEA", commands: &["idea"], goto: Goto::LineFlag, installs: &[] },
    Known { id: "webstorm", label: "WebStorm", commands: &["webstorm"], goto: Goto::LineFlag, installs: &[] },
    Known { id: "pycharm", label: "PyCharm", commands: &["pycharm"], goto: Goto::LineFlag, installs: &[] },
    Known { id: "goland", label: "GoLand", commands: &["goland"], goto: Goto::LineFlag, installs: &[] },
    Known { id: "clion", label: "CLion", commands: &["clion"], goto: Goto::LineFlag, installs: &[] },
];

/// The system's default app for the file.
pub const SYSTEM: &str = "system";
/// The user's own command.
pub const CUSTOM: &str = "custom";

/// An editor found on this computer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Editor {
    pub id: &'static str,
    pub label: &'static str,
    path: PathBuf,
    goto: Goto,
}

#[derive(Default)]
struct Detected {
    editors: Option<Vec<Editor>>,
    _task: Option<Task<()>>,
}

impl Global for Detected {}

/// Looks for installed editors in the background (once).
pub fn init(cx: &mut App) {
    if cx.has_global::<Detected>() {
        return;
    }
    let task = cx.spawn(async move |cx| {
        let editors = cx.background_executor().spawn(async { detect() }).await;
        cx.update(|cx| {
            cx.global_mut::<Detected>().editors = Some(editors);
            cx.refresh_windows();
        });
    });
    cx.set_global(Detected { editors: None, _task: Some(task) });
}

/// The editors found so far (empty while still looking).
pub fn installed(cx: &App) -> &[Editor] {
    cx.try_global::<Detected>().and_then(|d| d.editors.as_deref()).unwrap_or_default()
}

fn detect() -> Vec<Editor> {
    let mut found = Vec::new();
    for known in KNOWN {
        let path = known
            .commands
            .iter()
            .find_map(|name| orbit_core::provider::process::resolve_binary(name, None, &[]))
            .map(|path| prefer_gui_exe(&path).unwrap_or(path))
            .or_else(|| install_location(known));
        if let Some(path) = path {
            found.push(Editor { id: known.id, label: known.label, path, goto: known.goto });
        }
    }
    found
}

/// On Windows, VS Code-style CLIs are `bin\code.cmd` next to the app; run
/// the app itself, which takes the same arguments without cmd.exe quoting.
fn prefer_gui_exe(path: &Path) -> Option<PathBuf> {
    if !cfg!(windows) || !path.extension().is_some_and(|e| e.eq_ignore_ascii_case("cmd")) {
        return None;
    }
    let bin = path.parent()?;
    let install = bin.parent()?;
    let stem = path.file_stem()?.to_string_lossy().to_lowercase();
    let exe = match stem.as_str() {
        "code" => "Code.exe",
        "cursor" => "Cursor.exe",
        "windsurf" => "Windsurf.exe",
        _ => return None,
    };
    let candidate = install.join(exe);
    candidate.is_file().then_some(candidate)
}

fn install_location(known: &Known) -> Option<PathBuf> {
    let roots: Vec<PathBuf> = if cfg!(windows) {
        ["LOCALAPPDATA", "ProgramFiles"].iter().filter_map(std::env::var_os).map(PathBuf::from).collect()
    } else if cfg!(target_os = "macos") {
        let mut roots = vec![PathBuf::from("/Applications")];
        roots.extend(dirs_home().map(|h| h.join("Applications")));
        roots
    } else {
        Vec::new()
    };
    roots.iter().flat_map(|root| known.installs.iter().map(move |rel| root.join(rel))).find(|p| p.is_file())
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from)
}

/// The editor files open in: the user's choice when it is installed (or the
/// system default or custom command), else the first one found.
pub fn preferred(cx: &App) -> Option<Editor> {
    let choice = crate::prefs::get(cx).editor.clone();
    let installed = installed(cx);
    if choice == SYSTEM || choice == CUSTOM {
        return None;
    }
    installed.iter().find(|e| e.id == choice).or(installed.first()).cloned()
}

/// What "Open in editor" says: "Open in VS Code", or a generic label.
pub fn open_label(cx: &App) -> String {
    let choice = crate::prefs::get(cx).editor.clone();
    match preferred(cx) {
        Some(editor) if choice != SYSTEM && choice != CUSTOM => format!("Open in {}", editor.label),
        _ if choice == CUSTOM && !crate::prefs::get(cx).editor_command.trim().is_empty() => "Open in editor".into(),
        _ => "Open with the default app".into(),
    }
}

/// Where paths live when the window shows another computer.
fn remote_host(cx: &App) -> Option<orbit_net::remote::HostEntry> {
    let model = cx.try_global::<crate::settings::SettingsModel>()?;
    let entry = model.app().read(cx).active_entry();
    (!entry.is_local()).then(|| entry.clone())
}

/// Opens `path` (a file at `line`, or a folder) on another computer.
fn open_remote(host: &orbit_net::remote::HostEntry, path: &Path, line: Option<u32>, cx: &App) -> Result<(), String> {
    use orbit_net::remote::HostKind;
    let label = host.label();
    let unreachable = || format!("{} is on {label}. Open it there, or use VS Code with Remote - SSH or WSL.", path.display());
    let remote = match &host.kind {
        HostKind::Ssh { .. } => format!("ssh-remote+{}", host.ssh_remote().unwrap_or_default()),
        HostKind::Wsl { distro } => format!("wsl+{distro}"),
        _ => return Err(unreachable()),
    };
    let editor = preferred(cx).ok_or_else(unreachable)?;
    match editor.goto {
        // VS Code, Cursor, Windsurf.
        Goto::Flag => {
            let mut args: Vec<std::ffi::OsString> = vec!["--remote".into(), remote.into()];
            args.extend(goto_args(editor.goto, path, line));
            spawn(&editor.path, &args, None)
        }
        Goto::Suffix if editor.id == "zed" => match &host.kind {
            HostKind::Ssh { target, port, .. } => {
                let port = port.map(|p| format!(":{p}")).unwrap_or_default();
                let line = line.filter(|l| *l > 0).map(|l| format!(":{l}")).unwrap_or_default();
                let url = format!("ssh://{target}{port}/{}{line}", path.display().to_string().trim_start_matches('/'));
                spawn(&editor.path, &[url.into()], None)
            }
            _ => Err(unreachable()),
        },
        _ => Err(unreachable()),
    }
}

/// Opens `file` at `line` (1-based) in the preferred editor.
pub fn open_file(file: &Path, line: Option<u32>, cx: &mut App) {
    if let Some(host) = remote_host(cx) {
        let result = open_remote(&host, file, line, cx);
        report(result, cx);
        return;
    }
    let prefs = crate::prefs::get(cx);
    let (choice, custom) = (prefs.editor.clone(), prefs.editor_command.clone());
    let result = if choice == CUSTOM && !custom.trim().is_empty() {
        let folder = file.parent().unwrap_or(file);
        run_custom(&custom, Some(file), line, folder)
    } else if let Some(editor) = preferred(cx).filter(|_| choice != SYSTEM) {
        let args = goto_args(editor.goto, file, line);
        spawn(&editor.path, &args, None)
    } else {
        cx.open_with_system(file);
        Ok(())
    };
    report(result, cx);
}

/// Opens a folder (a worktree, a project) in the preferred editor.
pub fn open_folder(folder: &Path, cx: &mut App) {
    if let Some(host) = remote_host(cx) {
        let result = open_remote(&host, folder, None, cx);
        report(result, cx);
        return;
    }
    let prefs = crate::prefs::get(cx);
    let (choice, custom) = (prefs.editor.clone(), prefs.editor_command.clone());
    let result = if choice == CUSTOM && !custom.trim().is_empty() {
        run_custom(&custom, None, None, folder)
    } else if let Some(editor) = preferred(cx).filter(|_| choice != SYSTEM) {
        spawn(&editor.path, &[folder.as_os_str().to_owned()], None)
    } else {
        cx.open_with_system(folder);
        Ok(())
    };
    report(result, cx);
}

/// Opens a terminal window of the system in `folder`: on another
/// computer, an SSH session (or `wsl.exe`) there.
pub fn open_terminal(folder: &Path, cx: &mut App) {
    if let Some(host) = remote_host(cx) {
        use orbit_net::remote::{HostKind, shell_word};
        let dir = shell_word(&folder.display().to_string());
        let command = match &host.kind {
            HostKind::Ssh { .. } => {
                let tools = orbit_net::remote::Tools::default();
                let remote = orbit_net::remote::Remote::new(&host, &tools);
                // `-t` for a terminal; the folder and the login shell are
                // the remote's (`$SHELL` must reach it unexpanded).
                let shell = if cfg!(windows) { "$SHELL" } else { r"\$SHELL" };
                remote.interactive_command().map(|ssh| format!("{} \"cd {dir} && exec {shell} -l\"", ssh.replacen("ssh ", "ssh -t ", 1)))
            }
            HostKind::Wsl { distro } => Some(format!("wsl.exe -d {distro} --cd {dir}")),
            _ => None,
        };
        let result = match command {
            Some(command) => crate::hosts::panel::open_terminal(&command).map_err(|e| format!("Couldn’t open a terminal: {e}")),
            None => Err(format!("{} is on {}; open a terminal there.", folder.display(), host.label())),
        };
        report(result, cx);
        return;
    }
    let result = if cfg!(windows) {
        match orbit_core::provider::process::resolve_binary("wt", None, &[]) {
            Some(wt) => spawn(&wt, &["-d".into(), folder.as_os_str().to_owned()], None),
            // `start` gives the shell a console window of its own.
            None => spawn(Path::new("cmd"), &["/c".into(), "start".into(), "".into(), "cmd".into()], Some(folder)),
        }
    } else if cfg!(target_os = "macos") {
        spawn(Path::new("open"), &["-a".into(), "Terminal".into(), folder.as_os_str().to_owned()], None)
    } else {
        let terminal = std::env::var_os("TERMINAL")
            .map(PathBuf::from)
            .or_else(|| {
                ["x-terminal-emulator", "gnome-terminal", "konsole", "kitty", "alacritty", "wezterm", "xterm"]
                    .iter()
                    .find_map(|name| orbit_core::provider::process::resolve_binary(name, None, &[]))
            });
        match terminal {
            Some(terminal) => spawn(&terminal, &[], Some(folder)),
            None => Err("No terminal app was found. Set $TERMINAL to choose one.".into()),
        }
    };
    report(result, cx);
}

/// Shows `path` selected in the file manager.
pub fn reveal(path: &Path, cx: &mut App) {
    if let Some(host) = remote_host(cx) {
        report(Err(format!("{} is on {}, which this computer’s file manager can’t show.", path.display(), host.label())), cx);
        return;
    }
    cx.reveal_path(path);
}

/// The name of the system file manager, for menu labels.
pub fn file_manager() -> &'static str {
    if cfg!(windows) {
        "Explorer"
    } else if cfg!(target_os = "macos") {
        "Finder"
    } else {
        "the file manager"
    }
}

fn goto_args(goto: Goto, file: &Path, line: Option<u32>) -> Vec<std::ffi::OsString> {
    let file = file.as_os_str().to_owned();
    let Some(line) = line.filter(|l| *l > 0) else { return vec![file] };
    match goto {
        Goto::Flag => {
            let mut target = file;
            target.push(format!(":{line}"));
            vec!["--goto".into(), target]
        }
        Goto::Suffix => {
            let mut target = file;
            target.push(format!(":{line}"));
            vec![target]
        }
        Goto::LineFlag => vec!["--line".into(), line.to_string().into(), file],
    }
}

/// Runs the user's command: `{file}`, `{line}`, `{column}` and `{folder}`
/// are replaced, then it is split like a shell would (quotes respected).
fn run_custom(template: &str, file: Option<&Path>, line: Option<u32>, folder: &Path) -> Result<(), String> {
    let words = split_command(template);
    let Some((program, rest)) = words.split_first() else { return Err("The custom editor command is empty.".into()) };
    let fill = |word: &str| {
        word.replace("{file}", &file.map(|f| f.display().to_string()).unwrap_or_else(|| folder.display().to_string()))
            .replace("{line}", &line.unwrap_or(1).to_string())
            .replace("{column}", "1")
            .replace("{folder}", &folder.display().to_string())
    };
    let mut args: Vec<std::ffi::OsString> = rest.iter().map(|w| fill(w).into()).collect();
    // A command without placeholders gets the file (or folder) at the end.
    if !template.contains('{') {
        args.push(file.unwrap_or(folder).as_os_str().to_owned());
    }
    let program = fill(program);
    let resolved = orbit_core::provider::process::resolve_binary(&program, None, &[]).unwrap_or_else(|| PathBuf::from(&program));
    spawn(&resolved, &args, Some(folder))
}

/// Splits a command line into words: spaces separate, quotes group.
pub(crate) fn split_command(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    for c in text.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => current.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                started = true;
            }
            (None, c) if c.is_whitespace() => {
                if started || !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            (None, c) => current.push(c),
        }
    }
    if started || !current.is_empty() {
        words.push(current);
    }
    words
}

/// Starts a program detached from Orbit, without a console window.
fn spawn(program: &Path, args: &[std::ffi::OsString], cwd: Option<&Path>) -> Result<(), String> {
    let mut command = std::process::Command::new(program);
    command.args(args).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command.spawn().map_err(|e| format!("Couldn’t start {}: {e}", program.display()))?;
    // Reaped in the background so it never lingers as a zombie.
    std::thread::Builder::new()
        .name("orbit-open".into())
        .spawn(move || {
            let _ = child.wait();
        })
        .ok();
    Ok(())
}

fn report(result: Result<(), String>, cx: &mut App) {
    let Err(message) = result else { return };
    log::warn!("{message}");
    if let Some(window) = cx.active_window() {
        let _ = window.update(cx, |_, window, cx| {
            use gpui_kit::component::WindowExt as _;
            use gpui_kit::component::notification::Notification;
            window.push_notification(Notification::error(message), cx);
        });
    }
}

/// Settings › Workspace › "Open files in": the editor files and folders
/// open in, and the custom command.
pub fn settings_group() -> gpui_kit::component::setting::SettingGroup {
    use gpui_kit::component::setting::{SettingField, SettingGroup, SettingItem};
    let editor = SettingField::render(|options: &gpui_kit::component::setting::RenderOptions, _, cx| {
        use gpui_kit::component::Disableable as _;
        use gpui_kit::component::Sizable as _;
        use gpui_kit::component::button::Button;
        use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
        let choice = crate::prefs::get(cx).editor.clone();
        let found: Vec<(String, String)> = installed(cx).iter().map(|e| (e.id.to_owned(), e.label.to_owned())).collect();
        let current = match choice.as_str() {
            SYSTEM => "System default".to_owned(),
            CUSTOM => "Custom command".to_owned(),
            _ => preferred(cx).map(|e| e.label.to_owned()).unwrap_or_else(|| "System default".to_owned()),
        };
        let checked = preferred(cx).map(|e| e.id.to_owned()).filter(|_| choice != SYSTEM && choice != CUSTOM).unwrap_or(choice.clone());
        Button::new("editor-choice")
            .outline()
            .small()
            .label(current)
            .dropdown_caret(true)
            .disabled(options.is_disabled())
            .dropdown_menu(move |mut menu, _, _| {
                for (id, label) in &found {
                    let id2 = id.clone();
                    menu = menu.item(PopupMenuItem::new(label.clone()).checked(checked == *id).on_click(move |_, _, cx| {
                        let id = id2.clone();
                        crate::prefs::update(cx, |p| p.editor = id);
                    }));
                }
                if found.is_empty() {
                    menu = menu.label("No editor found on PATH");
                }
                menu.separator()
                    .item(PopupMenuItem::new("System default").checked(checked == SYSTEM).on_click(|_, _, cx| {
                        crate::prefs::update(cx, |p| p.editor = SYSTEM.into())
                    }))
                    .item(PopupMenuItem::new("Custom command").checked(checked == CUSTOM).on_click(|_, _, cx| {
                        crate::prefs::update(cx, |p| p.editor = CUSTOM.into())
                    }))
            })
            .into_any_element()
    });
    let command = SettingField::input(
        |cx: &App| SharedString::from(crate::prefs::get(cx).editor_command.clone()),
        |value: SharedString, cx: &mut App| {
            let value = value.to_string();
            crate::prefs::update(cx, |p| p.editor_command = value);
        },
    );
    SettingGroup::new().title("Open files in").items([
        SettingItem::new("Editor", editor)
            .description("Files open at their line, folders and worktrees as a whole. Found on this computer: VS Code, Cursor, Windsurf, Zed, Sublime Text and JetBrains IDEs.")
            .keywords(["vscode", "cursor", "zed", "jetbrains", "sublime", "open in editor"]),
        SettingItem::new("Custom command", command)
            .description("Used when the editor is Custom command. {file}, {line}, {column} and {folder} are filled in, e.g. nvim-qt +{line} {file}.")
            .keywords(["editor", "command"]),
    ])
}

#[cfg(test)]
mod tests {
    use super::{Goto, goto_args, split_command};
    use std::path::Path;

    #[test]
    fn editors_are_told_where_to_go_their_own_way() {
        let file = Path::new("src/main.rs");
        let args = |goto, line| -> Vec<String> {
            goto_args(goto, file, line).into_iter().map(|a| a.to_string_lossy().into_owned()).collect()
        };
        assert_eq!(args(Goto::Flag, Some(12)), ["--goto", "src/main.rs:12"]);
        assert_eq!(args(Goto::Suffix, Some(3)), ["src/main.rs:3"]);
        assert_eq!(args(Goto::LineFlag, Some(7)), ["--line", "7", "src/main.rs"]);
        assert_eq!(args(Goto::Flag, None), ["src/main.rs"]);
    }

    #[test]
    fn custom_commands_split_like_a_shell() {
        assert_eq!(split_command(r#"nvim-qt "+{line}" {file}"#), vec!["nvim-qt", "+{line}", "{file}"]);
        assert_eq!(split_command(r#""C:\Program Files\Ed\ed.exe" --at '{line}'"#), vec![r"C:\Program Files\Ed\ed.exe", "--at", "{line}"]);
        assert_eq!(split_command(r#"ed "" x"#), vec!["ed", "", "x"]);
        assert!(split_command("   ").is_empty());
    }
}
