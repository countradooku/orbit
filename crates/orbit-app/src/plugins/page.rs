//! Settings › Plugins: what is installed (on, off, waiting for approval,
//! blocked), each plugin's capabilities, log, update and removal, and the
//! catalogs to browse.

use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::setting::{SettingField, SettingGroup, SettingItem, SettingPage};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_proto::*;

use super::consent;
use crate::app_state::AppState;
use crate::assets::Lucide;

/// Where Settings › Plugins is among the pages.
pub const PAGE: usize = 13;

#[derive(Default)]
struct Request(bool);

impl Global for Request {}

/// Opens Settings on this page.
pub fn open(window: &mut Window, cx: &mut App) {
    cx.set_global(Request(true));
    window.dispatch_action(Box::new(crate::actions::OpenSettings), cx);
}

/// Whether Settings should open on this page (once).
pub fn take_request(cx: &mut App) -> bool {
    std::mem::take(&mut cx.default_global::<Request>().0)
}

pub fn page(app: &Entity<AppState>, cx: &App) -> SettingPage {
    let host = app.read(cx).active_entry().label();
    let (list_app, browse_app) = (app.clone(), app.clone());
    let secrets = match super::state(cx).secrets.as_str() {
        "keychain" => "In the system keychain.",
        "file" => "In a file on the host only you can read: it has no keychain.",
        _ => "In the system keychain when the host has one, else in a file only you can read.",
    };
    SettingPage::new("Plugins").icon(Icon::new(Lucide::Puzzle)).resettable(false).groups([
        SettingGroup::new()
            .description(format!(
                "Plugins add themes, agents, panels and guardrails. They run on {host}, and do only what you approve; \
                 every call they make is logged."
            ))
            .item(
                SettingItem::new("Installed", SettingField::render(move |_, window, cx| installed(&list_app, window, cx)))
                    .layout(Axis::Vertical)
                    .keywords(["extensions", "add-ons", "themes", "agents", "wasm", "consent", "capabilities"]),
            ),
        SettingGroup::new()
            .title("Browse")
            .description("Catalogs list plugins whose files are pinned by a content hash and signed by the catalog’s publisher.")
            .item(
                SettingItem::new("Catalogs", SettingField::render(move |_, window, cx| browse(&browse_app, window, cx)))
                    .layout(Axis::Vertical)
                    .keywords(["store", "marketplace", "gallery", "discover"]),
            ),
        SettingGroup::new().title("Secrets").item(
            SettingItem::new("Where plugin secrets are kept", SettingField::render(move |_, _, cx| {
                div().text_sm().text_color(cx.theme().muted_foreground).child(secrets).into_any_element()
            }))
            .keywords(["keyring", "keychain", "credentials", "tokens"]),
        ),
    ])
}

fn tier_icon(tier: PluginTier) -> Icon {
    match tier {
        PluginTier::Data => Icon::new(IconName::Palette),
        PluginTier::Agent => Icon::new(IconName::Bot),
        PluginTier::Panel => Icon::new(IconName::PanelRight),
        PluginTier::Wasm => Icon::new(Lucide::ShieldCheck),
    }
}

/// The newest version a catalog offers of `id`, if newer than `version`.
fn update_for<'a>(state: &'a PluginsState, id: &str, version: &str) -> Option<(&'a str, &'a CatalogEntry)> {
    let installed = semver::Version::parse(version).ok()?;
    state
        .catalogs
        .iter()
        .flat_map(|c| c.entries.iter().map(move |e| (c.name.as_str(), e)))
        .filter(|(_, e)| e.id == id)
        .filter_map(|(c, e)| Some((semver::Version::parse(&e.version).ok()?, c, e)))
        .filter(|(v, _, _)| *v > installed)
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, c, e)| (c, e))
}

fn installed(app: &Entity<AppState>, _: &mut Window, cx: &mut App) -> AnyElement {
    let theme = cx.theme().clone();
    let state = super::state(cx).clone();
    let local = app.read(cx).active_entry().is_local();
    let rows = state.plugins.iter().map(|p| {
        let id = SharedString::from(p.id.clone());
        let (status, tone) = match &p.status {
            PluginStatus::Active => ("On".to_owned(), theme.muted_foreground),
            PluginStatus::Disabled => ("Off".to_owned(), theme.muted_foreground),
            PluginStatus::NeedsConsent => ("Waiting for your approval".to_owned(), theme.warning),
            PluginStatus::Killed { reason } => (format!("Blocked by Orbit: {reason}"), theme.danger),
            PluginStatus::Broken { error } => (format!("Doesn’t load: {}", error.lines().next().unwrap_or_default()), theme.danger),
            PluginStatus::Incompatible { requires } => (format!("Needs Orbit {requires}"), theme.warning),
        };
        let can_toggle = !matches!(p.status, PluginStatus::Killed { .. } | PluginStatus::Broken { .. } | PluginStatus::Incompatible { .. });
        let on = !matches!(p.status, PluginStatus::Disabled);
        let caps = if p.capabilities.is_empty() {
            "No capabilities".to_owned()
        } else {
            p.capabilities.iter().map(|c| c.key()).collect::<Vec<_>>().join(", ")
        };
        let update = update_for(&state, &p.id, &p.version).map(|(c, e)| (c.to_owned(), e.version.clone()));
        let has_update = update.is_some();
        let (toggle_app, review_app, menu_app) = (app.clone(), app.clone(), app.clone());
        let (toggle_id, plugin) = (p.id.clone(), p.clone());
        let review_plugin = p.clone();
        let menu = Button::new(SharedString::from(format!("plugin-more-{id}"))).ghost().small().icon(IconName::Ellipsis).tooltip("More").dropdown_menu(
            move |menu, _, _| {
                let (log_app, log_plugin) = (menu_app.clone(), plugin.clone());
                let mut menu = menu.item(PopupMenuItem::new("Log…").icon(Lucide::ScrollText).on_click(move |_, window, cx| {
                    consent::show_logs(&log_app, &log_plugin, window, cx);
                }));
                if !plugin.settings.is_empty() {
                    let (app, settings_plugin) = (menu_app.clone(), plugin.clone());
                    menu = menu.item(PopupMenuItem::new("Settings…").icon(IconName::Settings2).on_click(move |_, window, cx| {
                        super::settings::open(&app, &settings_plugin, window, cx);
                    }));
                }
                if plugin.dev {
                    let (app, id) = (menu_app.clone(), plugin.id.clone());
                    menu = menu.item(PopupMenuItem::new("Reload").icon(Lucide::RefreshCw).on_click(move |_, window, cx| {
                        consent::run(&app, PluginCommand::Reload { id: Some(id.clone()) }, window, cx);
                    }));
                }
                if let Some((catalog, version)) = &update {
                    let (app, catalog, id) = (menu_app.clone(), catalog.clone(), plugin.id.clone());
                    menu = menu.item(PopupMenuItem::new(format!("Update to {version}…")).icon(Lucide::CircleArrowUp).on_click(move |_, window, cx| {
                        consent::preview(&app, InstallSource::Catalog { catalog: catalog.clone(), id: id.clone() }, window, cx);
                    }));
                }
                let (app, remove_plugin) = (menu_app.clone(), plugin.clone());
                menu.separator().item(PopupMenuItem::new("Remove…").icon(Lucide::Trash).on_click(move |_, window, cx| {
                    remove(&app, &remove_plugin, window, cx);
                }))
            },
        );
        h_flex()
            .w_full()
            .gap_3()
            .py_1p5()
            .items_start()
            .child(tier_icon(p.tier).small().mt_0p5().text_color(theme.muted_foreground))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .gap_2()
                            .text_sm()
                            .child(div().min_w_0().truncate().font_weight(FontWeight::MEDIUM).child(p.name.clone()))
                            .child(div().flex_none().text_xs().text_color(theme.muted_foreground).child(p.version.clone()))
                            .when(p.dev, |el| el.child(Tag::secondary().xsmall().child("Dev")))
                            .when(has_update, |el| el.child(Tag::info().xsmall().child("Update"))),
                    )
                    .child(div().text_xs().truncate().text_color(tone).child(status))
                    .child(div().text_xs().truncate().text_color(theme.muted_foreground).child(format!("{} · {caps}", p.summary))),
            )
            .when(p.status == PluginStatus::NeedsConsent, |el| {
                el.child(Button::new(SharedString::from(format!("plugin-review-{id}"))).outline().small().label("Review…").on_click(
                    move |_, window, cx| consent::review(&review_app, &review_plugin, window, cx),
                ))
            })
            .child(
                Switch::new(SharedString::from(format!("plugin-on-{id}")))
                    .small()
                    .checked(on)
                    .disabled(!can_toggle)
                    .accessibility_label(format!("Turn {} on or off", p.name))
                    .on_click(move |on: &bool, window, cx| {
                        let id = toggle_id.clone();
                        let command = if *on { PluginCommand::Enable { id } } else { PluginCommand::Disable { id } };
                        consent::run(&toggle_app, command, window, cx);
                    }),
            )
            .child(menu)
    });
    let install_app = app.clone();
    let install = Button::new("plugins-install").outline().small().icon(IconName::Plus).label("Install…").dropdown_caret(true).dropdown_menu(
        move |menu, _, _| {
            let (a, b, c, d) = (install_app.clone(), install_app.clone(), install_app.clone(), install_app.clone());
            let menu = menu
                .item(PopupMenuItem::new("From a folder…").icon(IconName::Folder).on_click(move |_, window, cx| consent::install_folder(&a, window, cx)))
                .item(PopupMenuItem::new("From an archive…").icon(Lucide::Package).on_click(move |_, window, cx| consent::install_archive(&b, window, cx)))
                .item(PopupMenuItem::new("From git…").icon(Lucide::GitBranch).on_click(move |_, window, cx| consent::install_git(&c, window, cx)));
            if local {
                menu.separator().item(PopupMenuItem::new("Load a folder in dev mode…").icon(Lucide::RefreshCw).on_click(move |_, window, cx| {
                    consent::load_dev_folder(&d, window, cx)
                }))
            } else {
                menu
            }
        },
    );
    v_flex()
        .w_full()
        .gap_1()
        .when(state.plugins.is_empty(), |el| {
            el.child(div().py_2().text_sm().text_color(theme.muted_foreground).child("No plugins yet. Install one, or browse a catalog below."))
        })
        .children(rows)
        .child(h_flex().pt_2().child(install))
        .into_any_element()
}

fn remove(app: &Entity<AppState>, plugin: &PluginInfo, window: &mut Window, cx: &mut App) {
    let (app, id, name, dev) = (app.clone(), plugin.id.clone(), plugin.name.clone(), plugin.dev);
    window.open_alert_dialog(cx, move |alert, _, _| {
        let (app, id) = (app.clone(), id.clone());
        alert
            .title(format!("Remove “{name}”?"))
            .description(if dev {
                "Deletes its storage, settings, secrets and log. Its folder is left alone."
            } else {
                "Deletes its files, storage, settings, secrets and log."
            })
            .confirm()
            .ok_text("Remove")
            .ok_variant(ButtonVariant::Danger)
            .on_ok(move |_, window, cx| {
                consent::run(&app, PluginCommand::Remove { id: id.clone() }, window, cx);
                true
            })
    });
}

fn browse(app: &Entity<AppState>, _: &mut Window, cx: &mut App) -> AnyElement {
    let theme = cx.theme().clone();
    let state = super::state(cx).clone();
    let catalogs = state.catalogs.iter().map(|catalog| {
        let name = SharedString::from(catalog.name.clone());
        let remove_app = app.clone();
        let remove_name = catalog.name.clone();
        let cards = catalog.entries.iter().map(|entry| {
            let installed = state.plugin(&entry.id);
            let newer = installed.is_some_and(|p| {
                matches!((semver::Version::parse(&entry.version), semver::Version::parse(&p.version)), (Ok(a), Ok(b)) if a > b)
            });
            let (label, enabled) = match installed {
                Some(_) if newer => ("Update…", true),
                Some(_) => ("Installed", false),
                None => ("Install…", true),
            };
            let (install_app, source) = (app.clone(), InstallSource::Catalog { catalog: catalog.name.clone(), id: entry.id.clone() });
            let caps = match entry.capabilities.len() {
                0 => "No capabilities".to_owned(),
                1 => "1 capability".to_owned(),
                n => format!("{n} capabilities"),
            };
            v_flex()
                .flex_1()
                .min_w(rems(14.))
                .p_3()
                .gap_1()
                .rounded(theme.radius_lg)
                .border_1()
                .border_color(theme.border)
                .child(
                    h_flex()
                        .gap_2()
                        .child(tier_icon(entry.tier).small().text_color(theme.muted_foreground))
                        .child(div().flex_1().min_w_0().truncate().text_sm().font_weight(FontWeight::MEDIUM).child(entry.name.clone()))
                        .child(div().text_xs().text_color(theme.muted_foreground).child(entry.version.clone())),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(if entry.publisher.is_empty() { entry.tier.label().to_owned() } else { format!("{} · {}", entry.publisher, entry.tier.label()) }),
                )
                .child(div().text_sm().line_clamp(2).child(entry.description.clone()))
                .child(
                    h_flex()
                        .pt_1()
                        .gap_2()
                        .child(div().flex_1().text_xs().text_color(theme.muted_foreground).child(caps))
                        .child(
                            Button::new(SharedString::from(format!("catalog-install-{}-{}", catalog.name, entry.id)))
                                .small()
                                .outline()
                                .label(label)
                                .disabled(!enabled)
                                .on_click(move |_, window, cx| consent::preview(&install_app, source.clone(), window, cx)),
                        ),
                )
        });
        v_flex()
            .w_full()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(name.clone()))
                            .child(div().text_xs().truncate().text_color(theme.muted_foreground).child(catalog.location.clone())),
                    )
                    .child(
                        Button::new(SharedString::from(format!("catalog-remove-{name}")))
                            .ghost()
                            .small()
                            .icon(Icon::new(Lucide::Trash))
                            .tooltip("Remove catalog")
                            .on_click(move |_, window, cx| consent::run(&remove_app, PluginCommand::RemoveCatalog { name: remove_name.clone() }, window, cx)),
                    ),
            )
            .when_some(catalog.error.clone(), |el, error| el.child(div().text_xs().text_color(theme.danger).child(error)))
            .when(catalog.entries.is_empty() && catalog.error.is_none(), |el| {
                el.child(div().text_xs().text_color(theme.muted_foreground).child("Not read yet, or it lists nothing."))
            })
            .child(h_flex().w_full().gap_2().flex_wrap().children(cards))
    });
    let (add_app, refresh_app) = (app.clone(), app.clone());
    v_flex()
        .w_full()
        .gap_4()
        .when(state.catalogs.is_empty(), |el| {
            el.child(div().text_sm().text_color(theme.muted_foreground).child("No catalogs. Add one to browse the plugins it lists."))
        })
        .children(catalogs)
        .child(
            h_flex()
                .gap_2()
                .child(Button::new("catalog-add").outline().small().icon(IconName::Plus).label("Add catalog…").on_click(move |_, window, cx| {
                    consent::add_catalog(&add_app, window, cx)
                }))
                .when(!state.catalogs.is_empty(), |el| {
                    el.child(Button::new("catalog-refresh").ghost().small().icon(Icon::new(Lucide::RefreshCw)).label("Refresh").on_click(
                        move |_, window, cx| consent::run(&refresh_app, PluginCommand::RefreshCatalogs, window, cx),
                    ))
                }),
        )
        .into_any_element()
}
