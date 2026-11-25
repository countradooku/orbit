//! The plugin dock: plugins' panels as tabs in a panel beside the work
//! area. Opened from the palette (or a plugin's command); a panel whose
//! plugin is turned off or removed closes by itself.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::*;

use super::panel::PanelView;
use crate::app_state::AppState;

pub struct PluginDock {
    app: Entity<AppState>,
    tabs: Vec<Entity<PanelView>>,
    active: usize,
}

impl PluginDock {
    pub fn new(app: Entity<AppState>) -> Self {
        Self { app, tabs: Vec::new(), active: 0 }
    }

    pub fn is_open(&self) -> bool {
        !self.tabs.is_empty()
    }

    /// Shows a plugin's panel: its tab if open, else a new one.
    pub fn open(&mut self, plugin: &str, panel: &str, cx: &mut Context<Self>) {
        match self.tabs.iter().position(|t| {
            let t = t.read(cx);
            t.plugin() == plugin && t.panel() == panel
        }) {
            Some(ix) => self.active = ix,
            None => {
                let view = cx.new(|_| PanelView::new(self.app.clone(), plugin.to_owned(), panel.to_owned()));
                self.tabs.push(view);
                self.active = self.tabs.len() - 1;
            }
        }
        cx.notify();
    }

    fn close(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix < self.tabs.len() {
            self.tabs.remove(ix);
            self.active = self.active.min(self.tabs.len().saturating_sub(1));
            cx.notify();
        }
    }

    /// Drops tabs whose plugin or panel is gone; returns whether any were.
    pub fn prune(&mut self, cx: &mut Context<Self>) -> bool {
        let before = self.tabs.len();
        let state = super::state(cx);
        let keep: Vec<bool> = self
            .tabs
            .iter()
            .map(|t| {
                let t = t.read(cx);
                state.plugin(t.plugin()).is_some_and(|p| p.status.is_active() && p.ui.panels.iter().any(|x| x.id == t.panel()))
            })
            .collect();
        let mut keep = keep.into_iter();
        self.tabs.retain(|_| keep.next().unwrap_or(false));
        self.active = self.active.min(self.tabs.len().saturating_sub(1));
        before != self.tabs.len()
    }
}

impl Render for PluginDock {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let state = super::state(cx);
        let titles: Vec<SharedString> = self
            .tabs
            .iter()
            .map(|t| {
                let t = t.read(cx);
                state
                    .plugin(t.plugin())
                    .and_then(|p| p.ui.panels.iter().find(|x| x.id == t.panel()))
                    .map(|p| SharedString::from(p.title.clone()))
                    .unwrap_or_else(|| SharedString::from(t.panel().to_owned()))
            })
            .collect();
        let active = self.active;
        let body = self.tabs.get(active).cloned();
        v_flex()
            .size_full()
            .bg(theme.background)
            .border_l_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .h_8()
                    .flex_none()
                    .pl_1()
                    .pr_1()
                    .gap_1()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        div().flex_1().min_w_0().overflow_x_hidden().child(
                            TabBar::new("plugin-dock-tabs")
                                .xsmall()
                                .underline()
                                .selected_index(active)
                                .children(titles.into_iter().map(|title| Tab::new().label(title)))
                                .on_click(cx.listener(|this, ix: &usize, _, cx| {
                                    this.active = *ix;
                                    cx.notify();
                                })),
                        ),
                    )
                    .child(
                        Button::new("plugin-dock-close")
                            .ghost()
                            .xsmall()
                            .icon(Icon::new(IconName::Close))
                            .tooltip("Close panel")
                            .on_click(cx.listener(move |this, _, _, cx| this.close(active, cx))),
                    ),
            )
            .child(div().flex_1().min_h_0().children(body))
    }
}
