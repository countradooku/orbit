//! A plugin's settings: the dialog that edits what its manifest declares.
//! Values are checked on the host against their declared type; secrets go
//! to the host's keychain and never come back (the field only says whether
//! one is saved).

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::{DialogAction, DialogClose};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::select::{SearchableVec, Select, SelectState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{ActiveTheme as _, IndexPath, WindowExt as _, h_flex, v_flex};
use gpui_kit::*;
use orbit_proto::*;

use crate::app_state::AppState;

type ChoiceSelect = SelectState<SearchableVec<SharedString>>;

enum Control {
    Text(Entity<InputState>),
    Toggle(bool),
    Choice(Entity<ChoiceSelect>),
}

struct Field {
    setting: PluginSetting,
    control: Control,
}

pub struct SettingsForm {
    fields: Vec<Field>,
}

impl SettingsForm {
    fn new(plugin: &PluginInfo, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let fields = plugin
            .settings
            .iter()
            .map(|s| {
                let control = match s.kind.as_str() {
                    "toggle" => Control::Toggle(s.value == "true"),
                    "choice" => {
                        let items: Vec<SharedString> = s.choices.iter().map(|c| SharedString::from(c.clone())).collect();
                        let ix = s.choices.iter().position(|c| *c == s.value).map(|row| IndexPath::default().row(row));
                        Control::Choice(cx.new(|cx| ChoiceSelect::new(SearchableVec::new(items), ix, window, cx)))
                    }
                    "secret" => {
                        let placeholder = if s.set { "Saved; type to replace" } else { "Not set" };
                        Control::Text(cx.new(|cx| InputState::new(window, cx).placeholder(placeholder).masked(true)))
                    }
                    _ => {
                        let value = s.value.clone();
                        Control::Text(cx.new(|cx| InputState::new(window, cx).default_value(value)))
                    }
                };
                Field { setting: s.clone(), control }
            })
            .collect();
        Self { fields }
    }

    /// The settings that changed: (key, new value).
    fn changes(&self, cx: &App) -> Vec<(String, String)> {
        self.fields
            .iter()
            .filter_map(|f| {
                let value = match &f.control {
                    Control::Text(input) => input.read(cx).value().to_string(),
                    Control::Toggle(on) => on.to_string(),
                    Control::Choice(select) => select.read(cx).selected_value().map(|v| v.to_string()).unwrap_or_default(),
                };
                let changed = if f.setting.kind == "secret" { !value.is_empty() } else { value != f.setting.value };
                changed.then(|| (f.setting.key.clone(), value))
            })
            .collect()
    }
}

impl Render for SettingsForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let rows = self.fields.iter().enumerate().map(|(ix, f)| {
            let control = match &f.control {
                Control::Text(input) => Input::new(input).into_any_element(),
                Control::Choice(select) => Select::new(select).into_any_element(),
                Control::Toggle(on) => Switch::new(SharedString::from(format!("plugin-setting-{}", f.setting.key)))
                    .checked(*on)
                    .on_click(cx.listener(move |this, on: &bool, _, cx| {
                        if let Some(Field { control: Control::Toggle(value), .. }) = this.fields.get_mut(ix) {
                            *value = *on;
                        }
                        cx.notify();
                    }))
                    .into_any_element(),
            };
            let label = div().flex_1().min_w_0().text_sm().font_weight(FontWeight::MEDIUM).child(f.setting.label.clone());
            let description = (!f.setting.description.is_empty())
                .then(|| div().text_xs().text_color(theme.muted_foreground).child(f.setting.description.clone()));
            if matches!(f.control, Control::Toggle(_)) {
                // A switch sits at the end of its label's row.
                v_flex().gap_1().child(h_flex().gap_2().child(label).child(control)).children(description)
            } else {
                v_flex().gap_1().child(label).children(description).child(control)
            }
        });
        v_flex().gap_3().children(rows)
    }
}

/// Opens the settings dialog for `plugin`.
pub fn open(app: &Entity<AppState>, plugin: &PluginInfo, window: &mut Window, cx: &mut App) {
    let form = cx.new(|cx| SettingsForm::new(plugin, window, cx));
    let (app, id, name) = (app.clone(), plugin.id.clone(), plugin.name.clone());
    window.open_dialog(cx, move |dialog, _, _| {
        let (app, id, form) = (app.clone(), id.clone(), form.clone());
        dialog
            .title(format!("{name} settings"))
            .w(px(480.))
            .child(form.clone())
            .footer(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(DialogClose::new().child(Button::new("plugin-settings-cancel").outline().label("Cancel")))
                    .child(DialogAction::new().child(Button::new("plugin-settings-save").primary().label("Save"))),
            )
            .on_ok(move |_, window, cx| {
                for (key, value) in form.read(cx).changes(cx) {
                    let command = PluginCommand::Setting { id: id.clone(), key, value: Some(value) };
                    let task = app.update(cx, |app, cx| app.call(Command::Plugin(Box::new(command)), cx));
                    let handle = window.window_handle();
                    cx.spawn(async move |cx| {
                        if let Err(message) = task.await {
                            let _ = handle.update(cx, |_, window, cx| {
                                window.push_notification(gpui_kit::component::notification::Notification::error(message), cx)
                            });
                        }
                    })
                    .detach();
                }
                true
            })
    });
}
