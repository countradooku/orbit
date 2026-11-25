//! Adding a host: an SSH host (typed, or one from `~/.ssh/config`), a WSL
//! distro, or a direct `orbitd` (its address with a token, or a pairing
//! code from that computer's Share page). Adding switches the window to it.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_net::remote::ssh_config::SshHost;
use orbit_net::remote::{HostEntry, HostKind};

use crate::app_state::AppState;
use crate::assets::Lucide;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Ssh,
    Wsl,
    Direct,
}

pub struct AddHostView {
    app: Entity<AppState>,
    kind: Kind,
    kinds: Vec<Kind>,
    target: Entity<InputState>,
    identity: Entity<InputState>,
    url: Entity<InputState>,
    secret: Entity<InputState>,
    /// From `~/.ssh/config`, not added yet.
    suggestions: Vec<SshHost>,
    /// `None` while `wsl.exe -l` runs.
    distros: Option<Vec<String>>,
    distro: Option<String>,
    error: Option<String>,
    /// Pairing: the digits to compare, once the host answered.
    pairing: Option<Option<String>>,
    _tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

/// Opens the Add host dialog.
pub fn open(window: &mut Window, cx: &mut App) {
    let Some(app) = super::app_entity(cx) else { return };
    let view = cx.new(|cx| AddHostView::new(app, window, cx));
    window.open_dialog(cx, move |dialog, _, _| dialog.title("Add host").w(px(560.)).child(view.clone()));
}

impl AddHostView {
    fn new(app: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = |placeholder: &'static str, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        let target = input("user@host, host:port or an alias", window, cx);
        let identity = input("Identity file (optional; your SSH config and agent apply)", window, cx);
        let url = input("ws://100.101.102.103:7840/", window, cx);
        let secret = input("Pairing code (ABCD-EFGH) or access token", window, cx);
        let subscriptions = [&target, &identity, &url, &secret]
            .into_iter()
            .map(|input| {
                cx.subscribe_in(input, window, |this: &mut Self, _, event: &InputEvent, window, cx| match event {
                    InputEvent::PressEnter { .. } => this.add(window, cx),
                    InputEvent::Change => {
                        this.error = None;
                        cx.notify();
                    }
                    _ => {}
                })
            })
            .collect();
        let known: Vec<String> = app.read(cx).hosts().hosts.iter().map(|h| h.id.clone()).collect();
        let suggestions = orbit_net::remote::ssh_config::read_user_config()
            .into_iter()
            .filter(|h| !known.contains(&HostEntry::ssh(&h.alias, None, "").id))
            .collect();
        let mut kinds = vec![Kind::Ssh];
        if cfg!(windows) {
            kinds.push(Kind::Wsl);
        }
        kinds.push(Kind::Direct);
        let tools = app.read(cx).tools().clone();
        let list = cx.background_spawn(async move { orbit_net::remote::wsl::list(&tools) });
        let load = cx.spawn(async move |this, cx| {
            let distros = list.await;
            let _ = this.update(cx, |this, cx| {
                let known: Vec<String> = this.app.read(cx).hosts().hosts.iter().map(|h| h.id.clone()).collect();
                this.distros = Some(distros.into_iter().filter(|d| !known.contains(&HostEntry::wsl(d).id)).collect());
                cx.notify();
            });
        });
        target.update(cx, |t, cx| t.focus(window, cx));
        Self {
            app,
            kind: Kind::Ssh,
            kinds,
            target,
            identity,
            url,
            secret,
            suggestions,
            distros: None,
            distro: None,
            error: None,
            pairing: None,
            _tasks: vec![load],
            _subscriptions: subscriptions,
        }
    }

    /// The host the form describes, or what is missing.
    fn entry(&self, cx: &App) -> Result<HostEntry, String> {
        match self.kind {
            Kind::Ssh => HostEntry::parse_ssh(&self.target.read(cx).value(), &self.identity.read(cx).value()),
            Kind::Wsl => match &self.distro {
                Some(distro) => Ok(HostEntry::wsl(distro)),
                None => Err("Choose a distro.".into()),
            },
            Kind::Direct => {
                let url = normalize_url(&self.url.read(cx).value())?;
                Ok(HostEntry::url(&url, &self.secret.read(cx).value()))
            }
        }
    }

    fn add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pairing.is_some() {
            return;
        }
        let entry = match self.entry(cx) {
            Ok(entry) => entry,
            Err(message) => {
                self.error = Some(message);
                cx.notify();
                return;
            }
        };
        if let HostKind::Url { url, token } = &entry.kind
            && orbit_net::share::looks_like_code(token)
        {
            self.pair(url.clone(), token.clone(), window, cx);
            return;
        }
        if let HostKind::Url { token, .. } = &entry.kind
            && token.trim().is_empty()
        {
            self.error = Some("Enter the pairing code shown on that computer, or its access token.".into());
            cx.notify();
            return;
        }
        self.finish(entry, window, cx);
    }

    fn finish(&mut self, entry: HostEntry, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.app.update(cx, |app, cx| app.add_host(entry, cx));
        window.close_dialog(cx);
        super::switch_to(id.into(), window, cx);
    }

    /// Pairs with a shared computer: shows the digits to compare while the
    /// user there decides.
    fn pair(&mut self, url: String, code: String, window: &mut Window, cx: &mut Context<Self>) {
        self.pairing = Some(None);
        self.error = None;
        cx.notify();
        let (digits_tx, digits_rx) = futures::channel::oneshot::channel::<String>();
        let device = orbit_net::share::device_name();
        let pair_url = url.clone();
        let work = cx.background_spawn(async move {
            orbit_net::share::pair(&pair_url, &code, &device, move |digits| {
                let _ = digits_tx.send(digits);
            })
        });
        let shown = cx.spawn(async move |this, cx| {
            if let Ok(digits) = digits_rx.await {
                let _ = this.update(cx, |this, cx| {
                    if this.pairing.is_some() {
                        this.pairing = Some(Some(digits));
                        cx.notify();
                    }
                });
            }
        });
        let done = cx.spawn_in(window, async move |this, cx| {
            let result = work.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.pairing = None;
                match result {
                    Ok(token) => this.finish(HostEntry::url(&url, &token), window, cx),
                    Err(message) => {
                        this.error = Some(message);
                        cx.notify();
                    }
                }
            });
        });
        self._tasks.extend([shown, done]);
    }

    fn render_ssh(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        v_flex()
            .gap_2()
            .child(Input::new(&self.target))
            .child(Input::new(&self.identity).small())
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("Orbit runs your system’s ssh, so your config, keys, agent and ProxyJump work as in a terminal. It never asks for passwords: if SSH needs one, Orbit shows the command to run once."),
            )
            .when(!self.suggestions.is_empty(), |el| {
                el.child(crate::ui::section_label("From your SSH config", cx).px_0())
                    .child(
                        v_flex()
                            .id("add-host-suggestions")
                            .max_h(px(180.))
                            .overflow_y_scroll()
                            .gap_0p5()
                            .children(self.suggestions.iter().map(|host| {
                                let alias = host.alias.clone();
                                h_flex()
                                    .id(SharedString::from(format!("ssh-suggestion-{}", host.alias)))
                                    .h_8()
                                    .px_2()
                                    .gap_2()
                                    .rounded(theme.radius)
                                    .text_sm()
                                    .cursor_default()
                                    .hover(|s| s.bg(theme.muted))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.target.update(cx, |t, cx| t.set_value(alias.clone(), window, cx));
                                        this.add(window, cx);
                                    }))
                                    .child(Icon::new(Lucide::Server).small().text_color(theme.muted_foreground))
                                    .child(div().min_w_0().truncate().child(host.alias.clone()))
                                    .when_some(host.detail(), |el, detail| {
                                        el.child(div().min_w_0().truncate().text_xs().text_color(theme.muted_foreground).child(detail))
                                    })
                            })),
                    )
            })
    }

    fn render_wsl(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let body: AnyElement = match &self.distros {
            None => h_flex()
                .gap_2()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(Spinner::new().xsmall())
                .child("Looking for WSL distros…")
                .into_any_element(),
            Some(distros) if distros.is_empty() => div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child("No WSL distro to add. Install one with wsl --install, then come back.")
                .into_any_element(),
            Some(distros) => v_flex()
                .gap_0p5()
                .children(distros.iter().map(|distro| {
                    let picked = self.distro.as_ref() == Some(distro);
                    let name = distro.clone();
                    h_flex()
                        .id(SharedString::from(format!("wsl-{distro}")))
                        .h_8()
                        .px_2()
                        .gap_2()
                        .rounded(theme.radius)
                        .text_sm()
                        .cursor_default()
                        .when(picked, |el| el.bg(theme.accent).text_color(theme.accent_foreground))
                        .when(!picked, |el| el.hover(|s| s.bg(theme.muted)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.distro = Some(name.clone());
                            this.error = None;
                            cx.notify();
                        }))
                        .child(Icon::new(Lucide::Terminal).small().text_color(theme.muted_foreground))
                        .child(distro.clone())
                }))
                .into_any_element(),
        };
        v_flex().gap_2().child(body).child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("Agents run inside the distro, on its Linux files. Orbit installs its orbitd there (a Linux build) the first time."),
        )
    }

    fn render_direct(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        v_flex()
            .gap_2()
            .child(Input::new(&self.url))
            .child(Input::new(&self.secret))
            .child(div().text_xs().text_color(theme.muted_foreground).child(
                "On the other computer, open Settings › Hosts › Share this computer and show a pairing code. Or connect to \
                 orbitd --listen with the token orbitd --print-token prints. Over anything but Tailscale or a VPN the \
                 connection isn’t encrypted: prefer SSH there.",
            ))
            .when_some(self.pairing.clone(), |el, digits| {
                el.child(
                    h_flex()
                        .gap_2()
                        .p_3()
                        .rounded(theme.radius)
                        .bg(theme.muted)
                        .text_sm()
                        .child(Spinner::new().xsmall())
                        .child(match digits {
                            None => div().child("Asking the other computer…"),
                            Some(digits) => div().child(format!("Allow this device on the other computer if it shows {digits}.")),
                        }),
                )
            })
    }
}

/// `100.1.2.3:7840` or `ws://host:7840` as a WebSocket URL.
fn normalize_url(text: &str) -> Result<String, String> {
    let text = text.trim().trim_end_matches('/');
    if text.is_empty() {
        return Err("Enter the other computer’s address.".into());
    }
    let with_scheme = if text.contains("://") { text.to_owned() } else { format!("ws://{text}") };
    let rest = with_scheme.strip_prefix("ws://").or_else(|| with_scheme.strip_prefix("wss://"));
    match rest {
        Some(rest) if !rest.is_empty() && !rest.contains(char::is_whitespace) => {}
        _ => return Err("An address is written ws://host:port.".into()),
    }
    let with_port = if rest.is_some_and(|r| r.rsplit_once(':').is_some_and(|(_, p)| p.parse::<u16>().is_ok())) {
        with_scheme
    } else {
        format!("{with_scheme}:{}", orbit_net::DEFAULT_PORT)
    };
    Ok(format!("{with_port}/"))
}

impl Render for AddHostView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let kinds = self.kinds.clone();
        let selected = kinds.iter().position(|k| *k == self.kind).unwrap_or(0);
        let body = match self.kind {
            Kind::Ssh => self.render_ssh(cx).into_any_element(),
            Kind::Wsl => self.render_wsl(cx).into_any_element(),
            Kind::Direct => self.render_direct(cx).into_any_element(),
        };
        let busy = self.pairing.is_some();
        v_flex()
            .gap_3()
            .child(
                TabBar::new("add-host-kind")
                    .segmented()
                    .small()
                    .selected_index(selected)
                    .on_click(cx.listener(move |this, ix: &usize, window, cx| {
                        this.kind = kinds[*ix];
                        this.error = None;
                        match this.kind {
                            Kind::Ssh => this.target.update(cx, |t, cx| t.focus(window, cx)),
                            Kind::Direct => this.url.update(cx, |t, cx| t.focus(window, cx)),
                            Kind::Wsl => {}
                        }
                        cx.notify();
                    }))
                    .children(self.kinds.iter().map(|k| {
                        Tab::new().label(match k {
                            Kind::Ssh => "SSH",
                            Kind::Wsl => "WSL",
                            Kind::Direct => "Direct",
                        })
                    })),
            )
            .child(body)
            .when_some(self.error.clone(), |el, error| el.child(div().text_sm().text_color(theme.danger).child(error)))
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(Button::new("add-host-cancel").outline().label("Cancel").on_click(|_, window, cx| window.close_dialog(cx)))
                    .child(
                        Button::new("add-host-add")
                            .primary()
                            .label(if self.kind == Kind::Direct { "Connect" } else { "Add and connect" })
                            .disabled(busy)
                            .on_click(cx.listener(|this, _, window, cx| this.add(window, cx))),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_url;

    #[test]
    fn addresses_get_a_scheme_and_port() {
        assert_eq!(normalize_url("100.64.1.2").unwrap(), "ws://100.64.1.2:7840/");
        assert_eq!(normalize_url("ws://box:9000/").unwrap(), "ws://box:9000/");
        assert_eq!(normalize_url(" box:7841 ").unwrap(), "ws://box:7841/");
        assert!(normalize_url("").is_err());
        assert!(normalize_url("http://box").is_err());
    }
}
