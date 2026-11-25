//! Settings › Bots: connect Telegram, Slack and Discord, let people in,
//! decide how chats start threads, and edit the guard list.
//!
//! Every page in Settings keeps a fixed list of items (field state is keyed
//! by position), so the lists that grow and shrink (people, guard rules) are
//! drawn inside one item each.

use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::clipboard::Clipboard;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::link::Link;
use gpui_kit::component::setting::{RenderOptions, SettingField, SettingGroup, SettingItem, SettingPage};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_proto::*;
use orbit_proto::Platform;

use super::{SLACK_MANIFEST, edit_guard, edit_options, guard, send, send_optimistic, state, unavailable};
use crate::app_state::AppState;
use crate::assets::Lucide;
use crate::format::relative_time;

pub fn page(app: &Entity<AppState>, cx: &mut App) -> SettingPage {
    super::ensure_loaded(cx);
    let off = unavailable(cx).is_some();
    let description = match unavailable(cx) {
        Some(reason) => reason.to_owned(),
        None => "Drive agents from Telegram, Slack and Discord. Orbit connects out to each platform, so nothing on \
                 this computer is opened to the internet."
            .to_owned(),
    };
    SettingPage::new("Bots").icon(Icon::new(IconName::Bot)).resettable(false).description(description).groups([
        platform_group(Platform::Telegram, off, cx),
        platform_group(Platform::Slack, off, cx),
        platform_group(Platform::Discord, off, cx),
        people(off),
        chats(app, off, cx),
        guard_group(),
    ])
}

// ── Platforms ────────────────────────────────────────────────────────────

fn platform_group(platform: Platform, off: bool, cx: &App) -> SettingGroup {
    let current = state(cx).map(|s| s.platform(platform)).unwrap_or_else(|| PlatformState::new(platform));
    let error = current.error.as_ref().map(|e| {
        let ago = match relative_time(current.error_ms, now_ms()) {
            now if now == "now" => "just now".to_owned(),
            rel => format!("{rel} ago"),
        };
        format!("Last error, {ago}: {e}")
    });
    let status = SettingItem::new("Status", status_field(platform))
        .description(error.unwrap_or_else(|| "Turn the bot on once its token is saved.".to_owned()));
    let enabled = SettingItem::new(
        "Enabled",
        SettingField::switch(
            move |cx| state(cx).is_some_and(|s| s.platform(platform).enabled),
            move |enabled, cx| {
                send_optimistic(
                    |s| match s.platforms.iter_mut().find(|p| p.platform == platform) {
                        Some(p) => p.enabled = enabled,
                        None => s.platforms.push(PlatformState { enabled, ..PlatformState::new(platform) }),
                    },
                    BotCommand::Enable { platform, enabled },
                    cx,
                )
            },
        ),
    )
    .disabled(off);
    let hint = |hint: &str, what: &str| {
        if hint.is_empty() { format!("No {what} saved.") } else { format!("Saved {hint}. Paste a new one to replace it.") }
    };
    let group = SettingGroup::new().title(platform.label()).items([status, enabled]);
    match platform {
        Platform::Telegram => group.items([
            SettingItem::new("Bot token", token_field(platform, false, "123456789:AA…"))
                .description(format!("{} Kept in Orbit’s data folder, never in logs.", hint(&current.token_hint, "token")))
                .disabled(off)
                .keywords(["telegram", "botfather", "token"]),
            steps_item(platform),
        ]),
        Platform::Slack => group.items([
            SettingItem::new("Bot token", token_field(platform, false, "xoxb-…"))
                .description(hint(&current.token_hint, "bot token"))
                .disabled(off)
                .keywords(["slack", "xoxb", "token"]),
            SettingItem::new("App-level token", token_field(platform, true, "xapp-…"))
                .description(format!(
                    "{} Socket Mode uses it to connect without a public address.",
                    hint(&current.app_token_hint, "app-level token")
                ))
                .disabled(off)
                .keywords(["slack", "xapp", "socket mode"]),
            steps_item(platform),
        ]),
        Platform::Discord => group.items([
            SettingItem::new("Bot token", token_field(platform, false, "Token"))
                .description(format!("{} Kept in Orbit’s data folder, never in logs.", hint(&current.token_hint, "token")))
                .disabled(off)
                .keywords(["discord", "token"]),
            steps_item(platform),
        ]),
    }
}

/// A dot and a word for the connection; the word carries the meaning, the
/// color only repeats it.
fn status_field(platform: Platform) -> SettingField<SharedString> {
    SettingField::render(move |_, _, cx| {
        let theme = cx.theme().clone();
        let current = state(cx).map(|s| s.platform(platform));
        let (label, color) = match current.as_ref().map(|p| (&p.connection, p.account.as_str())) {
            None if unavailable(cx).is_some() => ("Unavailable".to_owned(), theme.muted_foreground),
            None => ("Checking…".to_owned(), theme.muted_foreground),
            Some((BotConnection::Off, _)) => ("Off".to_owned(), theme.muted_foreground),
            Some((BotConnection::NeedsToken, _)) => ("Needs a token".to_owned(), theme.warning),
            Some((BotConnection::Connecting, _)) => ("Connecting…".to_owned(), theme.muted_foreground),
            Some((BotConnection::Connected, "")) => ("Connected".to_owned(), theme.success),
            Some((BotConnection::Connected, account)) => (format!("Connected as {account}"), theme.success),
            Some((BotConnection::Retrying { attempt, .. }, _)) => (format!("Retrying (attempt {attempt})"), theme.warning),
            Some((BotConnection::Rejected, _)) => ("Token rejected".to_owned(), theme.danger),
        };
        h_flex().gap_1p5().text_sm().child(div().size_2().rounded_full().bg(color)).child(label)
    })
}

struct TokenFieldState {
    input: Entity<InputState>,
    _enter: Subscription,
}

/// A masked token entry. Saved tokens never come back from the engine, so
/// the field starts empty; Save (or Enter) sends what was typed and clears
/// it. The main token's field also offers Remove once a token is saved.
fn token_field(platform: Platform, app_token: bool, placeholder: &'static str) -> SettingField<SharedString> {
    SettingField::render(move |options: &RenderOptions, window, cx| {
        let key = SharedString::from(format!("bot-{}-{}", platform.id(), if app_token { "app-token" } else { "token" }));
        let state_entity = window.use_keyed_state(key.clone(), cx, move |window, cx| {
            let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder).masked(true));
            let _enter = cx.subscribe_in(&input, window, move |_, input, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    save_token(platform, app_token, input, window, cx);
                }
            });
            TokenFieldState { input, _enter }
        });
        let input = state_entity.read(cx).input.clone();
        let typed = !input.read(cx).value().trim().is_empty();
        let saved = state(cx).is_some_and(|s| {
            let p = s.platform(platform);
            !p.token_hint.is_empty() || !p.app_token_hint.is_empty()
        });
        let disabled = options.is_disabled();
        let save_input = input.clone();
        h_flex()
            .gap_2()
            .child(Input::new(&input).with_size(options.size()).disabled(disabled).mask_toggle().w_64())
            .child(
                Button::new(SharedString::from(format!("{key}-save")))
                    .outline()
                    .small()
                    .label("Save")
                    .disabled(disabled || !typed)
                    .on_click(move |_, window, cx| save_token(platform, app_token, &save_input, window, cx)),
            )
            .when(!app_token && saved, |row| {
                row.child(
                    Button::new(SharedString::from(format!("{key}-remove")))
                        .ghost()
                        .small()
                        .label(if platform == Platform::Slack { "Remove tokens" } else { "Remove" })
                        .disabled(disabled)
                        .on_click(move |_, _, cx| {
                            send(
                                BotCommand::SetTokens {
                                    platform,
                                    token: String::new(),
                                    app_token: String::new(),
                                    clear: true,
                                },
                                cx,
                            )
                        }),
                )
            })
    })
}

fn save_token(platform: Platform, app_token: bool, input: &Entity<InputState>, window: &mut Window, cx: &mut App) {
    let typed = input.read(cx).value().trim().to_owned();
    if typed.is_empty() {
        return;
    }
    input.update(cx, |input, cx| input.set_value("", window, cx));
    let (token, app) = if app_token { (String::new(), typed) } else { (typed, String::new()) };
    send(BotCommand::SetTokens { platform, token, app_token: app, clear: false }, cx);
}

/// One numbered step: text and, optionally, a control (a link or a copy
/// button) after it.
struct Step {
    text: &'static str,
    extra: Option<StepExtra>,
}

enum StepExtra {
    Link { id: &'static str, label: &'static str, url: &'static str },
    CopyManifest,
}

const fn step(text: &'static str) -> Step {
    Step { text, extra: None }
}

const fn link(text: &'static str, id: &'static str, label: &'static str, url: &'static str) -> Step {
    Step { text, extra: Some(StepExtra::Link { id, label, url }) }
}

fn steps(platform: Platform) -> &'static [Step] {
    const TELEGRAM: &[Step] = &[
        link("In Telegram, open", "bot-tg-botfather", "@BotFather", "https://t.me/BotFather"),
        step("Send /newbot and pick a name and a username ending in “bot”."),
        step("Copy the token BotFather replies with and save it in Bot token above."),
        step("Turn the bot on, then write to it. Approve the pairing code it sends you under People."),
    ];
    const SLACK: &[Step] = &[
        link("Open", "bot-slack-apps", "api.slack.com/apps", "https://api.slack.com/apps"),
        Step { text: "Create New App › From a manifest, pick the workspace and paste this manifest:", extra: Some(StepExtra::CopyManifest) },
        step("Install the app to the workspace, then copy the Bot User OAuth Token (xoxb-) into Bot token above."),
        step("Under Basic Information › App-Level Tokens, generate one with the connections:write scope and save it as the App-level token."),
        step("Turn the bot on, then message it directly or @mention it in a channel. Approve the pairing code under People."),
    ];
    const DISCORD: &[Step] = &[
        link("Open the", "bot-discord-portal", "Discord developer portal", "https://discord.com/developers/applications"),
        step("Create an application. Under Bot, reset and copy the token, then save it in Bot token above."),
        step("On the same page, turn on Message Content Intent."),
        step("Under OAuth2 › URL Generator, choose the bot and applications.commands scopes and the Send Messages, Read Message History and Attach Files permissions. Open the URL to add the bot to your server."),
        step("Turn the bot on, then message it directly or @mention it. Approve the pairing code under People."),
    ];
    match platform {
        Platform::Telegram => TELEGRAM,
        Platform::Slack => SLACK,
        Platform::Discord => DISCORD,
    }
}

fn steps_item(platform: Platform) -> SettingItem {
    SettingItem::new(
        "Set up",
        SettingField::render(move |_, _, cx| {
            let muted = cx.theme().muted_foreground;
            v_flex().gap_1p5().w_full().children(steps(platform).iter().enumerate().map(|(ix, s)| {
                let extra = s.extra.as_ref().map(|extra| match extra {
                    StepExtra::Link { id, label, url } => {
                        Link::new(*id).href(*url).child(*label).into_any_element()
                    }
                    StepExtra::CopyManifest => Clipboard::new("bot-slack-manifest")
                        .label("Copy manifest")
                        .tooltip("Copy the Slack app manifest")
                        .value(SLACK_MANIFEST)
                        .into_any_element(),
                });
                h_flex()
                    .items_start()
                    .gap_2()
                    .text_sm()
                    .child(div().w_4().flex_none().text_color(muted).child(format!("{}.", ix + 1)))
                    .child(h_flex().flex_1().min_w_0().flex_wrap().gap_1().child(s.text).children(extra))
            }))
        }),
    )
    .layout(Axis::Vertical)
    .keywords(["setup", "how to", "manifest"])
}

// ── People ───────────────────────────────────────────────────────────────

fn people(off: bool) -> SettingGroup {
    SettingGroup::new()
        .title("People")
        .description("Only people you let in can use the bots. Anyone else who writes to one gets a six-digit code and nothing more.")
        .items([
            SettingItem::new("Waiting to pair", SettingField::render(|options, _, cx| pending_list(options.is_disabled(), cx)))
                .layout(Axis::Vertical)
                .description("Approve a code here, or run orbit bot pair <code> on this host. Codes expire after 15 minutes.")
                .disabled(off)
                .keywords(["pairing", "code", "approve"]),
            SettingItem::new("Paired", SettingField::render(|options, _, cx| paired_list(options.is_disabled(), cx)))
                .layout(Axis::Vertical)
                .description("Revoking stops their chats at once. They can ask to pair again.")
                .disabled(off)
                .keywords(["allowlist", "revoke", "users"]),
        ])
}

fn empty_line(text: &'static str, cx: &App) -> AnyElement {
    div().text_sm().text_color(cx.theme().muted_foreground).child(text).into_any_element()
}

fn pending_list(disabled: bool, cx: &App) -> AnyElement {
    let pending = state(cx).map(|s| s.pending.clone()).unwrap_or_default();
    if pending.is_empty() {
        return empty_line("No one is waiting.", cx);
    }
    let theme = cx.theme().clone();
    let now = now_ms();
    v_flex()
        .w_full()
        .gap_1()
        .children(pending.into_iter().map(|request| {
            let left = request.expires_ms.saturating_sub(now) / 60_000;
            let expires = if left == 0 { "expires in under a minute".to_owned() } else { format!("expires in {left}m") };
            let (approve, reject) = (request.code.clone(), request.code.clone());
            h_flex()
                .w_full()
                .gap_3()
                .text_sm()
                .child(div().flex_1().min_w_0().truncate().child(format!("{} · {}", request.name, request.platform.label())))
                .child(div().font_family(theme.mono_font_family.clone()).child(request.code.clone()))
                .child(div().text_color(theme.muted_foreground).child(expires))
                .child(
                    Button::new(SharedString::from(format!("bot-approve-{}", request.code)))
                        .outline()
                        .small()
                        .label("Approve")
                        .disabled(disabled)
                        .on_click(move |_, _, cx| send(BotCommand::Approve { code: approve.clone() }, cx)),
                )
                .child(
                    Button::new(SharedString::from(format!("bot-reject-{}", request.code)))
                        .ghost()
                        .small()
                        .label("Reject")
                        .disabled(disabled)
                        .on_click(move |_, _, cx| send(BotCommand::Reject { code: reject.clone() }, cx)),
                )
        }))
        .into_any_element()
}

fn paired_list(disabled: bool, cx: &App) -> AnyElement {
    let paired = state(cx).map(|s| s.paired.clone()).unwrap_or_default();
    if paired.is_empty() {
        return empty_line("No one yet. Write to a bot, then approve your own code above.", cx);
    }
    let theme = cx.theme().clone();
    let now = now_ms();
    v_flex()
        .w_full()
        .gap_1()
        .children(paired.into_iter().map(|user| {
            let ago = match relative_time(user.paired_ms, now) {
                now if now == "now" => "paired just now".to_owned(),
                rel => format!("paired {rel} ago"),
            };
            let (platform, user_id) = (user.platform, user.user_id.clone());
            h_flex()
                .w_full()
                .gap_3()
                .text_sm()
                .child(div().flex_1().min_w_0().truncate().child(format!("{} · {}", user.name, user.platform.label())))
                .child(
                    div()
                        .font_family(theme.mono_font_family.clone())
                        .text_color(theme.muted_foreground)
                        .child(user.user_id.clone()),
                )
                .child(div().text_color(theme.muted_foreground).child(ago))
                .child(
                    Button::new(SharedString::from(format!("bot-revoke-{}-{}", user.platform.id(), user.user_id)))
                        .ghost()
                        .small()
                        .label("Revoke")
                        .disabled(disabled)
                        .on_click(move |_, _, cx| send(BotCommand::Revoke { platform, user_id: user_id.clone() }, cx)),
                )
        }))
        .into_any_element()
}

// ── Chats ────────────────────────────────────────────────────────────────

/// A dropdown over bot options, stored by position.
fn option_choice<T: Copy + PartialEq + 'static>(
    options: Vec<(T, SharedString)>,
    value: impl Fn(&BotOptions) -> T + 'static,
    set: impl Fn(&mut BotOptions, T) + 'static,
) -> SettingField<SharedString> {
    let values: Rc<Vec<T>> = Rc::new(options.iter().map(|(v, _)| *v).collect());
    let items = options.into_iter().enumerate().map(|(ix, (_, label))| (SharedString::from(ix.to_string()), label)).collect();
    let read = values.clone();
    SettingField::dropdown(
        items,
        move |cx| {
            let current = value(&state(cx).map(|s| s.options.clone()).unwrap_or_default());
            SharedString::from(read.iter().position(|v| *v == current).unwrap_or(0).to_string())
        },
        move |picked: SharedString, cx| {
            if let Some(&v) = picked.parse::<usize>().ok().and_then(|ix| values.get(ix)) {
                edit_options(|o| set(o, v), cx);
            }
        },
    )
}

fn option_switch(value: fn(&BotOptions) -> bool, set: fn(&mut BotOptions, bool)) -> SettingField<bool> {
    SettingField::switch(
        move |cx| value(&state(cx).map(|s| s.options.clone()).unwrap_or_default()),
        move |v, cx| edit_options(|o| set(o, v), cx),
    )
}

fn chats(app: &Entity<AppState>, off: bool, cx: &App) -> SettingGroup {
    // Project ids by position: 0 is "Ask each time".
    let projects: Vec<(Option<ProjectId>, SharedString)> = std::iter::once((None, "Ask each time".into()))
        .chain(app.read(cx).state().projects.iter().map(|p| (Some(p.id), p.name.clone().into())))
        .collect();
    SettingGroup::new().title("Chats").items([
        SettingItem::new(
            "Project for new chats",
            option_choice(projects, |o| o.project, |o, v| o.project = v),
        )
        .description("Where /new starts a thread when the message names no project.")
        .disabled(off)
        .keywords(["default project", "/new"]),
        SettingItem::new(
            "Access",
            option_choice(
                [Access::Readonly, Access::Ask, Access::AutoEdit].into_iter().map(|a| (a, a.label().into())).collect(),
                |o| o.access,
                |o, v| o.access = v,
            ),
        )
        .description("What an agent started from a chat may do without asking. Full access is chosen per thread, in the app.")
        .disabled(off)
        .keywords(["permission", "approval"]),
        SettingItem::new("Start on a worktree", option_switch(|o| o.worktree, |o, v| o.worktree = v))
            .description("Each chat thread gets its own branch and folder.")
            .disabled(off)
            .keywords(["branch", "isolation"]),
        SettingItem::new("Steer running turns", option_switch(|o| o.steer, |o, v| o.steer = v))
            .description("A message sent while the agent works joins the turn instead of waiting for it to end.")
            .disabled(off)
            .keywords(["queue", "steer"]),
        SettingItem::new(
            "Push to paired chats",
            option_choice(
                vec![
                    (BotNotify::Off, "Off".into()),
                    (BotNotify::WhenAway, "When Orbit is in the background".into()),
                    (BotNotify::Always, "Always".into()),
                ],
                |o| o.notify,
                |o, v| o.notify = v,
            ),
        )
        .description("Approvals, questions and failures from any thread, with buttons to answer them.")
        .disabled(off)
        .keywords(["notifications", "needs you", "alerts"]),
    ])
}

// ── Guard ────────────────────────────────────────────────────────────────

fn guard_group() -> SettingGroup {
    SettingGroup::new()
        .title("Guard")
        .description(
            "Matching actions always ask in Orbit, even at Full access, and can’t be approved from a chat. Approving \
             one covers that action only.",
        )
        .items([
            SettingItem::new(
                "Guard risky actions",
                SettingField::switch(
                    |cx| guard(cx).is_none_or(|g| g.enabled),
                    |enabled, cx| edit_guard(|g| g.enabled = enabled, cx),
                ),
            )
            .keywords(["guard", "force push", "rm -rf", "sudo", "deploy", "secrets"]),
            SettingItem::new("Rules", SettingField::render(|_, window, cx| guard_rules(window, cx)))
                .layout(Axis::Vertical)
                .description("A rule matches commands, file paths and tool input. * stands for any text; case is ignored and a match starts at a word.")
                .keywords(["patterns", "guard list"]),
        ])
}

struct NewRuleState {
    pattern: Entity<InputState>,
    label: Entity<InputState>,
    _subscriptions: [Subscription; 2],
}

fn guard_rules(window: &mut Window, cx: &mut App) -> AnyElement {
    let new_rule = window.use_keyed_state("bot-guard-new-rule", cx, |window, cx| {
        let pattern = cx.new(|cx| InputState::new(window, cx).placeholder("make release*"));
        let label = cx.new(|cx| InputState::new(window, cx).placeholder("Reason, e.g. Publishes"));
        let on_enter = |this: &mut NewRuleState, _: &Entity<InputState>, event: &InputEvent, window: &mut Window, cx: &mut Context<NewRuleState>| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                add_rule(&this.pattern.clone(), &this.label.clone(), window, cx);
            }
        };
        let _subscriptions = [cx.subscribe_in(&pattern, window, on_enter), cx.subscribe_in(&label, window, on_enter)];
        NewRuleState { pattern, label, _subscriptions }
    });
    let theme = cx.theme().clone();
    let current = guard(cx).cloned().unwrap_or_default();
    let disabled = !current.enabled;
    let (pattern, label) = {
        let state = new_rule.read(cx);
        (state.pattern.clone(), state.label.clone())
    };
    let can_add = !pattern.read(cx).value().trim().is_empty();
    let rows = current.rules.iter().map(|rule| {
        let remove = rule.pattern.clone();
        h_flex()
            .w_full()
            .gap_3()
            .text_sm()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_family(theme.mono_font_family.clone())
                    .when(disabled, |d| d.text_color(theme.muted_foreground))
                    .child(rule.pattern.trim_end().to_owned()),
            )
            .child(div().w_48().flex_none().truncate().text_color(theme.muted_foreground).child(rule.label.clone()))
            .child(
                Button::new(SharedString::from(format!("bot-guard-remove-{}", rule.pattern)))
                    .ghost()
                    .xsmall()
                    .icon(Lucide::Trash)
                    .tooltip(format!("Remove “{}”", rule.pattern.trim_end()))
                    .on_click(move |_, _, cx| {
                        let remove = remove.clone();
                        edit_guard(move |g| g.rules.retain(|r| r.pattern != remove), cx)
                    }),
            )
    });
    let (add_pattern, add_label) = (pattern.clone(), label.clone());
    let is_default = current.rules == default_rules();
    v_flex()
        .w_full()
        .gap_1()
        .children(rows)
        .when(current.rules.is_empty(), |list| {
            list.child(div().text_sm().text_color(theme.muted_foreground).child("No rules. Nothing is held for you."))
        })
        .child(
            h_flex()
                .w_full()
                .gap_2()
                .pt_2()
                .child(div().flex_1().min_w_0().child(Input::new(&pattern).small()))
                .child(div().w_48().flex_none().child(Input::new(&label).small()))
                .child(
                    Button::new("bot-guard-add")
                        .outline()
                        .small()
                        .label("Add")
                        .disabled(!can_add)
                        .on_click(move |_, window, cx| add_rule(&add_pattern, &add_label, window, cx)),
                ),
        )
        .child(
            h_flex().pt_1().child(
                Button::new("bot-guard-defaults")
                    .ghost()
                    .small()
                    .icon(Lucide::RotateCcw)
                    .label("Restore defaults")
                    .disabled(is_default)
                    .on_click(|_, _, cx| edit_guard(|g| g.rules = default_rules(), cx)),
            ),
        )
        .into_any_element()
}

fn add_rule(pattern_input: &Entity<InputState>, label_input: &Entity<InputState>, window: &mut Window, cx: &mut App) {
    let pattern = pattern_input.read(cx).value().to_string();
    if pattern.trim().is_empty() {
        return;
    }
    let label = label_input.read(cx).value().trim().to_owned();
    pattern_input.update(cx, |i, cx| i.set_value("", window, cx));
    label_input.update(cx, |i, cx| i.set_value("", window, cx));
    edit_guard(
        move |g| {
            let key = pattern.trim().to_lowercase();
            if !g.rules.iter().any(|r| r.pattern.trim_end().to_lowercase() == key) {
                g.rules.push(GuardRule::new(pattern.trim(), label));
            }
        },
        cx,
    );
}
