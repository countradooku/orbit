//! A plugin's declarative panel, drawn with GPUI Kit's own components.
//!
//! The tree (`orbit_plugin::panel::Node`) comes from the host as JSON; its
//! text reads the plugin's storage through `{{key}}` placeholders. Inputs
//! belong to the panel; switches and selects write their storage key;
//! buttons and Enter in an input fire an action. Only the action's id and
//! the inputs go to the host, which finds the action in the installed tree
//! again and checks the plugin's capability before doing it.

use std::collections::HashMap;
use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::group_box::{GroupBox, GroupBoxVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::progress::Progress;
use gpui_kit::component::select::{SearchableVec, Select, SelectEvent, SelectState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::tag::Tag;
use gpui_kit::component::text::TextView;
use gpui_kit::component::{ActiveTheme as _, Icon, IndexPath, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_plugin::panel::{ButtonStyle, Gap, Node, TextSize, Tone};
use orbit_plugin::template;
use orbit_proto::*;
use sonic_rs::{JsonContainerTrait as _, JsonValueTrait as _, Value};

use crate::app_state::AppState;
use crate::assets::Lucide;

type ChoiceSelect = SelectState<SearchableVec<SharedString>>;

/// Where a fired action goes: `(action id, inputs)`. The host by default;
/// tests record instead.
pub type Sink = Rc<dyn Fn(String, Vec<(String, String)>, &mut Window, &mut App)>;

pub struct PanelView {
    plugin: String,
    panel: String,
    sink: Sink,
    /// The parsed tree and the JSON it came from.
    tree: Option<(String, Rc<Node>)>,
    /// The parsed storage and the JSON it came from.
    storage: (String, Rc<Value>),
    inputs: HashMap<String, Entity<InputState>>,
    /// Which action Enter in an input fires (from the last render).
    submits: Rc<std::cell::RefCell<HashMap<String, String>>>,
    selects: HashMap<String, Entity<ChoiceSelect>>,
    /// Selected tab per tabs node (by path).
    tabs: HashMap<String, usize>,
    _subscriptions: Vec<Subscription>,
}

impl PanelView {
    pub fn new(app: Entity<AppState>, plugin: String, panel: String) -> Self {
        let (plugin_id, panel_id) = (plugin.clone(), panel.clone());
        let sink: Sink = Rc::new(move |action, inputs, window, cx| {
            let command = PluginCommand::PanelAction { id: plugin_id.clone(), panel: panel_id.clone(), action, inputs };
            let task = app.update(cx, |app, cx| app.call(Command::Plugin(Box::new(command)), cx));
            let handle = window.window_handle();
            cx.spawn(async move |cx| {
                let result = task.await;
                let _ = handle.update(cx, |_, window, cx| match result {
                    Ok(Reply::Plugin(reply)) => {
                        if let PluginReply::Message(message) = *reply
                            && !message.trim().is_empty()
                        {
                            window.push_notification(Notification::info(message), cx);
                        }
                    }
                    Ok(_) => {}
                    Err(message) => window.push_notification(Notification::error(message), cx),
                });
            })
            .detach();
        });
        Self::with_sink(plugin, panel, sink)
    }

    /// A panel whose actions go to `sink` (tests).
    pub fn with_sink(plugin: String, panel: String, sink: Sink) -> Self {
        Self {
            plugin,
            panel,
            sink,
            tree: None,
            storage: (String::new(), Rc::new(Value::new_object())),
            inputs: HashMap::new(),
            submits: Rc::default(),
            selects: HashMap::new(),
            tabs: HashMap::new(),
            _subscriptions: Vec::new(),
        }
    }

    pub fn plugin(&self) -> &str {
        &self.plugin
    }

    pub fn panel(&self) -> &str {
        &self.panel
    }

    /// Reads the current tree and storage from the plugins state.
    fn refresh(&mut self, cx: &App) -> Result<(), String> {
        let state = super::state(cx);
        let plugin = state.plugin(&self.plugin).filter(|p| p.status.is_active()).ok_or("The plugin is off or was removed.")?;
        let panel = plugin.ui.panels.iter().find(|p| p.id == self.panel).ok_or("The plugin no longer has this panel.")?;
        if self.tree.as_ref().is_none_or(|(json, _)| *json != panel.tree) {
            let node = Node::parse(&panel.tree)?;
            self.tree = Some((panel.tree.clone(), Rc::new(node)));
        }
        if self.storage.0 != plugin.ui.storage {
            let value = sonic_rs::from_str::<Value>(&plugin.ui.storage).unwrap_or_else(|_| Value::new_object());
            self.storage = (plugin.ui.storage.clone(), Rc::new(value));
        }
        Ok(())
    }

    /// Sends action `id` with every input's value (and `extra`).
    fn fire(&mut self, id: String, extra: Vec<(String, String)>, window: &mut Window, cx: &mut Context<Self>) {
        let mut inputs: Vec<(String, String)> = self.inputs.iter().map(|(bind, s)| (bind.clone(), s.read(cx).value().to_string())).collect();
        inputs.sort();
        inputs.extend(extra);
        let typed = id.starts_with('a') && inputs.iter().any(|(k, v)| k != "index" && k != "value" && !v.is_empty());
        (self.sink)(id, inputs, window, cx);
        // What was typed went with the action.
        if typed {
            for input in self.inputs.values() {
                input.update(cx, |i, cx| i.set_value("", window, cx));
            }
        }
    }

    fn input(&mut self, bind: &str, placeholder: &str, window: &mut Window, cx: &mut Context<Self>) -> Entity<InputState> {
        if let Some(input) = self.inputs.get(bind) {
            return input.clone();
        }
        let placeholder = SharedString::from(placeholder.to_owned());
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        let (submits, bind_key) = (self.submits.clone(), bind.to_owned());
        let sub = cx.subscribe_in(&input, window, move |this, _, event: &InputEvent, window, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                let action = submits.borrow().get(&bind_key).cloned();
                if let Some(action) = action {
                    this.fire(action, Vec::new(), window, cx);
                }
            }
        });
        self._subscriptions.push(sub);
        self.inputs.insert(bind.to_owned(), input.clone());
        input
    }

    fn select(&mut self, bind: &str, labels: Vec<SharedString>, values: Vec<String>, current: Option<usize>, window: &mut Window, cx: &mut Context<Self>) -> Entity<ChoiceSelect> {
        if let Some(select) = self.selects.get(bind) {
            let select = select.clone();
            let shown = select.read(cx).selected_index(cx).map(|ix| ix.row);
            if shown != current {
                select.update(cx, |s, cx| s.set_selected_index(current.map(|row| IndexPath::default().row(row)), window, cx));
            }
            return select;
        }
        let select = cx.new(|cx| ChoiceSelect::new(SearchableVec::new(labels.clone()), current.map(|row| IndexPath::default().row(row)), window, cx));
        let bind_key = bind.to_owned();
        let sub = cx.subscribe_in(&select, window, move |this, _, event: &SelectEvent<SearchableVec<SharedString>>, window, cx| {
            let SelectEvent::Confirm(Some(label)) = event else { return };
            if let Some(ix) = labels.iter().position(|l| l == label) {
                this.fire(format!("set:{bind_key}"), vec![("value".into(), values[ix].clone())], window, cx);
            }
        });
        self._subscriptions.push(sub);
        self.selects.insert(bind.to_owned(), select.clone());
        select
    }
}

/// What placeholders read: the storage, and in a row, `item` and `index`.
#[derive(Clone)]
struct Scope {
    storage: Rc<Value>,
    row: Option<(usize, Value)>,
}

impl Scope {
    fn text(&self, template_text: &str) -> String {
        template::render(template_text, &|path| self.lookup(path))
    }

    fn lookup(&self, path: &str) -> Option<String> {
        if let Some((index, item)) = &self.row {
            if path == "index" {
                return Some(index.to_string());
            }
            if path == "item" {
                return Some(template::text_of(item));
            }
            if let Some(rest) = path.strip_prefix("item.") {
                return template::lookup(item, rest).map(template::text_of);
            }
        }
        template::lookup(&self.storage, path).map(template::text_of)
    }

    fn array(&self, key: &str) -> Vec<Value> {
        template::lookup(&self.storage, key).and_then(|v| v.as_array()).map(|a| a.iter().cloned().collect()).unwrap_or_default()
    }

    fn row_index(&self) -> Option<usize> {
        self.row.as_ref().map(|(i, _)| *i)
    }

    fn row_input(&self) -> Vec<(String, String)> {
        self.row.as_ref().map(|(i, _)| vec![("index".to_owned(), i.to_string())]).unwrap_or_default()
    }
}

impl Render for PanelView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Err(message) = self.refresh(cx) {
            return crate::ui::empty_state(Icon::new(Lucide::Puzzle), "Panel unavailable", message, None, cx).into_any_element();
        }
        let (_, tree) = self.tree.clone().expect("refreshed");
        let scope = Scope { storage: self.storage.1.clone(), row: None };
        self.submits.borrow_mut().clear();
        let mut counter = 0;
        let body = self.node(&tree, "0", &scope, &mut counter, window, cx);
        v_flex()
            .id(SharedString::from(format!("plugin-panel-{}-{}", self.plugin, self.panel)))
            .size_full()
            .overflow_y_scroll()
            .p_3()
            .gap_3()
            .child(body)
            .into_any_element()
    }
}

impl PanelView {
    fn id(&self, path: &str, row: Option<usize>) -> SharedString {
        match row {
            Some(r) => format!("pp-{}-{}-{path}-r{r}", self.plugin, self.panel).into(),
            None => format!("pp-{}-{}-{path}", self.plugin, self.panel).into(),
        }
    }

    /// Draws one node; `counter` numbers explicit actions in tree order, as
    /// `Node::actions` does on the host. Each kind is drawn by its own
    /// function: GPUI builders are large, and one frame holding all of them
    /// would overflow the stack a few levels down a tree in debug builds.
    fn node(&mut self, node: &Node, path: &str, scope: &Scope, counter: &mut usize, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        match node {
            Node::Stack { gap, children } => self.stack(false, *gap, children, path, scope, counter, window, cx),
            Node::Row { gap, children } => self.stack(true, *gap, children, path, scope, counter, window, cx),
            Node::Text { text, muted, size } => self.text(text, *muted, *size, scope, cx),
            Node::Heading { text } => div().text_base().font_weight(FontWeight::SEMIBOLD).child(scope.text(text)).into_any_element(),
            Node::Spacer {} => div().flex_1().into_any_element(),
            Node::Button { label, style, .. } => self.button(label, *style, path, scope, counter, cx),
            Node::Badge { text, tone } => badge(text, *tone, scope),
            Node::Card { title, description, children } => self.card(title, description, children, path, scope, counter, window, cx),
            Node::List { items, item, empty } => self.list(items, item, empty, path, scope, counter, window, cx),
            Node::Input { bind, placeholder, submit } => {
                if submit.is_some() {
                    self.submits.borrow_mut().insert(bind.clone(), format!("a{}", *counter));
                    *counter += 1;
                }
                let input = self.input(bind, placeholder, window, cx);
                Input::new(&input).small().into_any_element()
            }
            Node::Switch { bind, label } => self.switch(bind, label, path, scope, cx),
            Node::Select { bind, options } => self.choice(bind, options, scope, window, cx),
            Node::Progress { value, max, label } => self.progress(value, *max, label, path, scope, cx),
            Node::Markdown { text } => {
                let id = self.id(path, scope.row_index());
                TextView::markdown(id, scope.text(text)).selectable(true).into_any_element()
            }
            Node::CodeView { text, language } => {
                let id = self.id(path, scope.row_index());
                TextView::markdown(id, format!("```{language}\n{}\n```", scope.text(text))).selectable(true).into_any_element()
            }
            Node::Table { rows, columns, empty } => table(rows, columns, empty, scope, cx),
            Node::Tabs { tabs } => self.tabs(tabs, path, scope, counter, window, cx),
            Node::EmptyState { title, description, button } => self.empty(title, description, button.as_ref(), path, scope, counter, cx),
        }
    }

    #[inline(never)]
    #[allow(clippy::too_many_arguments)]
    fn stack(&mut self, row: bool, gap: Gap, children: &[Node], path: &str, scope: &Scope, counter: &mut usize, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let kids = self.children(children, path, scope, counter, window, cx);
        let el = if row { h_flex().w_full().items_center().flex_wrap() } else { v_flex().w_full() };
        let el = match gap {
            Gap::None => el,
            Gap::Small => el.gap_1(),
            Gap::Medium => el.gap_2(),
            Gap::Large => el.gap_4(),
        };
        el.children(kids).into_any_element()
    }

    #[inline(never)]
    fn text(&self, text: &str, muted: bool, size: TextSize, scope: &Scope, cx: &App) -> AnyElement {
        let muted_color = cx.theme().muted_foreground;
        div()
            .when(size == TextSize::Small, |el| el.text_xs())
            .when(size == TextSize::Normal, |el| el.text_sm())
            .when(muted, |el| el.text_color(muted_color))
            .child(scope.text(text))
            .into_any_element()
    }

    #[inline(never)]
    fn button(&mut self, label: &str, style: ButtonStyle, path: &str, scope: &Scope, counter: &mut usize, cx: &mut Context<Self>) -> AnyElement {
        let id = format!("a{}", *counter);
        *counter += 1;
        let extra = scope.row_input();
        let button = Button::new(self.id(path, scope.row_index())).small().label(scope.text(label));
        let button = match style {
            ButtonStyle::Primary => button.primary(),
            ButtonStyle::Secondary => button.outline(),
            ButtonStyle::Ghost => button.ghost(),
            ButtonStyle::Danger => button.danger(),
        };
        button.on_click(cx.listener(move |this, _, window, cx| this.fire(id.clone(), extra.clone(), window, cx))).into_any_element()
    }

    #[inline(never)]
    #[allow(clippy::too_many_arguments)]
    fn card(&mut self, title: &str, description: &str, children: &[Node], path: &str, scope: &Scope, counter: &mut usize, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let kids = self.children(children, path, scope, counter, window, cx);
        let muted = cx.theme().muted_foreground;
        let content = v_flex()
            .gap_2()
            .when(!description.is_empty(), |el| el.child(div().text_xs().text_color(muted).child(scope.text(description))))
            .children(kids);
        let group = GroupBox::new().id(self.id(path, scope.row_index())).outline();
        let group = if title.is_empty() { group } else { group.title(scope.text(title)) };
        group.child(content).into_any_element()
    }

    #[inline(never)]
    #[allow(clippy::too_many_arguments)]
    fn list(&mut self, items: &str, item: &Node, empty: &str, path: &str, scope: &Scope, counter: &mut usize, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let rows = scope.array(items);
        let per_row = item.actions().len();
        let base = *counter;
        let muted = cx.theme().muted_foreground;
        let mut out: Vec<AnyElement> = Vec::new();
        if rows.is_empty() && !empty.is_empty() {
            out.push(div().text_sm().text_color(muted).child(scope.text(empty)).into_any_element());
        }
        for (i, value) in rows.into_iter().enumerate().take(orbit_plugin::panel::MAX_LIST) {
            let row_scope = Scope { storage: scope.storage.clone(), row: Some((i, value)) };
            let mut row_counter = base;
            let child = self.node(item, &format!("{path}.0"), &row_scope, &mut row_counter, window, cx);
            out.push(div().id(self.id(path, Some(i))).w_full().child(child).into_any_element());
        }
        *counter = base + per_row;
        v_flex().w_full().gap_1().children(out).into_any_element()
    }

    #[inline(never)]
    fn switch(&mut self, bind: &str, label: &str, path: &str, scope: &Scope, cx: &mut Context<Self>) -> AnyElement {
        let on = template::lookup(&scope.storage, bind).and_then(|v| v.as_bool()).unwrap_or(false);
        let key = bind.to_owned();
        Switch::new(self.id(path, scope.row_index()))
            .small()
            .checked(on)
            .label(scope.text(label))
            .on_click(cx.listener(move |this, on: &bool, window, cx| this.fire(format!("set:{key}"), vec![("value".into(), on.to_string())], window, cx)))
            .into_any_element()
    }

    #[inline(never)]
    fn choice(&mut self, bind: &str, options: &[orbit_plugin::panel::Choice], scope: &Scope, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let current = template::lookup(&scope.storage, bind).map(template::text_of);
        let labels: Vec<SharedString> = options.iter().map(|o| SharedString::from(o.label.clone())).collect();
        let values: Vec<String> = options.iter().map(|o| o.value.clone()).collect();
        let ix = current.and_then(|c| values.iter().position(|v| *v == c));
        let select = self.select(bind, labels, values, ix, window, cx);
        Select::new(&select).small().into_any_element()
    }

    #[inline(never)]
    fn progress(&self, value: &str, max: f64, label: &str, path: &str, scope: &Scope, cx: &App) -> AnyElement {
        let n: f64 = scope.text(value).trim().parse().unwrap_or(0.);
        let pct = (n / max * 100.).clamp(0., 100.) as f32;
        let muted = cx.theme().muted_foreground;
        v_flex()
            .w_full()
            .gap_1()
            .when(!label.is_empty(), |el| el.child(div().text_xs().text_color(muted).child(scope.text(label))))
            .child(Progress::new(self.id(path, scope.row_index())).value(pct))
            .into_any_element()
    }

    #[inline(never)]
    fn tabs(&mut self, tabs: &[orbit_plugin::panel::Tab], path: &str, scope: &Scope, counter: &mut usize, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let selected = self.tabs.get(path).copied().unwrap_or(0).min(tabs.len().saturating_sub(1));
        let key = path.to_owned();
        let bar = TabBar::new(self.id(path, scope.row_index()))
            .underline()
            .small()
            .selected_index(selected)
            .children(tabs.iter().map(|t| Tab::new().label(scope.text(&t.title))))
            .on_click(cx.listener(move |this, ix: &usize, _, cx| {
                this.tabs.insert(key.clone(), *ix);
                cx.notify();
            }));
        let mut body = Vec::new();
        for (i, tab) in tabs.iter().enumerate() {
            if i == selected {
                body.extend(self.children(&tab.children, &format!("{path}.{i}"), scope, counter, window, cx));
            } else {
                // Hidden tabs' actions keep their numbers.
                *counter += tab.children.iter().map(|c| c.actions().len()).sum::<usize>();
            }
        }
        v_flex().w_full().gap_2().child(bar).child(v_flex().w_full().gap_2().children(body)).into_any_element()
    }

    #[inline(never)]
    #[allow(clippy::too_many_arguments)]
    fn empty(&mut self, title: &str, description: &str, button: Option<&orbit_plugin::panel::Labelled>, path: &str, scope: &Scope, counter: &mut usize, cx: &mut Context<Self>) -> AnyElement {
        let action = button.map(|b| {
            let id = format!("a{}", *counter);
            *counter += 1;
            let extra = scope.row_input();
            Button::new(self.id(path, scope.row_index()))
                .small()
                .outline()
                .label(scope.text(&b.label))
                .on_click(cx.listener(move |this, _, window, cx| this.fire(id.clone(), extra.clone(), window, cx)))
                .into_any_element()
        });
        crate::ui::empty_state(Icon::new(Lucide::Puzzle), scope.text(title), scope.text(description), action, cx).into_any_element()
    }

    fn children(&mut self, children: &[Node], path: &str, scope: &Scope, counter: &mut usize, window: &mut Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        children.iter().enumerate().map(|(i, c)| self.node(c, &format!("{path}.{i}"), scope, counter, window, cx)).collect()
    }
}

#[inline(never)]
fn badge(text: &str, tone: Tone, scope: &Scope) -> AnyElement {
    let tag = match tone {
        Tone::Neutral => Tag::secondary(),
        Tone::Info => Tag::info(),
        Tone::Success => Tag::success(),
        Tone::Warning => Tag::warning(),
        Tone::Danger => Tag::danger(),
    };
    tag.small().child(scope.text(text)).into_any_element()
}

#[inline(never)]
fn table(rows: &str, columns: &[orbit_plugin::panel::Column], empty: &str, scope: &Scope, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let data = scope.array(rows);
    let header = h_flex()
        .w_full()
        .gap_2()
        .pb_1()
        .border_b_1()
        .border_color(theme.border)
        .text_xs()
        .text_color(theme.muted_foreground)
        .children(columns.iter().map(|c| div().flex_1().min_w_0().truncate().child(c.title.clone())));
    let mut lines: Vec<AnyElement> = vec![header.into_any_element()];
    if data.is_empty() && !empty.is_empty() {
        lines.push(div().text_sm().text_color(theme.muted_foreground).child(scope.text(empty)).into_any_element());
    }
    for value in data.iter().take(orbit_plugin::panel::MAX_LIST) {
        let cells = columns.iter().map(|c| {
            let text = template::lookup(value, &c.key).map(template::text_of).unwrap_or_default();
            div().flex_1().min_w_0().truncate().child(text)
        });
        lines.push(h_flex().w_full().gap_2().text_sm().children(cells).into_any_element());
    }
    v_flex().w_full().gap_1().children(lines).into_any_element()
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AppContext as _, Bounds, TestAppContext, WindowBounds, WindowOptions, px, size};
    use orbit_plugin::panel::Node;
    use orbit_proto::{PluginInfo, PluginPanel, PluginStatus, PluginUi, PluginsState};

    use super::{PanelView, Sink};

    /// Actions a panel fired: (id, inputs).
    type Fired = Rc<RefCell<Vec<(String, Vec<(String, String)>)>>>;

    const TREE: &str = r#"
type = "stack"
children = [
  { type = "heading", text = "Todo" },
  { type = "text", text = "{{count}} open", muted = true },
  { type = "list", items = "todos", empty = "Nothing to do", item = { type = "row", children = [
      { type = "text", text = "{{item}}" },
      { type = "button", label = "Done", style = "ghost", action = { call = "storage.remove", args = { key = "todos", index = "{{index}}" } } },
  ] } },
  { type = "input", bind = "draft", placeholder = "Add a todo", submit = { call = "storage.append", args = { key = "todos", value = "{{input.draft}}" } } },
  { type = "switch", bind = "compact", label = "Compact" },
  { type = "select", bind = "sort", options = [ { value = "new", label = "Newest" }, { value = "old", label = "Oldest" } ] },
  { type = "tabs", tabs = [
      { title = "One", children = [ { type = "button", label = "Ping", action = { call = "notifications.show", args = { title = "hi" } } } ] },
      { title = "Two", children = [ { type = "progress", value = "{{count}}", max = 4 } ] },
  ] },
  { type = "table", rows = "people", columns = [ { key = "name", title = "Name" } ] },
  { type = "card", title = "More", children = [ { type = "markdown", text = "**bold**" }, { type = "code_view", text = "fn main() {}", language = "rust" } ] },
  { type = "empty_state", title = "Done", description = "All clear", button = { label = "Reset", action = { call = "storage.clear", args = { key = "todos" } } } },
]
"#;

    fn state(storage: &str) -> PluginsState {
        let tree = Node::parse(TREE).unwrap().to_json();
        PluginsState {
            plugins: vec![PluginInfo {
                id: "todo".into(),
                name: "Todo".into(),
                status: PluginStatus::Active,
                ui: PluginUi { panels: vec![PluginPanel { id: "list".into(), title: "Todo".into(), tree }], storage: storage.into(), ..Default::default() },
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[gpui_kit::test]
    fn a_component_tree_renders_and_its_actions_fire_with_host_numbering(cx: &mut TestAppContext) {
        let fired: Fired = Rc::default();
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_global(super::super::Plugins { state: state(r#"{"count":2,"todos":["a","b"],"people":[{"name":"Ada"}]}"#), themes: Default::default() });
        });
        let sink_log = fired.clone();
        let sink: Sink = Rc::new(move |id, inputs, _, _| sink_log.borrow_mut().push((id, inputs)));
        let (handle, view) = cx.update(|cx| {
            let opened = gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::new(Default::default(), size(px(480.), px(900.))))),
                    ..Default::default()
                },
                cx,
                move |_, cx| cx.new(|_| PanelView::with_sink("todo".into(), "list".into(), sink)),
            )
            .unwrap();
            (opened.0, opened.1)
        });
        cx.update_window(handle, |_, window, cx| window.render_frame(cx)).unwrap();
        // Every bound control exists; the input knows its submit action.
        cx.update(|cx| {
            let view = view.read(cx);
            assert!(view.inputs.contains_key("draft"));
            assert!(view.selects.contains_key("sort"));
            // List rows share action a0; the input's submit is a1.
            assert_eq!(view.submits.borrow().get("draft").map(String::as_str), Some("a1"));
        });
        // The same numbering as the host's.
        let host: Vec<String> = Node::parse(TREE).unwrap().actions().iter().map(|a| a.call.clone()).collect();
        assert_eq!(host, ["storage.remove", "storage.append", "notifications.show", "storage.clear"]);
        // Firing from a row sends its index; typed inputs go along.
        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |v, cx| {
                v.inputs["draft"].update(cx, |i, cx| i.set_value("milk", window, cx));
                v.fire("a1".into(), Vec::new(), window, cx);
                v.fire("a0".into(), vec![("index".into(), "1".into())], window, cx);
            });
        })
        .unwrap();
        let fired = fired.borrow();
        assert_eq!(fired[0], ("a1".to_owned(), vec![("draft".to_owned(), "milk".to_owned())]));
        assert_eq!(fired[1].0, "a0");
        assert!(fired[1].1.contains(&("index".to_owned(), "1".to_owned())));
        assert!(fired[1].1.contains(&("draft".to_owned(), String::new())), "the input was cleared after it was sent");
    }

    #[gpui_kit::test]
    fn a_panel_of_a_plugin_that_went_away_says_so(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_global(super::super::Plugins::default());
        });
        let sink: Sink = Rc::new(|_, _, _, _| {});
        let handle = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, move |_, cx| cx.new(|_| PanelView::with_sink("gone".into(), "x".into(), sink)))
                .unwrap()
                .0
        });
        cx.update_window(handle, |_, window, cx| window.render_frame(cx)).unwrap();
    }
}
