//! Settings › Hosts › Share this computer: let other devices reach this
//! computer's background host over the network, paired with a short code
//! (see `orbit_net::share` and `docs/REMOTE.md` for the threat model).
//!
//! The host keeps the state; this page shows its [`ShareStatus`], read on
//! demand and every second while a code or a request is waiting.

use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::clipboard::Clipboard;
use gpui_kit::component::setting::{SettingField, SettingGroup, SettingItem};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_proto::*;

use crate::app_state::AppState;

/// The host's sharing as last read.
#[derive(Default)]
struct Model {
    status: Option<Result<ShareStatus, String>>,
    reading: bool,
    poll: Option<Task<()>>,
}

impl Global for Model {}

fn status(cx: &App) -> Option<Result<ShareStatus, String>> {
    cx.try_global::<Model>().and_then(|m| m.status.clone())
}

/// Sends `command` to the shown host and keeps its answer.
fn send(command: ShareCommand, cx: &mut App) {
    let Some(app) = super::app_entity(cx) else { return };
    if !app.read(cx).active_entry().is_local() {
        return;
    }
    cx.default_global::<Model>().reading = true;
    let reply = app.update(cx, |app, cx| app.call(Command::Host(HostCommand::Share(command)), cx));
    cx.spawn(async move |cx| {
        let result = reply.await;
        cx.update(|cx| {
            let model = cx.default_global::<Model>();
            model.reading = false;
            model.status = Some(match result {
                Ok(Reply::Host(HostReply::Share(status))) => Ok(status),
                Ok(_) => Err("This host can’t share; update Orbit.".into()),
                Err(message) => Err(message),
            });
            keep_polling(cx);
            cx.refresh_windows();
        });
    })
    .detach();
}

/// Reads the status every second while a code or a request is waiting.
fn keep_polling(cx: &mut App) {
    let waiting = matches!(status(cx), Some(Ok(s)) if s.code.is_some() || !s.pending.is_empty());
    let model = cx.default_global::<Model>();
    if !waiting {
        model.poll = None;
        return;
    }
    if model.poll.is_some() {
        return;
    }
    let task = cx.spawn(async move |cx| {
        cx.background_executor().timer(Duration::from_secs(1)).await;
        cx.update(|cx| {
            cx.default_global::<Model>().poll = None;
            send(ShareCommand::Status, cx);
        });
    });
    cx.default_global::<Model>().poll = Some(task);
}

/// Reads the status once when the page first shows it.
fn ensure_read(cx: &mut App) {
    let model = cx.default_global::<Model>();
    if model.status.is_none() && !model.reading {
        send(ShareCommand::Status, cx);
    }
}

pub fn group(app: &Entity<AppState>, cx: &App) -> SettingGroup {
    let local = app.read(cx).active_entry().is_local();
    SettingGroup::new()
        .title("Share this computer")
        .description(if local {
            "Let Orbit on your other devices run agents here. Off until you turn it on; each device pairs with a code you \
             allow here, and you can remove it at any time. Prefer SSH, or Tailscale, which encrypts the connection."
        } else {
            "Shown for This computer. Switch to it to share it."
        })
        .items([
            SettingItem::new(
                "Share",
                SettingField::switch(
                    |cx| matches!(status(cx), Some(Ok(s)) if s.enabled),
                    |on, cx| {
                        let listen = match status(cx) {
                            Some(Ok(s)) if !s.listen.is_empty() => s.listen,
                            Some(Ok(s)) => default_listen(&s),
                            _ => format!("0.0.0.0:{}", orbit_net::DEFAULT_PORT),
                        };
                        send(if on { ShareCommand::Enable { listen } } else { ShareCommand::Disable }, cx);
                    },
                ),
            )
            .disabled(!local)
            .description("The background host listens for paired devices, and keeps running for them while this is on.")
            .keywords(["remote", "pair", "tailscale", "lan", "network", "listen"]),
            SettingItem::new("Address", SettingField::render(|options, _, cx| address(options.is_disabled(), cx)))
                .layout(Axis::Vertical)
                .disabled(!local),
            SettingItem::new("Pair a device", SettingField::render(|options, _, cx| pairing(options.is_disabled(), cx)))
                .layout(Axis::Vertical)
                .disabled(!local)
                .description("On the other device: Add host › Direct, with this computer’s address and the code. Both screens then show the same six digits; allow it here if they match."),
            SettingItem::new("Paired devices", SettingField::render(|options, _, cx| devices(options.is_disabled(), cx)))
                .layout(Axis::Vertical)
                .disabled(!local)
                .description("Removing a device closes its connection at once; it can pair again with a new code."),
        ])
}

/// Tailscale's address when there is one, else every network.
fn default_listen(status: &ShareStatus) -> String {
    let ip = status.addresses.iter().find(|a| a.encrypted).map(|a| a.ip.clone()).unwrap_or_else(|| "0.0.0.0".into());
    format!("{ip}:{}", orbit_net::DEFAULT_PORT)
}

fn muted(text: impl Into<SharedString>, cx: &App) -> AnyElement {
    div().text_sm().text_color(cx.theme().muted_foreground).child(text.into()).into_any_element()
}

fn address(disabled: bool, cx: &mut App) -> AnyElement {
    if disabled {
        return muted("Not shared.", cx);
    }
    ensure_read(cx);
    let status = match status(cx) {
        None => return muted("Reading…", cx),
        Some(Err(message)) => return muted(message, cx),
        Some(Ok(status)) => status,
    };
    let theme = cx.theme().clone();
    let port = status.listen.rsplit_once(':').map(|(_, p)| p.to_owned()).unwrap_or_else(|| orbit_net::DEFAULT_PORT.to_string());
    let listening_ip = status.listen.rsplit_once(':').map(|(ip, _)| ip.to_owned()).unwrap_or_default();
    let mut choices: Vec<(String, String, bool)> =
        status.addresses.iter().map(|a| (a.ip.clone(), format!("{} · {}", a.label, a.ip), a.encrypted)).collect();
    choices.push(("0.0.0.0".into(), "Every network".into(), false));
    let shown: Vec<(String, String)> = status
        .addresses
        .iter()
        .filter(|a| listening_ip == "0.0.0.0" || a.ip == listening_ip)
        .map(|a| (format!("ws://{}:{port}/", a.ip), a.label.clone()))
        .collect();
    let encrypted = status.addresses.iter().any(|a| a.ip == listening_ip && a.encrypted);
    v_flex()
        .w_full()
        .gap_2()
        .child(
            h_flex().gap_1().flex_wrap().children(choices.into_iter().map(|(ip, label, _)| {
                let selected = status.enabled && ip == listening_ip;
                let listen = format!("{ip}:{port}");
                Button::new(SharedString::from(format!("share-on-{ip}")))
                    .small()
                    .when(selected, |b| b.primary())
                    .when(!selected, |b| b.outline())
                    .label(label)
                    .on_click(move |_, _, cx| send(ShareCommand::Enable { listen: listen.clone() }, cx))
            })),
        )
        .when_some(status.error.clone(), |el, error| el.child(div().text_sm().text_color(theme.danger).child(error)))
        .when(status.enabled && status.error.is_none(), |el| {
            el.children(shown.into_iter().map(|(url, label)| {
                h_flex()
                    .gap_2()
                    .text_sm()
                    .child(div().font_family(theme.mono_font_family.clone()).child(url.clone()))
                    .child(div().text_xs().text_color(theme.muted_foreground).child(label))
                    .child(Clipboard::new(SharedString::from(format!("share-url-{url}"))).value(url))
            }))
            .when(!encrypted, |el| {
                el.child(div().text_xs().text_color(theme.warning).child(
                    "Traffic on this network isn’t encrypted: someone on it could read it. Use Tailscale, or SSH, for anything sensitive.",
                ))
            })
        })
        .into_any_element()
}

fn pairing(disabled: bool, cx: &mut App) -> AnyElement {
    let status = match status(cx) {
        Some(Ok(status)) if status.enabled && !disabled => status,
        _ => return muted("Turn sharing on to pair a device.", cx),
    };
    let theme = cx.theme().clone();
    let now = now_ms();
    v_flex()
        .w_full()
        .gap_2()
        .child(match &status.code {
            Some(code) => {
                let left = code.expires_ms.saturating_sub(now).div_ceil(60_000);
                h_flex()
                    .gap_3()
                    .child(div().text_xl().font_weight(FontWeight::SEMIBOLD).font_family(theme.mono_font_family.clone()).child(code.code.clone()))
                    .child(div().text_xs().text_color(theme.muted_foreground).child(format!("works for {left} min, for one device")))
                    .child(Button::new("share-new-code").ghost().small().label("New code").on_click(|_, _, cx| send(ShareCommand::NewCode, cx)))
                    .into_any_element()
            }
            None => Button::new("share-show-code")
                .outline()
                .small()
                .label("Show a pairing code")
                .on_click(|_, _, cx| send(ShareCommand::NewCode, cx))
                .into_any_element(),
        })
        .children(status.pending.iter().map(|request| {
            let (allow, deny) = (request.id, request.id);
            h_flex()
                .w_full()
                .gap_3()
                .p_2()
                .rounded(theme.radius)
                .bg(theme.muted)
                .text_sm()
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(div().truncate().child(format!("{} asks to connect", request.device)))
                        .child(div().text_xs().text_color(theme.muted_foreground).child(format!("from {}", request.from))),
                )
                .child(div().font_family(theme.mono_font_family.clone()).text_lg().child(request.verify.clone()))
                .child(
                    Button::new(SharedString::from(format!("share-allow-{}", request.id)))
                        .primary()
                        .small()
                        .label("Allow")
                        .tooltip("Only if the other device shows the same digits")
                        .on_click(move |_, _, cx| send(ShareCommand::Confirm { request: allow, accept: true }, cx)),
                )
                .child(
                    Button::new(SharedString::from(format!("share-deny-{}", request.id)))
                        .ghost()
                        .small()
                        .label("Deny")
                        .on_click(move |_, _, cx| send(ShareCommand::Confirm { request: deny, accept: false }, cx)),
                )
        }))
        .into_any_element()
}

fn devices(disabled: bool, cx: &mut App) -> AnyElement {
    let status = match status(cx) {
        Some(Ok(status)) if !disabled => status,
        _ => return muted("No devices.", cx),
    };
    if status.devices.is_empty() {
        return muted("No device is paired.", cx);
    }
    let theme = cx.theme().clone();
    let now = now_ms();
    v_flex()
        .w_full()
        .gap_1()
        .children(status.devices.iter().map(|device| {
            let seen = if device.connected {
                "connected now".to_owned()
            } else {
                match crate::format::relative_time(device.last_seen_ms, now) {
                    now if now == "now" => "seen just now".to_owned(),
                    rel => format!("seen {rel} ago"),
                }
            };
            let id = device.id.clone();
            h_flex()
                .w_full()
                .gap_3()
                .text_sm()
                .child(super::dot(if device.connected { super::Tone::Good } else { super::Tone::Idle }, cx))
                .child(div().flex_1().min_w_0().truncate().child(device.name.clone()))
                .child(div().text_color(theme.muted_foreground).child(seen))
                .child(
                    Button::new(SharedString::from(format!("share-revoke-{}", device.id)))
                        .ghost()
                        .small()
                        .label("Remove")
                        .on_click(move |_, _, cx| send(ShareCommand::Revoke { device: id.clone() }, cx)),
                )
        }))
        .into_any_element()
}
