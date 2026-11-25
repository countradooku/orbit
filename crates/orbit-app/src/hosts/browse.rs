//! Picking a folder on the shown host, for a project on another computer:
//! the system's file dialog only sees this one, so the host's engine lists
//! its folders (`HostCommand::ListDir`). Paths are the host's own (Linux
//! paths for an SSH host or a WSL distro).

use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_proto::*;

use crate::app_state::AppState;
use crate::assets::Lucide;

/// Folders shown at once; a folder with more says so.
const MAX_SHOWN: usize = 400;

type OnPick = Rc<dyn Fn(String, &mut Window, &mut App)>;

pub struct BrowseView {
    app: Entity<AppState>,
    path: Entity<InputState>,
    listing: Option<DirListing>,
    loading: bool,
    error: Option<String>,
    on_pick: OnPick,
    _task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

/// Opens the folder picker; `on_pick` gets the chosen folder's path.
pub fn open(app: Entity<AppState>, on_pick: impl Fn(String, &mut Window, &mut App) + 'static, window: &mut Window, cx: &mut App) {
    let host = app.read(cx).active_entry().label();
    let on_pick: OnPick = Rc::new(on_pick);
    let view = cx.new(|cx| {
        let path = cx.new(|cx| InputState::new(window, cx).placeholder("A folder’s full path"));
        let subscriptions = vec![cx.subscribe_in(&path, window, |this: &mut BrowseView, input, event: &InputEvent, window, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                let path = input.read(cx).value().to_string();
                this.go(path, window, cx);
            }
        })];
        let mut view =
            BrowseView { app, path, listing: None, loading: false, error: None, on_pick, _task: None, _subscriptions: subscriptions };
        view.go(String::new(), window, cx);
        view
    });
    let title = format!("Add a project on {host}");
    window.open_dialog(cx, move |dialog, _, _| dialog.title(title.clone()).w(px(560.)).child(view.clone()));
}

impl BrowseView {
    /// Lists `path` on the host (`""`: the home folder).
    fn go(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        self.loading = true;
        self.error = None;
        cx.notify();
        let reply = self.app.update(cx, |app, cx| app.call(Command::Host(HostCommand::ListDir { path }), cx));
        self._task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = reply.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.loading = false;
                match result {
                    Ok(Reply::Host(HostReply::Dir(listing))) => {
                        this.path.update(cx, |input, cx| input.set_value(listing.path.clone(), window, cx));
                        this.listing = Some(listing);
                    }
                    Ok(_) => this.error = Some("This host can’t list folders; update its orbitd.".into()),
                    Err(message) => this.error = Some(message),
                }
                cx.notify();
            });
        }));
    }

    fn pick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(listing) = &self.listing else { return };
        let path = listing.path.clone();
        let on_pick = self.on_pick.clone();
        window.close_dialog(cx);
        on_pick(path, window, cx);
    }
}

impl Render for BrowseView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let listing = self.listing.clone();
        let parent = listing.as_ref().and_then(|l| l.parent.clone());
        let home = listing.as_ref().map(|l| l.home.clone()).unwrap_or_default();
        let folders: Vec<String> =
            listing.iter().flat_map(|l| l.entries.iter().filter(|e| e.is_dir).map(|e| e.name.clone())).collect();
        let files = listing.as_ref().map_or(0, |l| l.entries.iter().filter(|e| !e.is_dir).count());
        let more = folders.len().saturating_sub(MAX_SHOWN);
        let name = listing
            .as_ref()
            .map(|l| l.path.trim_end_matches(['/', '\\']).rsplit(['/', '\\']).next().unwrap_or("").to_owned())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "this folder".into());
        let separator = if listing.as_ref().is_some_and(|l| l.os == "windows") { "\\" } else { "/" };
        let current = listing.as_ref().map(|l| l.path.clone()).unwrap_or_default();
        v_flex()
            .gap_3()
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new("browse-up")
                            .ghost()
                            .small()
                            .icon(Icon::new(Lucide::FolderUp))
                            .tooltip("Parent folder")
                            .disabled(parent.is_none() || self.loading)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                if let Some(parent) = parent.clone() {
                                    this.go(parent, window, cx);
                                }
                            })),
                    )
                    .child(
                        Button::new("browse-home")
                            .ghost()
                            .small()
                            .icon(Icon::new(Lucide::House))
                            .tooltip("Home folder")
                            .disabled(self.loading)
                            .on_click(cx.listener(move |this, _, window, cx| this.go(home.clone(), window, cx))),
                    )
                    .child(div().flex_1().child(Input::new(&self.path).small())),
            )
            .child(
                v_flex()
                    .id("browse-folders")
                    .h(px(300.))
                    .overflow_y_scroll()
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(theme.border)
                    .p_1()
                    .when(self.loading && listing.is_none(), |el| {
                        el.child(h_flex().p_2().gap_2().text_sm().text_color(theme.muted_foreground).child(Spinner::new().xsmall()).child("Reading…"))
                    })
                    .when_some(self.error.clone(), |el, error| el.child(div().p_2().text_sm().text_color(theme.danger).child(error)))
                    .when(listing.is_some() && folders.is_empty() && self.error.is_none(), |el| {
                        el.child(div().p_2().text_sm().text_color(theme.muted_foreground).child("No folders here."))
                    })
                    .children(folders.into_iter().take(MAX_SHOWN).map(|folder| {
                        let target = format!("{}{separator}{folder}", current.trim_end_matches(['/', '\\']));
                        h_flex()
                            .id(SharedString::from(format!("browse-{folder}")))
                            .h_7()
                            .px_2()
                            .gap_2()
                            .rounded(theme.radius)
                            .text_sm()
                            .cursor_default()
                            .hover(|s| s.bg(theme.muted))
                            .on_click(cx.listener(move |this, _, window, cx| this.go(target.clone(), window, cx)))
                            .child(Icon::new(IconName::Folder).small().text_color(theme.muted_foreground))
                            .child(div().min_w_0().truncate().child(folder))
                    }))
                    .when(more > 0, |el| {
                        el.child(div().p_2().text_xs().text_color(theme.muted_foreground).child(format!("{more} more folders; type a path to go on.")))
                    }),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(match (listing.as_ref().is_some_and(|l| l.repo), files) {
                                (true, _) => "A git repository.".to_owned(),
                                (false, 0) => String::new(),
                                (false, 1) => "1 file here.".to_owned(),
                                (false, n) => format!("{n} files here."),
                            }),
                    )
                    .child(Button::new("browse-cancel").outline().label("Cancel").on_click(|_, window, cx| window.close_dialog(cx)))
                    .child(
                        Button::new("browse-pick")
                            .primary()
                            .label(format!("Add “{name}”"))
                            .disabled(listing.is_none() || self.loading)
                            .on_click(cx.listener(|this, _, window, cx| this.pick(window, cx))),
                    ),
            )
    }
}
