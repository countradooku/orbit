//! What the window shows for a host it has not reached yet: connecting, or
//! why not and the fix: install `orbitd`, accept a host key or sign in once
//! in a terminal (Orbit never types secrets or answers prompts), or wait for
//! the network.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::clipboard::Clipboard;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_net::ConnectionStatus;
use orbit_net::remote::hosts::LOCAL_ID;
use orbit_net::remote::{FailureKind, LinkPhase, Probe};

use super::kind_icon;
use crate::app_state::AppState;
use crate::assets::Lucide;

/// The shown host is another computer this window has not reached yet.
pub fn shows(app: &Entity<AppState>, cx: &App) -> bool {
    let app = app.read(cx);
    !app.active_entry().is_local() && !app.has_snapshot()
}

enum Situation {
    Connecting,
    Install { outdated: bool },
    RunOnce { why: String, command: String, auth: bool },
    Failed(String),
}

pub fn render(app: &Entity<AppState>, _: &mut Window, cx: &App) -> AnyElement {
    let theme = cx.theme().clone();
    let state = app.read(cx);
    let entry = state.active_entry().clone();
    let label = entry.label();
    // The address, unless the name already says it.
    let address = match &entry.kind {
        orbit_net::remote::HostKind::Ssh { target, port, .. } => format!("{target}{}", port.map(|p| format!(":{p}")).unwrap_or_default()),
        orbit_net::remote::HostKind::Wsl { distro } => distro.clone(),
        orbit_net::remote::HostKind::Url { url, .. } => url.clone(),
        orbit_net::remote::HostKind::Local => String::new(),
    };
    let detail = if label == address { entry.kind_label().to_owned() } else { format!("{} · {address}", entry.kind_label()) };
    let link = state.link().cloned();
    let situation = match (link.as_ref().map(|l| l.phase()), state.connection()) {
        (Some(LinkPhase::NeedsInstall(probe)), _) => Situation::Install { outdated: matches!(probe, Probe::Outdated(_)) },
        (Some(LinkPhase::Failed(f)), _) if matches!(f.kind, FailureKind::Auth | FailureKind::HostKey) && f.run_once.is_some() => {
            Situation::RunOnce { why: f.message.clone(), command: f.run_once.clone().unwrap_or_default(), auth: f.kind == FailureKind::Auth }
        }
        (Some(LinkPhase::Failed(f)), _) => Situation::Failed(f.message.clone()),
        (_, Some(ConnectionStatus::Disconnected { reason })) => Situation::Failed(reason.clone()),
        _ => Situation::Connecting,
    };
    let retry_app = app.clone();
    let retry = move |id: &'static str, primary: bool| {
        let app = retry_app.clone();
        Button::new(id)
            .when(primary, |b| b.primary())
            .when(!primary, |b| b.outline())
            .icon(Icon::new(Lucide::RefreshCw))
            .label("Try again")
            .on_click(move |_, _, cx| app.read(cx).retry())
    };
    let body: AnyElement = match situation {
        Situation::Connecting => h_flex()
            .gap_2()
            .text_sm()
            .text_color(theme.muted_foreground)
            .child(Spinner::new().xsmall())
            .child(format!("Connecting to {label}…"))
            .into_any_element(),
        Situation::Install { outdated } => {
            let install_app = app.clone();
            v_flex()
                .gap_3()
                .items_center()
                .child(
                    div()
                        .text_sm()
                        .text_center()
                        .text_color(theme.muted_foreground)
                        .child(match outdated {
                            true => format!("{label} has orbitd from another version of Orbit. Install this version’s to connect."),
                            false => format!("orbitd isn’t installed on {label}. Orbit runs agents there through it."),
                        }),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("host-install")
                                .primary()
                                .icon(Icon::new(Lucide::Download))
                                .label(if outdated { "Update orbitd…" } else { "Install orbitd…" })
                                .on_click(move |_, window, cx| super::install::open(&install_app, window, cx)),
                        )
                        .child(retry("host-retry", false)),
                )
                .into_any_element()
        }
        Situation::RunOnce { why, command, auth } => {
            let run = command.clone();
            v_flex()
                .gap_3()
                .items_center()
                .max_w(px(520.))
                .child(div().text_sm().text_center().text_color(theme.muted_foreground).child(why))
                .child(
                    h_flex()
                        .gap_2()
                        .px_3()
                        .py_2()
                        .rounded(theme.radius)
                        .bg(theme.muted)
                        .child(div().text_sm().font_family(theme.mono_font_family.clone()).child(command.clone()))
                        .child(Clipboard::new("host-run-once").value(command.clone())),
                )
                .when(auth, |el| {
                    el.child(
                        div()
                            .text_xs()
                            .text_center()
                            .text_color(theme.muted_foreground)
                            .child("With a key that has a passphrase, ssh-add once keeps it in your SSH agent so Orbit can use it."),
                    )
                })
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("host-open-terminal")
                                .primary()
                                .icon(Icon::new(Lucide::Terminal))
                                .label("Open in terminal")
                                .on_click(move |_, window, cx| {
                                    if let Err(err) = open_terminal(&run) {
                                        window.push_notification(
                                            gpui_kit::component::notification::Notification::warning(format!(
                                                "Couldn’t open a terminal ({err}). Copy the command and run it in yours."
                                            )),
                                            cx,
                                        );
                                    }
                                }),
                        )
                        .child(retry("host-retry", false)),
                )
                .into_any_element()
        }
        Situation::Failed(message) => v_flex()
            .gap_3()
            .items_center()
            .max_w(px(520.))
            .child(div().text_sm().text_center().text_color(theme.muted_foreground).child(message))
            .child(div().text_xs().text_color(theme.muted_foreground).child("Orbit keeps trying in the background."))
            .child(retry("host-retry", true))
            .into_any_element(),
    };
    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .gap_4()
        .px_6()
        .child(
            v_flex()
                .items_center()
                .gap_1()
                .child(Icon::new(kind_icon(&entry)).large().text_color(theme.muted_foreground))
                .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child(label))
                .child(div().text_xs().text_color(theme.muted_foreground).child(detail)),
        )
        .child(body)
        .child(
            Button::new("host-back-local")
                .ghost()
                .small()
                .icon(Icon::new(Lucide::Laptop))
                .label("Show this computer")
                .on_click(|_, window, cx| super::switch_to(LOCAL_ID.into(), window, cx)),
        )
        .into_any_element()
}

/// Opens the system terminal running `command`, for the user to answer
/// what SSH asks (a host key, a passphrase, a password) themselves.
pub fn open_terminal(command: &str) -> std::io::Result<()> {
    use std::process::Command;
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        // `start` opens a new console window; `cmd /k` keeps it open after.
        Command::new("cmd.exe").raw_arg(format!("/c start \"Orbit: connect once\" cmd /k {command}")).spawn().map(drop)
    }
    #[cfg(target_os = "macos")]
    {
        let script = format!("tell application \"Terminal\" to do script \"{}\"", command.replace('\\', "\\\\").replace('"', "\\\""));
        Command::new("osascript").args(["-e", &script, "-e", "tell application \"Terminal\" to activate"]).spawn().map(drop)
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let shell_line = format!("{command}; exec \"${{SHELL:-sh}}\"");
        let candidates: [(&str, &[&str]); 4] = [
            ("x-terminal-emulator", &["-e", "sh", "-c"]),
            ("gnome-terminal", &["--", "sh", "-c"]),
            ("konsole", &["-e", "sh", "-c"]),
            ("xterm", &["-e", "sh", "-c"]),
        ];
        let mut last = std::io::Error::new(std::io::ErrorKind::NotFound, "no terminal found");
        for (program, args) in candidates {
            match Command::new(program).args(args).arg(&shell_line).spawn() {
                Ok(_) => return Ok(()),
                Err(err) => last = err,
            }
        }
        Err(last)
    }
}
