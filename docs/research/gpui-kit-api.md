# GPUI Kit 0.11 API cheat sheet (source-verified, gpui-fast, Windows)

Verified 2026-11-25 against the crates.io sources in `~/.cargo/registry/src/index.crates.io-*/`:
`gpui-kit-0.11.0`, `gpui-component-0.11.0`, `gpui-base-0.11.0`, `gpui-kit-assets-0.11.0`, `gpui-fast-0.1.19` (+ `gpui-fast-macros`, `gpui-fast-scheduler`, `gpui-fast-platform`). `gpui-pre-0.3.14` is the default (non-fast) core and is NOT used when the `gpui-fast` feature is on.

- **Engine selection:** `gpui-kit/src/lib.rs` does `#[cfg(feature = "gpui-fast")] pub use ::gpui_fast::*;`. Every GPUI signature below therefore comes from `gpui-fast-0.1.19/src`. The imports are the same either way: `use gpui_kit::*;`.
- **Repo clone:** `nsh-research/gpui-kit` is at main c11ce78, which says 0.11.0. Ignoring CRLF, it differs from the published crate in only 6 component files, so stories and examples are safe to copy:
  - `input/mod.rs` and `input/input.rs`: main adds `LineDecoration*`, which is **not in 0.11**.
  - `menu/context_menu.rs`: on main an empty menu no longer steals focus.
  - `sidebar/menu.rs`: a11y ids.
  - `form/field.rs`: `min_w_0`.
  - `highlighter/input_adapter.rs`.
- **Skill errata:** the skill quick reference lists `.medium()`, which does **not** exist (see `Sizable`).

Layout: §1 is GPUI core. §2A covers shell, input and overlays. §2B covers feedback, text, chat and diff. §2C covers navigation, data, controls, icons, theme and traits.

Imports used throughout:
```rust
use gpui_kit::*;                              // GPUI (Fast). Includes AppContext, Styled, InteractiveElement, ...
use gpui_kit::prelude::FluentBuilder as _;    // .when/.when_some/.map: NOT in the root glob
use gpui_kit::component::{self as ui, h_flex, v_flex, ActiveTheme as _, StyledExt as _, Sizable as _,
    Disableable as _, Selectable as _, WindowExt as _, Icon, IconName, Theme, ThemeMode};
use gpui_kit::base;                           // gpui-base: motion, Root, ElementExt, reduce_motion, ...
```
## 1. GPUI core (gpui-fast 0.1.19 via `gpui_kit::*`)

### 1.1 Application and windows
```rust
// gpui_kit (desktop) re-exports platform::application:  pub fn application() -> gpui::Application
Application::with_assets(self, impl AssetSource) -> Self          // REQUIRED for icons: gpui_kit::assets::Assets
Application::with_http_client(self, Arc<dyn HttpClient>) -> Self
Application::run<F: 'static + FnOnce(&mut App)>(self, F)
Application::on_open_urls(&self, F) -> &Self;  on_reopen(&self, F) -> &Self
gpui_kit::init(cx: &mut App)                                       // = gpui_component::init (theme, Root plugin, keybindings, base, reduce-motion)

// Kit helper (wraps your view in base::Root, which component overlays need):
pub fn gpui_kit::open_window<V: Render>(options: WindowOptions, cx: &mut App,
    build: impl FnOnce(&mut Window, &mut App) -> Entity<V>) -> Result<(AnyWindowHandle, Entity<V>)>
// Raw GPUI (you must return cx.new(|cx| Root::new(view, window, cx)) yourself):
App::open_window<V: 'static + Render>(&mut self, options: WindowOptions,
    build_root_view: impl FnOnce(&mut Window, &mut App) -> Entity<V>) -> anyhow::Result<WindowHandle<V>>
```
`WindowOptions` (all pub fields; `Default` exists):
`window_bounds: Option<WindowBounds>`, `titlebar: Option<TitlebarOptions>`, `focus: bool`, `show: bool`, `kind: WindowKind {Normal, PopUp, AnchoredPopup(popup::PopupOptions), Floating, Dialog, LayerShell(..) /*linux+wayland*/}`, `is_movable`, `app_owns_titlebar_drag: bool` (macOS only), `inactive_frame_interval / low_power_frame_interval / thermal_frame_interval: Option<Duration>` (**gpui-fast only**: the frame-rate caps default to 30/30/60 fps), `is_resizable`, `is_minimizable`, `display_id: Option<DisplayId>`, `window_background: WindowBackgroundAppearance`, `app_id: Option<String>`, `window_min_size: Option<Size<Pixels>>`, `window_decorations: Option<WindowDecorations {Server, Client}>` (Linux), `icon: Option<Arc<image::RgbaImage>>` (X11 only), `tabbing_identifier`, `use_display_p3`.
- `TitlebarOptions { title: Option<SharedString>, appears_transparent: bool /* hides the system titlebar on Win/mac */, traffic_light_position: Option<Point<Pixels>> }`
- `WindowBounds::{Windowed(Bounds<Pixels>), Maximized(Bounds), Fullscreen(Bounds)}`; `Bounds::centered(display_id: Option<DisplayId>, size: Size<Pixels>, cx: &App)`
- `WindowBackgroundAppearance::{Opaque (default), Transparent, Blurred, MicaBackdrop, MicaAltBackdrop}`. Mica is for Windows 11, and the theme bg must be translucent for it to show. At runtime: `window.set_background_appearance(..)`.
- Custom title bar: `component::TitleBar::window_options()`, which is `{ titlebar: Some(TitleBar::title_bar_options() /* appears_transparent: true */), app_owns_titlebar_drag: true, ..Default }`. See §2A.
```rust
// examples/window_title + hello_world (0.11)
gpui_kit::application().with_assets(gpui_kit::assets::Assets).run(|cx| {
    gpui_kit::init(cx);
    let opts = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(1200.), px(800.)), cx))),
        window_min_size: Some(size(px(720.), px(480.))),
        window_background: WindowBackgroundAppearance::MicaBackdrop,
        ..TitleBar::window_options()
    };
    gpui_kit::open_window(opts, cx, |window, cx| cx.new(|cx| AppView::new(window, cx))).expect("window");
});
```
Window controls (gpui-fast `window.rs`): `window.set_window_title(&mut self, &str)`, `minimize_window()`, `zoom_window()` (maximize toggle), `toggle_fullscreen()`, `is_maximized()`, `window_bounds() -> WindowBounds`, `bounds()`, `viewport_size()`, `start_window_move()`, `show_window_menu(pos)`, `activate_window()`, `remove_window()` (close), `is_window_active()`, `appearance() -> WindowAppearance`, `rem_size()`, `line_height()`, `on_window_should_close(&self, cx: &App, f: impl Fn(&mut Window, &mut App) -> bool + 'static)`, `prompt(level: PromptLevel, message: &str, detail: Option<&str>, answers: &[T], cx) -> oneshot::Receiver<usize>`.
App: `cx.quit()`, `cx.on_window_closed(FnMut(&mut App, WindowId)) -> Subscription`, `cx.windows()`, `cx.active_window()`, `cx.activate(ignoring_other_apps: bool)`, `cx.set_menus(..)`, `cx.on_app_quit(..)`.

### 1.2 Views, elements, entities
```rust
pub trait Render: 'static + Sized { fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement; }
pub trait RenderOnce: 'static { fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement; }
#[derive(IntoElement)] struct Chip { .. }  impl RenderOnce for Chip { .. }   // derive from gpui_kit::IntoElement
pub trait EventEmitter<E: Any>: 'static {}      // impl EventEmitter<MyEvent> for MyView {}
pub trait Focusable: 'static { fn focus_handle(&self, cx: &App) -> FocusHandle; }
```
- `cx.new(|cx: &mut Context<T>| T) -> Entity<T>` comes from the `AppContext` trait (in the glob) and works on App, Context, AsyncApp and Window contexts.
- `Entity<T>`: `.read(cx) -> &T`, `.read_with(cx, |t, app| ..) -> R`, `.update(cx, |t, cx: &mut Context<T>| ..) -> R`, `.update_in(cx /*VisualContext*/, |t, window, cx| ..) -> C::Result<R>`, `.downgrade() -> WeakEntity<T>`, `.entity_id()`, `.write(cx, value)` (replace and notify).
- `WeakEntity<T>`: `.upgrade() -> Option<Entity<T>>`, **`.update(cx, f) -> anyhow::Result<R>`**, `.update_in(cx, |t, window, cx| ..) -> Result<R>`, `.read_with(cx, f) -> Result<R>`, `WeakEntity::new_invalid()`.
- `Context<T>` (derefs to `App`):
  - `entity()`, `weak_entity()`, `entity_id()`
  - `notify()`
  - `emit(event)` (needs `T: EventEmitter<Evt>`)
  - `listener(|this: &mut T, ev: &E, window, cx| ..) -> impl Fn(&E, &mut Window, &mut App)`
  - `processor(|this, e: E, window, cx| -> R)` (for `uniform_list`)
  - `focus_handle()` (App)
  - `focus_self(window)`
  - `focus_view(&Entity<W: Focusable>, window)`
  - `on_next_frame(window, |this, window, cx| ..)`
  - `defer_in(window, |this, window, cx| ..)`
  - `on_release(..)`, `on_focus / on_blur / on_focus_in / on_focus_out / on_focus_lost(..)`
  - `observe_window_bounds / observe_window_activation / observe_window_appearance(window, |this, window, cx| ..)`
  - `observe_global::<G>(..)`, `observe_keystrokes(..)`
- Subscriptions return `Subscription`. Store it (`_subs: Vec<Subscription>`) or call `.detach()`.
```rust
cx.subscribe(&entity, |this: &mut T, emitter: Entity<T2>, ev: &Evt, cx: &mut Context<T>| ..) -> Subscription
cx.subscribe_in(&entity, window, |this, emitter: &Entity<E>, ev: &Evt, window, cx| ..) -> Subscription
cx.observe(&entity, |this, observed: Entity<W>, cx| ..) -> Subscription          // fires on observed.notify()
cx.observe_in(&entity, window, |this, observed: Entity<V2>, window, cx| ..) -> Subscription
cx.subscribe_self(|this, ev: &Evt, cx| ..); cx.observe_self(..)
// App-level: App::subscribe(&entity, |Entity<T>, &Event, &mut App|), App::observe(&entity, |Entity<W>, &mut App|)
```
State inside render: `window.use_keyed_state(key: impl Into<ElementId>, cx, |window, cx: &mut Context<S>| S) -> Entity<S>`, plus `window.use_state(cx, init)` (keyed by caller location).
`ElementId` variants: `View, Integer(u64), Name, Uuid, FocusHandle, NamedInteger(SharedString,u64), Path, CodeLocation, NamedChild(Arc<ElementId>, SharedString), OpaqueId`. `From` is implemented for `&'static str`, `String`, `SharedString`, `usize/u64/i32`, `(&'static str, usize|u64|u32|EntityId)`, `(SharedString, usize)`, `Uuid`. Also `ElementId::named_usize(name, ix)`.

### 1.3 Async (AsyncFnOnce closures: `async move |..| {}`)
```rust
// Context<T>
pub fn spawn<AsyncFn, R>(&self, f: AsyncFn) -> Task<R>
    where AsyncFn: AsyncFnOnce(WeakEntity<T>, &mut AsyncApp) -> R + 'static, R: 'static
pub fn spawn_in<AsyncFn, R>(&self, window: &Window, f: AsyncFn) -> Task<R>
    where AsyncFn: AsyncFnOnce(WeakEntity<T>, &mut AsyncWindowContext) -> R + 'static
// App:    spawn(AsyncFnOnce(&mut AsyncApp) -> R);   Window: spawn(&self, cx: &App, AsyncFnOnce(&mut AsyncWindowContext) -> R)
// AppContext trait (App/Context/AsyncApp/Window...):
fn background_spawn<R: Send + 'static>(&self, future: impl Future<Output = R> + Send + 'static) -> Task<R>
cx.background_executor().timer(Duration) -> Task<()>        // await it; BackgroundExecutor::now() -> Instant
cx.background_executor().spawn(fut) / cx.foreground_executor().spawn(fut)
App::spawn_with_priority(Priority, f); ForegroundExecutor::spawn_when_idle(..)
```
- `Task<T>`: dropping it cancels the task. Use `.detach()`, `Task::ready(v)`, `.is_ready()`, `.fallible()`. On `Task<Result<T,E>>`, the `TaskExt` trait gives `.detach_and_log_err(cx: &App)`.
- `AsyncApp` (owned, `'static`, not Send):
  - **`cx.update(|app: &mut App| ..) -> R`** returns R, not `Result` (a change from older GPUI). It panics if the app has been released.
  - `Entity::update(&mut async_cx, ..) -> R` also returns R directly (`AppContext` has no `Result` assoc type).
  - `cx.background_executor()`, `cx.subscribe(..)`, `cx.open_window(..)`.
- `AsyncWindowContext`: `cx.update(|window, cx| ..) -> Result<R>` (Err if the window closed).
- `WeakEntity::update(cx, ..)` from async always returns `anyhow::Result<R>`. In a loop, use the `Err` to stop.
```rust
// data_table_story.rs / shell_story.rs (0.11)
self._poll = cx.spawn(async move |this, cx| {
    loop {
        cx.background_executor().timer(Duration::from_millis(33)).await;
        if this.update(cx, |this, cx| { this.tick(); cx.notify(); }).is_err() { break; }
    }
});
self._load = cx.spawn(async move |this, cx| {
    let rows = cx.background_spawn(async move { load_rows(path) }).await;   // off the UI thread
    _ = this.update(cx, |this, cx| { this.rows = rows; cx.notify(); });
});
// with a window (examples/markdown):
cx.spawn_in(window, async move |this, cx| {
    _ = this.update_in(cx, |this, window, cx| this.input.update(cx, |s, cx| s.set_value("x", window, cx)));
}).detach();
```
`gpui_kit::base::async_util` also exists (helpers). The skill's `references/gpui/async.md` matches these signatures.

### 1.4 Actions, keybindings, focus
```rust
gpui_kit::actions!(app, [Send, NewChat, ToggleSidebar]);   // Kit macro; also actions!([A, B]) with no namespace
// Data-carrying action (gpui-fast derive emits `gpui::...` paths, which `use gpui_kit::*` provides via hidden `gpui` re-export):
#[derive(Clone, PartialEq, gpui_kit::Action)]
#[action(namespace = app, no_json)]          // no_json avoids needing serde Deserialize + schemars JsonSchema
pub struct OpenChat { pub id: u64 }
// (gpui-base uses #[derive(Clone, Action, PartialEq, Eq, Deserialize)] #[action(namespace = ui, no_json)])

KeyBinding::new<A: Action>(keystrokes: &'static str, action: A, context: Option<&str>) -> KeyBinding
cx.bind_keys(impl IntoIterator<Item = KeyBinding>);   cx.clear_key_bindings();
App::on_action<A: Action>(&mut self, impl Fn(&A, &mut App) + 'static) -> &mut Self      // global fallback
cx.dispatch_action(&dyn Action);  window.dispatch_action(Box<dyn Action>, cx)
// element side (InteractiveElement):
.key_context(C: TryInto<KeyContext>)    // "Chat" or "Chat mode=insert"
.track_focus(&FocusHandle)
.on_action(impl Fn(&A, &mut Window, &mut App) + 'static)   // usually cx.listener(Self::handler)
.capture_action(..)  .on_key_down(..)  .tab_index(isize)  .tab_stop(bool)
.focus(|s| s..) .in_focus(|s| s..) .focus_visible(|s| s..)   // focus styles
```
- `FocusHandle`: `cx.focus_handle()`, `.focus(&self, window, cx)`, `.is_focused(window)`, `.contains_focused(window, cx)`, `.within_focused(window, cx)`, `.tab_index(isize)`, `.tab_stop(bool)`, `.downgrade()`, `.dispatch_action(&dyn Action, window, cx)`.
- **`window.focus(&handle, cx)` takes `cx` in this version.** Also `window.blur(cx)`, `window.focused(cx) -> Option<FocusHandle>`, `window.focus_next(cx)` / `focus_prev(cx)`.
- Keystroke syntax: `"ctrl-k"`, `"cmd-shift-p"`, chords `"ctrl-k ctrl-s"`. **`KeyBinding::new` and `Keystroke::parse` reject the `secondary-` alias** (they parse without a host OS and panic/err with `InvalidKeystrokeError`); only `Keystroke::parse_with_os` and keymap files resolve it. Pick `cmd-`/`ctrl-` with `cfg!(target_os = "macos")`.
```rust
actions!(chat, [Send, FocusComposer]);
cx.bind_keys([KeyBinding::new("ctrl-enter", Send, Some("Chat")), KeyBinding::new("ctrl-l", FocusComposer, None)]);
impl Render for ChatView {
  fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    v_flex().key_context("Chat").track_focus(&self.focus_handle)
      .on_action(cx.listener(|this, _: &Send, window, cx| this.send(window, cx)))
  }
}
impl Focusable for ChatView { fn focus_handle(&self, _: &App) -> FocusHandle { self.focus_handle.clone() } }
```
Component key contexts (CONTEXT consts in 0.11): `Root, Input, NumberInput, Select, Combobox, Command, Dialog, Sheet, Popover, PopoverContent, PopupMenu, AppMenuBar, Tree, Diff, ColorPicker, TimeField, Carousel, GridLayout`. Don't rebind their keys globally without a context. Root handles Tab/Shift-Tab cycling.

### 1.5 Animation
**Core (gpui-fast `elements/animation.rs`)**
```rust
Animation::new(duration: Duration) -> Self    // oneshot, linear
  .repeat() .repeat_synced() /* phase-locked to an app clock */ .with_easing(impl Fn(f32)->f32 + 'static) .with_max_fps(f32)
trait AnimationExt (blanket for all IntoElement):
  fn with_animation(self, id: impl Into<ElementId>, animation: Animation, animator: impl Fn(Self, f32) -> Self + 'static) -> AnimationElement<Self>
  fn with_animations(self, id, Vec<Animation>, animator: impl Fn(Self, usize, f32) -> Self + 'static) -> AnimationElement<Self>
  fn with_spring<T: SpringTarget>(self, id, SpringAnimation<T>, animator: impl FnOnce(Self, T::Output) -> Self + 'static) -> SpringAnimationElement<Self>
AnimationElement::map_element(self, f: impl FnOnce(E) -> E) -> AnimationElement<E>
SpringAnimation::new(SpringConfig::new(stiffness, damping, mass)).to(target).from(initial).with_epsilon(f32).playback(SpringPlayback)
// easing fns (gpui_kit::*): linear, quadratic, ease_in_out, ease_out_quint() -> impl Fn, heartbeat, bounce(f), pulsating_between(min, max)
// gpui_kit::base::animation: cubic_bezier(x1,y1,x2,y2) -> impl Fn, ease_out_cubic, ease_in_cubic, ease_in_out_cubic, trait Lerp (f32, Pixels, Point<Pixels>, Hsla)
window.request_animation_frame(&self)   // schedule another frame (custom animation loops)
```
- **Reduced motion is built into `AnimationExt`.** When `cx.reduce_motion()` is true, oneshot animations render their end state and repeating ones their start state, and no frames are scheduled.
- **No general element transform:** gpui-fast has no `.translate()`, `.scale()` or `.rotate()` on `div`. Animate position through `.top() / .left() / .mt()` (with `.relative()`/`.absolute()`), and fade through `.opacity(f32)`.
- `svg()` has real transforms: `.with_transformation(Transformation::rotate(rad) | ::scale(size) | ::translate(pt))`, and also `.with_timed_transformation(TimedTransformation::rotation(period, from))`.
```rust
div().id("toast").relative().child(..)
  .with_animation(("enter", self.generation), Animation::new(Duration::from_millis(180)).with_easing(ease_in_out),
      |el, t| el.opacity(t).top(px(8.) * (1. - t)))
Icon::new(IconName::Loader).with_animation("spin", Animation::new(Duration::from_secs(1)).repeat(),
      |icon, t| icon.transform(Transformation::rotate(percentage(t))))   // verbatim pattern from spinner.rs; Icon::rotate(radians) also exists
```
**gpui-base motion (`gpui_kit::base::motion::*`, also at `gpui_kit::base::*`)**: these are value transitions keyed by id and sampled during render. They request frames themselves and respect reduced motion.
```rust
Transition::new(Duration) .delay(impl Into<SignedDuration>) .ease(impl Fn(f32)->f32) .easing(Easing) .reduced_motion(ReducedMotion::{Finish, Continue})
Easing::{Linear, Ease, EaseIn, EaseOut (default), EaseInOut, CubicBezier{..}, Steps{..}, LinearStops(..), Custom(Rc<dyn Fn>)}; Easing::cubic_bezier(..)->Result
pub fn transition<T: Interpolate + PartialEq + 'static>(id: impl Into<TransitionId>, target: T, policy: Transition, window, cx) -> T
pub fn transition_with_status<T>(..) -> MotionValue<T> { value, status: MotionStatus::{Idle,Delayed,Running,Finished} }
pub fn spring<T: SpringTarget>(id, target: T, policy: Spring, window, cx) -> T::Output     // Spring::new(response: Duration).with_damping(ratio).with_travel(bool)
pub fn animate_keyframes<T: Interpolate>(id, &Keyframes<T>, Timing, window, cx) -> MotionValue<T>
TransitionState<T>::default().sample(target, policy, window, cx) -> MotionValue<T>   // caller-owned: survives virtualized rows scrolling out
Presence::new(id, present: bool).transition(Transition).sample(window, cx) -> PresenceSample { phase: PresencePhase::{Entering,Present,Exiting,Absent}, progress, status }  // .should_render()
MotionTransform { translation, scale, rotation_radians, opacity } (Interpolate); Stagger, Sequence, MotionReveal
EffectTransition::new(dur).ease(f).slide_y(from,to).slide_x(..).fade(from,to).width(..).height(..).apply(element, id) -> AnimationElement  // legacy helper (top/left/opacity/w/h)
// TransitionId: From<&'static str | String | SharedString | usize | i32 | ElementId | (impl Into<ElementId>, impl Into<SharedString>)>
```
```rust
// hover-lift color without per-view state:
let bg = base::transition(("row-bg", item.id), if hovered { theme.list_hover } else { theme.list },
                          Transition::new(Duration::from_millis(120)), window, cx);
```
- **Reduced motion:** `gpui_base::init` (via `gpui_kit::init`) reads the OS setting into `App::reduce_motion()`. On Windows that is `SPI_GETCLIENTAREAANIMATION`, read once at init, not live. It can be overridden with `base::set_reduce_motion_preference(ReduceMotionPreference::{System, Reduce, Allow}, cx)` or `cx.set_reduce_motion(bool)`, and re-read with `base::apply_system_reduce_motion(cx)`.
- **Theme motion tokens:** `cx.theme().motion: MotionTokens { duration_instant/fast/normal/slow, easing_enter/exit/move: Easing, spring_control/spring_move: Spring, distance_short/medium: Rems }`.
- **How components animate:**
  - Dialog uses `base::motion::Presence` with an exit transition, and `DialogEntrance { SlideDown (default), Fade, FadeSlide, None }`.
  - Sheet uses `with_animation("slide", 150ms)` offsetting top/right/bottom/left.
  - Notification uses `with_animation` with `cubic_bezier(0.25,0.1,0.25,1.)`, slide plus opacity and a shadow fade, and has an exit animation.
  - Sidebar collapse uses `EffectTransition` (200ms, `ease_in_out_cubic`). Tooltip uses `EffectTransition`. Popover uses `with_animation`.
  - Spinner, Skeleton and Shimmer use repeat animations, which go static under reduced motion.
  - `TextView::stream_fade(true)` fades in per word.

### 1.6 Lists and scrolling
```rust
// uniform_list: every row has the same height, and only the visible range renders
pub fn uniform_list<R: IntoElement>(id: impl Into<ElementId>, item_count: usize,
    f: impl 'static + Fn(Range<usize>, &mut Window, &mut App) -> Vec<R>) -> UniformList
UniformList: .track_scroll(&UniformListScrollHandle) .with_sizing_behavior(ListSizingBehavior::{Infer, Auto})
  .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::{FitList, Unconstrained}) .with_width_from_item(Option<usize>)
  .with_decoration(impl UniformListDecoration) .y_flipped(bool)
UniformListScrollHandle::new(); .scroll_to_item(ix, ScrollStrategy::{Top, Center, Bottom, Nearest}); .scroll_to_item_strict(..)
  .scroll_to_item_with_offset(ix, strategy, offset); .scroll_to_bottom(); .logical_scroll_top_index(); .is_scrolled_to_end() -> Option<bool>
// use the view: uniform_list("rows", n, cx.processor(|this, range: Range<usize>, window, cx| range.map(|ix| this.row(ix)).collect::<Vec<_>>()))
// (table/state.rs in gpui-component 0.11 does exactly this)

// list: variable height; items are measured lazily
pub fn list(state: ListState, render_item: impl FnMut(usize, &mut Window, &mut App) -> AnyElement + 'static) -> List
List::with_sizing_behavior(ListSizingBehavior)
ListState::new(item_count: usize, alignment: ListAlignment::{Top, Bottom}, overdraw: Pixels) -> Self    // Clone (Rc); no render closure in new()
  .measure_all() .with_uniform_item_height(px)                       // builders
  reset(count); reset_with_uniform_height(count, h); splice(old_range: Range<usize>, count); splice_focusable(..)
  remeasure(); remeasure_items(range)                                // call after an item's content height changes (streaming)
  item_count(); is_scrolled_to_end() -> Option<bool>; logical_scroll_top() -> ListOffset { item_ix, offset_in_item }
  scroll_by(Pixels); scroll_to(ListOffset); scroll_to_end(); scroll_to_reveal_item(ix); bounds_for_item(ix) -> Option<Bounds<Pixels>>
  set_follow_mode(FollowMode::{Normal, Tail}); is_following_tail(); pause_following_tail()    // gpui-fast tail-follow for chat
  set_scroll_handler(FnMut(&ListScrollEvent{visible_range, count, is_scrolled, is_following_tail}, &mut Window, &mut App))
  set_item_measurement_readiness(Fn(usize, &App) -> bool); viewport_bounds(); item_is_above_viewport(ix)/below
```
- For chat, `ListAlignment::Bottom` still exists. gpui-fast adds `FollowMode::Tail`, which re-engages when the user scrolls back to the bottom. `component::message_scroller::MessageScroller` wraps it with Top alignment, Tail follow, 400px overdraw and a "jump to latest" button (§2B). Prefer it for chat.
- Measuring: list items are measured automatically. `bounds_for_item(ix)` gives laid-out bounds, and `base::ElementExt::on_prepaint(|bounds, window, cx| ..)` (re-exported as `component::ElementExt`) reports any element's bounds. `canvas(prepaint, paint)` also works.
- `ScrollHandle` (for `div().id(..).overflow_y_scroll().track_scroll(&h)`): `new()`, `offset()`, `set_offset(Point)`, `max_offset()`, `scroll_to_bottom()`, `scroll_to_item(ix)`, `scroll_to_top_of_item(ix)`, `bounds_for_item(ix)`, `top_item()`, `bottom_item()`, `logical_scroll_top() -> (usize, Pixels)`, `children_count()`. `.anchor_scroll(Option<ScrollAnchor>)`.
- gpui-kit `v_virtual_list(view, id, item_sizes: Rc<Vec<Size<Pixels>>>, |this, range, window, cx| Vec<R>)` needs the sizes up front (§2C). Scrollbars come from `component::scroll::ScrollableElement` (`.overflow_y_scrollbar()`, `.vertical_scrollbar(&handle_or_list_state)`), see §2C.

### 1.7 Styling (`Styled`, `InteractiveElement`, `StatefulInteractiveElement`)
- Units: `px(f32)`, `rems(f32)`, `relative(f32)` (fraction), `percentage(f32)` (Radians for rotate), `size(w,h)`, `point(x,y)`, `bounds(origin,size)`, `hsla(h,s,l,a)`, `rgb(0xRRGGBB)`, `rgba(0xRRGGBBAA)`, `Hsla::opacity(f32)`.
- Layout: `div()`, `h_flex()` / `v_flex()` (gpui_kit::component or `div().h_flex()` via StyledExt), `flex`, `flex_col`, `flex_row`, `flex_1`, `flex_grow`, `flex_shrink_0`, `flex_none`, `flex_wrap`, `gap_N`/`gap_x_N`, `items_{start,center,end,baseline,stretch}`, `justify_{start,center,end,between,around,evenly}`, `self_*`, `content_*`, `grid` + `grid_cols(n)` + `col_span(n)`. Size: `size_full`, `w_full`, `h_full`, `w(len)`, `h(len)`, `min_w`, `max_w`, `min_h`, `max_h`, `size_N`, `w_N` (Tailwind scale: 0, 0p5, 1…96, px, full, auto, fractions like 1_2). Spacing: `p_N`/`px_N`/`py_N`/`pt_N`…, `m_N`/`mx_auto`…, plus `paddings(..)`/`margins(..)` (StyledExt). Position: `relative`, `absolute`, `top`, `right`, `bottom`, `left`, `inset_0`. `aspect_ratio`, `aspect_square`. Display: `block`, `hidden`, `visible`/`invisible`.
- Overflow: `overflow_hidden`, `overflow_x_hidden`, `overflow_y_hidden`. `overflow_scroll` / `overflow_x_scroll` / `overflow_y_scroll` are on `StatefulInteractiveElement`, **so they require `.id(..)` first**. Likewise `.on_click`, `.hover`-tooltip, `.active`, `.on_drag` and `track_scroll` need `.id()`.
- State styles: `.hover(|s| s.bg(..))`, `.active(|s| ..)` (needs id), `.group("name")` + `.group_hover("name", |s| ..)`, `.group_active`, `.focus(|s| ..)`, `.in_focus`, `.focus_visible`, `.drag_over::<T>(|s, &T, window, cx| ..)`, `.group_drag_over::<T>(name, |s| ..)`.
- Visual: `bg(impl Into<Fill>)`, `opacity(f32)`, `rounded_{none,xs,sm,md,lg,xl,2xl,3xl,full}` plus `rounded(len)` and per-corner `rounded_t/b/l/r/tl/tr/bl/br_*`, `border_N` / `border_t_N` / … with `border_color(..)` and `border_dashed()`, `shadow_{none,2xs,xs,sm,md,lg,xl,2xl}` and `shadow(Vec<BoxShadow>)`, `cursor_{default,pointer,text,move,not_allowed,grab,grabbing,col_resize,row_resize,ew_resize,ns_resize,crosshair,…}`.
- Text: `text_color`, `text_{xs,sm,base,lg,xl,2xl,3xl}`, `text_size(impl Into<AbsoluteLength>)`, `font_family(impl Into<SharedString>)`, `font_weight(FontWeight::BOLD|SEMIBOLD|MEDIUM…)`, `italic`, `underline`, `line_through`, `line_height(impl Into<DefiniteLength>)`, `text_left/center/right`, `truncate()` (nowrap + overflow hidden + ellipsis), `text_ellipsis`, `text_ellipsis_start`, `text_ellipsis_middle`, `line_clamp(n)`, `whitespace_nowrap`, `whitespace_normal`, `text_decoration_*`. Theme fonts are `cx.theme().font_family` and `.mono_font_family` (§2C).
- Events: `.on_click(Fn(&ClickEvent, &mut Window, &mut App))` (`ClickEvent::is_middle_click()`, …), `.on_mouse_down(MouseButton, ..)`, `.on_mouse_up`, `.on_mouse_move`, `.on_mouse_down_out`, `.on_scroll_wheel`, `.on_hover(Fn(&bool, ..))`, `.tooltip(Fn(&mut Window, &mut App) -> AnyView)`, `.tooltip_show_delay(Duration)`, `.occlude()`, `.block_mouse_except_scroll()`, `.on_drag(value: T, |&T, offset: Point<Pixels>, window, cx| -> Entity<W: Render>)`, `.on_drop::<T>(Fn(&T, &mut Window, &mut App))`, `.can_drop(Fn(&dyn Any, ..) -> bool)`.
- Elements:
  - `img(source: impl Into<ImageSource>)`. `ImageSource` has `From<&str | String | SharedString | SharedUri | &Path | PathBuf | Arc<Path> | Arc<RenderImage> | Arc<Image> | Fn>`, so URLs, asset paths and files all work. It takes `.object_fit(ObjectFit::..)` (StyledImage) and `.with_fallback(..)` / `.with_loading(..)`.
  - `svg().path("icons/x.svg")` (asset path), `.external_path(..)`, `.data(&[u8])`, `.text_color(..)` (tints), `.with_transformation(..)`.
  - `canvas(prepaint: FnOnce(Bounds<Pixels>, &mut Window, &mut App) -> T, paint: FnOnce(Bounds<Pixels>, T, &mut Window, &mut App))`.
  - `deferred(child).with_priority(usize)`: paints later, above siblings.
  - `anchored().anchor(Anchor).position(Point<Pixels>).offset(Point).position_mode(AnchoredPositionMode).snap_to_window().snap_to_window_with_margin(edges)`. `Anchor::{TopLeft, TopRight, BottomLeft, BottomRight, TopCenter, BottomCenter, LeftCenter, RightCenter}`, and **there is no `Corner` type in gpui-fast**.
  - `container_query(|size: Size<Pixels>, window, cx| E)` (**gpui-fast only**: a responsive layout by container size).
- `FluentBuilder` (`gpui_kit::prelude::FluentBuilder`): `.when(bool, |el| ..)`, `.when_some(opt, |el, v| ..)`, `.when_none(&opt, ..)`, `.when_else(c, a, b)`, `.map(|el| ..)`.

### 1.8 Platform
```rust
cx.write_to_clipboard(ClipboardItem::new_string(text: String));   // ::new_string_with_metadata, ::new_string_with_json_metadata, ::new_image(&Image)
cx.read_from_clipboard() -> Option<ClipboardItem>;  item.text() -> Option<String>;  item.entries() -> &[ClipboardEntry]
cx.open_url(&self, url: &str);  cx.reveal_path(&self, &Path);  cx.open_with_system(&Path)
cx.prompt_for_paths(PathPromptOptions { files: bool, directories: bool, multiple: bool, prompt: Option<SharedString> })
    -> oneshot::Receiver<Result<Option<Vec<PathBuf>>>>
cx.prompt_for_new_path(directory: &Path, suggested_name: Option<&str>) -> oneshot::Receiver<Result<Option<PathBuf>>>
cx.window_appearance() -> WindowAppearance; window.appearance() -> WindowAppearance::{Light, VibrantLight, Dark, VibrantDark}
cx.set_window_appearance(Option<WindowAppearance>)   // force native chrome light/dark; None = follow system
cx.observe_window_appearance(window, |this, window, cx| ..) -> Subscription   // on Context<T>
window.set_window_title(&mut self, title: &str);  cx.quit();  cx.reduce_motion()
```
```rust
// folder picker (adapted from examples/markdown):
let rx = cx.prompt_for_paths(PathPromptOptions { files: false, directories: true, multiple: false, prompt: Some("Open project".into()) });
cx.spawn(async move |this, cx| {
    let Some(dir) = rx.await.ok()?.ok()??.into_iter().next() else { return None };
    this.update(cx, |this, cx| { this.open_dir(dir); cx.notify(); }).ok()
}).detach();
// file drag and drop from Explorer:
div().id("drop").size_full()
  .drag_over::<ExternalPaths>(|s, _, _, cx| s.bg(cx.theme().drop_target))
  .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| { this.attach(paths.paths().to_vec()); cx.notify(); }))
// system dark mode -> theme (component Theme does NOT follow automatically):
Theme::sync_system_appearance(Some(window), cx);
self._appearance = cx.observe_window_appearance(window, |_, window, cx| Theme::sync_system_appearance(Some(window), cx));
```
`ExternalPaths(pub SmallVec<[PathBuf; 2]>)` with `.paths() -> &[PathBuf]`. Input and Textarea have `.on_paste(Fn(&ClipboardItem, &mut Window, &mut App) -> bool)`, and copied files arrive there as `ClipboardEntry::ExternalPaths`.

## 2A. Components: shell, input, overlays (gpui-component 0.11.0 / gpui-base 0.11.0)

Paths are relative to `gpui_kit::component::` unless noted. Sources: `~/.cargo/registry/src/*/gpui-component-0.11.0/src`, `gpui-base-0.11.0/src`. Story snippets are from `crates/story/src/stories/*` (main differs from 0.11 in only 6 component files, none affecting the APIs below).

Traits you must import for builder methods: `button::ButtonVariants` (`.primary()` etc.), `Sizable` (`.small()/.xsmall()/.large()/.with_size()`), `Disableable` (`.disabled()`), `Selectable` (`.selected()`), `StyledExt`, `WindowExt`, `menu::{DropdownMenu, ContextMenuExt}`. All of these except the `menu::` ones are re-exported at the `component::` root.

### Root and `gpui_kit::open_window`

```rust
// gpui-kit/src/lib.rs
pub fn open_window<V: Render>(options: WindowOptions, cx: &mut App,
    build: impl FnOnce(&mut Window, &mut App) -> Entity<V>) -> Result<(AnyWindowHandle, Entity<V>)>
// It wraps your view in `base::Root` for you: cx.new(|cx| base::Root::new(view, window, cx)).
```
- `component::Root` is `gpui_base::Root` (re-export). `Root::new(view: impl Into<AnyView>, window, cx: &mut Context<Root>)`, `Root::read(window, cx) -> &Root`, `Root::update(window, cx, |root, window, cx| ..)`, `root.view() -> &AnyView`, `Root::register_plugin::<V: RootPlugin>(cx, factory)`, `root.plugin::<V>() -> Option<Entity<V>>`.
- **Overlay layers render automatically in 0.11.** `gpui_component::init` registers a `WindowState` RootPlugin that mounts the sheet, dialog and notification layers, tooltips, the native-menu fallback and touch selection above your content. It also sets rem size from `theme.font_size`, the root font, bg and text colors, and wraps the surface in `window_border()`. There is **no `Root::render_dialog_layer` / `render_sheet_layer` / `render_notification_layer` in 0.11**, so don't call them.
- If you use `cx.open_window` directly, you must return `cx.new(|cx| Root::new(view, window, cx))` yourself. If `init` was not called, the window panics with "component window state is missing; call gpui_component::init before gpui_kit::open_window".
- Root handles Tab/Shift-Tab focus cycling even when nothing is focused.

### TitleBar (`component::TitleBar`, `TITLE_BAR_HEIGHT = px(34.)`)
```rust
pub fn new() -> Self
pub fn title_bar_options() -> TitlebarOptions   // { title: None, appears_transparent: true, traffic_light_position: Some(point(px(9.), px(9.))) }
pub fn window_options() -> WindowOptions        // { titlebar: Some(title_bar_options()), app_owns_titlebar_drag: true, ..Default::default() }
pub fn on_close_window(self, f: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Self  // Linux only; no-op elsewhere
// impl Styled + ParentElement + RenderOnce
```
- Windows: TitleBar draws Minimize/Maximize(Restore)/Close on the right. Their clicks go through platform hit-testing ("If user clicked in the bounds, the window event will be triggered"), so there are no click handlers to wire. On non-Linux the left padding is 12px; on macOS it is 80px.
- `window_border()` -> `WindowBorder::new().shadow_size(px).resize_hit_size(px)`, plus `window_corner_radii(window, cx)` and `window_paddings(window)`. Root applies this automatically; it only matters for Linux CSD.
```rust
// examples/dialog_overlay + story lib.rs
let options = WindowOptions {
    window_bounds: Some(WindowBounds::Windowed(bounds)),
    window_min_size: Some(gpui_kit::Size { width: px(480.), height: px(320.) }),
    ..TitleBar::window_options()
};
gpui_kit::open_window(options, cx, |_, cx| cx.new(|_| App))?;
// render:
v_flex().size_full().child(TitleBar::new().child("My App")).child(body)
```

### sidebar (`component::sidebar::*`)
Default width is 255px and collapsed (icon) width is 48px. The collapse animation runs over 200ms using `EffectTransition` with `ease_in_out_cubic`.
```rust
Sidebar::<E: SidebarItem>::new(id: impl Into<ElementId>) -> Self   // no Sidebar::left()/right() in 0.11
  .side(Side)                                  // Side::Left default (start edge; flips in RTL)
  .collapsible(impl Into<SidebarCollapsible>)  // enum SidebarCollapsible { Icon (default), Offcanvas, None }; From<bool>
  .collapsed(bool)                             // app owns the bool
  .header(impl IntoElement) .footer(impl IntoElement)
  .child(E) .children(impl IntoIterator<Item=E>)   // + Styled (.w(px(220.)))
SidebarHeader::new() / SidebarFooter::new()    // ParentElement + Styled + InteractiveElement + Selectable + Collapsible + DropdownMenu (.dropdown_menu(..))
SidebarGroup::<E>::new(label) .suffix(|window, cx| impl IntoElement) .child(E) .children(..)
SidebarMenu::new() .child(impl Into<SidebarMenuItem>) .children(..)
SidebarMenuItem::new(label)
  .icon(impl Into<Icon>) .active(bool) .disable(bool) .collapsed(bool)
  .default_open(bool) .click_to_open(bool) .click_to_toggle(bool)   // submenu behaviour
  .children(impl IntoIterator<Item = impl Into<Self>>)              // nested submenu
  .suffix(|window, cx| impl IntoElement) .label_style(StyleRefinement) .accessibility_label(..)
  .on_click(impl Fn(&ClickEvent, &mut Window, &mut App) + 'static)
  .context_menu(impl Fn(PopupMenu, &mut Window, &mut App) -> PopupMenu + 'static)
SidebarToggleButton::new() .side(Side) .collapsed(bool) .accessibility_label(..)
  .on_click(impl Fn(&ClickEvent, &mut Window, &mut App) + 'static)
trait SidebarItem: Collapsible + Clone   // implemented by SidebarMenu, SidebarGroup<E>, SidebarMenuItem
```
The sidebar has no state entity and emits no events. Collapse is the app's `bool`.
```rust
Sidebar::new("sidebar").collapsible(SidebarCollapsible::Icon).collapsed(self.collapsed)
  .header(SidebarHeader::new().child(Icon::new(IconName::GalleryVerticalEnd)).child("Acme"))
  .child(SidebarGroup::new("Platform").child(SidebarMenu::new().children(items.iter().map(|it|
      SidebarMenuItem::new(it.label()).icon(it.icon()).active(it == &self.active)
          .on_click(cx.listener(move |this, _, _, cx| { /* set active */ cx.notify(); }))))))
  .footer(SidebarFooter::new().child("Jason Lee"))
// elsewhere:
SidebarToggleButton::new().collapsed(self.collapsed)
  .on_click(cx.listener(|this, _, _, cx| { this.collapsed = !this.collapsed; cx.notify(); }))
```

### resizable (re-exported at root and in `component::resizable`)
```rust
pub fn h_resizable(id: impl Into<ElementId>) -> ResizablePanelGroup   // component versions add the themed handle
pub fn v_resizable(id: impl Into<ElementId>) -> ResizablePanelGroup
pub fn resizable_panel() -> ResizablePanel
pub fn resize_handle<T: 'static, E: 'static + Render>(id, axis: Axis) -> ResizeHandle<T, E>  // standalone edge; .on_drag(..) .inside(HandleEdge)
ResizablePanelGroup: .with_state(&Entity<ResizableState>) .axis(Axis) .child(impl Into<ResizablePanel>) .children(..)
  .size(Pixels)  // cross-axis size
  .on_resize(impl Fn(&Entity<ResizableState>, &mut Window, &mut App) + 'static)
ResizablePanel: .size(impl Into<Pixels>) .size_range(impl Into<Range<Pixels>>) .visible(bool) .fixed() // + Styled, ParentElement
   // any `T: Into<AnyElement>` converts into a flexible panel (`.child(div())`)
ResizableState (Default; cx.new(|_| ResizableState::default())):
  sizes(&self) -> &Vec<Pixels>; is_resizing(); container_size() -> Pixels
  resize_panel(&mut self, ix, size: Pixels, window, cx)   // emits Resized
  insert_panel(&mut self, size: Option<Pixels>, ix: Option<usize>, cx); remove_panel(ix, cx); reset_panel(ix, cx); clear()
enum ResizablePanelEvent { Resized }   // EventEmitter on ResizableState
```
- **There is no built-in persistence or autosave in 0.11.** Persist sizes by subscribing to `ResizablePanelEvent::Resized` on your `Entity<ResizableState>` (or using `.on_resize`) and reading `sizes()`. Restore them by passing the saved `.size(px)` to each `resizable_panel()` on first render, or by calling `resize_panel` afterwards.
- Docs warn against `.absolute()` and `.overflow_hidden()` on a panel, because they clip or remove the handle.
```rust
h_resizable("main").with_state(&self.split)
  .child(resizable_panel().size(px(260.)).size_range(px(180.)..px(480.)).child(sidebar))
  .child(content)                                        // flexible
  .child(resizable_panel().size(px(320.)).fixed().child(inspector))
// in new(): cx.subscribe(&split, |this, state, _: &ResizablePanelEvent, cx| this.save(state.read(cx).sizes().clone())).detach();
```

### input (`component::input::*`)
State types are mode-typed aliases, so a method on the wrong mode is a compile error:
`InputState = InputBaseState<InputMode>`, `TextareaState = InputBaseState<TextareaMode>`, `EditorState = InputBaseState<EditorMode>`.
**There is no `InputState::multi_line(true)`, `.auto_grow` or `.rows` on InputState in 0.11. Use `TextareaState` + `Textarea` for multi-line text.**

Shared (all modes) builders and methods:
```rust
InputState::new(window: &mut Window, cx: &mut Context<Self>) -> Self   // same for TextareaState/EditorState
.placeholder(impl Into<SharedString>)  .default_value(impl Into<SharedString>)
.clean_on_escape()  /* no arg */        .submit_on_enter(bool)  .context_menu(bool)  .replaceable(bool)
.mask_pattern(impl Into<MaskPattern>)   // "(999)-999-9999", MaskPattern::Number { separator: Some(','), fraction: Some(3) }
.input_action(TextInputAction) .autocorrect(bool) .autocapitalize(..) .show_whitespaces(bool)
value(&self) -> SharedString; text(&self) -> &Rope; selected_value(); unmask_value()
set_value(&mut self, value: impl Into<InputContent>, window, cx)   // &str/String/SharedString OK
set_placeholder(&mut self, impl Into<SharedString>, window, cx)
replace_all(text, window, cx); insert(text, window, cx); replace(..); clean(window, cx)
set_disabled(bool, cx); set_readonly(bool, cx); set_submit_on_enter(bool, cx); set_clean_on_escape(bool)
focus(&self, window, cx: &mut Context<Self>)   // or state.focus_handle(cx).focus(window, cx)
cursor(); cursor_position() -> Position; set_cursor_position(..); selected_range(); set_selected_range(range, cx); select_all(window, cx)
```
InputState only: `.masked(bool)`, `set_masked`, `.pattern(regex::Regex)`, `.validate(Fn(&str,&mut App)->bool)`, `.step/.min/.max` (NumberInput), `set_loading(bool, window, cx)`.
TextareaState only: `.rows(usize)`, `set_rows(rows, cx)`, `.auto_grow(min_rows, max_rows)`, `set_auto_grow(min, max, cx)`. Multi-line modes also have `.soft_wrap(bool)`, `.searchable(bool)`, `.wrapping_indent(..)` and `.scroll_bounce(bool)`.

Events, from `impl EventEmitter<InputEvent> + EventEmitter<InputEditsEvent>`:
```rust
pub enum InputEvent { Change, PressEnter { secondary: bool, shift: bool }, Focus, Blur }
pub struct InputEditsEvent { .. }  // .edits() -> &[(InputEdit, Rope)]
```
Enter semantics (base/state.rs). The `"enter"` key maps to `Enter{secondary:false,shift:false}`, `"shift-enter"` maps to `Enter{shift:true}`, and `"ctrl-enter"` (`"cmd-enter"` on macOS) maps to `Enter{secondary:true}`. In multi-line mode a newline is inserted when `!secondary && (!submit_on_enter || shift)`. In every case `PressEnter` is emitted. A chat composer therefore needs `TextareaState::new(..).auto_grow(1, 5).submit_on_enter(true)` and should handle `PressEnter { shift: false, .. }`.

Elements:
```rust
Input::new(&Entity<InputState>)   .prefix(impl IntoElement) .suffix(impl IntoElement) .cleanable(bool) .mask_toggle()
  .disabled(bool) .readonly(bool) .appearance(bool) .bordered(bool) .focus_bordered(bool) .h(..) .h_full()
  .tab_index(isize) .aria_label(..) .content_type(InputContentType) .context_menu(..) .on_paste(..) .id(..)  // + Sizable
Textarea::new(&Entity<TextareaState>)  .h(..) .appearance(bool) .bordered(bool) .disabled(bool) .readonly(bool)
  .on_paste(Fn(&ClipboardItem, &mut Window, &mut App) -> bool)  // return true = handled
```
```rust
// textarea_story.rs (chat input)
let chat_input = cx.new(|cx| TextareaState::new(window, cx).auto_grow(1, 5).submit_on_enter(true)
    .placeholder("Type a message, Enter to send, Shift+Enter for newline"));
let _sub = cx.subscribe_in(&chat_input, window, |this: &mut Self, input, event, window, cx| match event {
    InputEvent::PressEnter { shift, .. } if !shift => {
        let text = input.read(cx).value().trim().to_string();
        if !text.is_empty() {
            this.messages.push(text);
            input.update(cx, |state, cx| state.set_value("", window, cx));
            cx.notify();
        }
    }
    _ => {}
});
// render: Textarea::new(&self.chat_input)
// input_story.rs: InputState::new(window, cx).placeholder("Enter text and clear it by pressing ESC").clean_on_escape()
//                 InputState::new(window, cx).masked(true) + Input::new(&s).mask_toggle()
```
Other components in the module: `NumberInput` (with `NumberInputEvent`, `NumberStep`, `StepAction`), `OtpInput`, `Editor`/`EditorState` (`.language(..)`, `.line_number(bool)`, `.folding(bool)`), and `AnyInputState`. `window.focused_input(cx)` and `window.has_focused_input(cx)` live on WindowExt.

### button (`component::button::*`)
```rust
Button::new(id: impl Into<ElementId>) -> Self
  .label(impl Into<SharedString>) .icon(impl Into<ButtonIcon>)   // IconName / Icon / Spinner ...
  .outline()  /* inherent; combine with variants */  .compact() .rounded(impl Into<ButtonRounded>)
  .loading(bool) .loading_icon(impl Into<Icon>) .dropdown_caret(bool) .toggled(bool)
  .tooltip(impl Into<SharedString>) .tooltip_placement(Placement) .tooltip_show_delay(Duration)
  .tooltip_with_action(tooltip, action: &dyn Action, context: Option<&str>)   // shows keybinding
  .on_click(impl Fn(&ClickEvent, &mut Window, &mut App) + 'static)
  .on_hover(impl Fn(&bool, &mut Window, &mut App) + 'static)
  .track_focus(&FocusHandle) .tab_index(isize) .tab_stop(bool) .aria_expanded(bool) .accessibility_label(..)
  // impl Disableable (.disabled), Selectable (.selected), Sizable, ButtonVariants, Styled, ParentElement, InteractiveElement, FocusableExt
pub enum ButtonVariant { Default, Primary, Secondary, Danger, Info, Success, Warning, Ghost, Link, Text, Custom(ButtonCustomVariant) }
pub trait ButtonVariants { fn with_variant(self, ButtonVariant) -> Self; primary() secondary() danger() warning() success() info() ghost() link() text() custom(ButtonCustomVariant) }
ButtonCustomVariant::new(cx: &App).color(Hsla).foreground(Hsla).hover(Hsla).active(Hsla).shadow(bool)
pub enum ButtonRounded { None, Small, Medium, Large, Size(Pixels) }
```
There is no `Button::new(id).label(..)` shortcut constructor such as `Button::primary(id)`.

`ButtonGroup::new(id) .child(Button) .children(..) .multiple(bool) .layout(Axis) .compact() .outline() .on_click(impl Fn(&Vec<usize>, &mut Window, &mut App))`. It implements Disableable, Sizable and ButtonVariants. Child buttons use `.selected(bool)`.

`Toggle::new(id) .label(..) .icon(impl Into<Icon>) .checked(bool) .tooltip(..) .on_click(impl Fn(&bool, &mut Window, &mut App))`. It implements `ToggleVariants` (`.ghost()` default, `.outline()`; `enum ToggleVariant { Ghost, Outline }`), Sizable and Disableable.
`ToggleGroup::new(id) .child(impl Into<Toggle>) .children(..) .segmented() .single() .on_click(impl Fn(&Vec<bool>, &mut Window, &mut App))`.

`DropdownButton::new(id) .button(Button) .dropdown_menu(impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu) .dropdown_menu_with_anchor(anchor, f) .outline() .menu_accessibility_label(..)` + ButtonVariants/Sizable/Disableable/Selectable.
```rust
Button::new("save").primary().small().icon(IconName::Check).label("Save").loading(self.saving)
    .tooltip_with_action("Save", &Save, None).on_click(cx.listener(|this, _, window, cx| this.save(window, cx)))
```

### popover (`component::popover::{Popover, PopoverState}`)
```rust
Popover::new(id: impl Into<ElementId>) -> Self
  .trigger<T: Selectable + IntoElement + 'static>(trigger: T)   // Button works (it is Selectable)
  .content<F, E>(F) where F: Fn(&mut PopoverState, &mut Window, &mut Context<PopoverState>) -> E   // rebuilt every render
  .child(..) // ParentElement: static content alternative
  .anchor(impl Into<Anchor>)   // gpui Anchor::{TopLeft,TopRight,BottomLeft,BottomRight,TopCenter,BottomCenter,LeftCenter,RightCenter}
  .offset(impl Into<Pixels>) .arrow(bool) .mouse_button(MouseButton)
  .default_open(bool) .open(bool) /* controlled; pair with on_open_change */ .on_open_change(impl Fn(&bool, &mut Window, &mut App))
  .overlay_closable(bool) .appearance(bool) .trigger_style(StyleRefinement) .track_focus(&FocusHandle)  // + Styled
PopoverState: is_open(); dismiss(window, cx); show(window, cx); set_open(bool, cx); toggle_open(window, cx)
```
`Anchor` replaces older GPUI's `Corner`. **There is no `gpui::Corner` in gpui-fast 0.1.19.**
```rust
Popover::new("form").p_0().trigger(Button::new("pop").outline().label("Popup Form"))
  .track_focus(&form.focus_handle(cx)).open(self.open)
  .on_open_change(cx.listener(|this, open: &bool, _, cx| { this.open = *open; cx.notify(); }))
  .child(form)
// dismiss from content: .content(|state, window, cx| Button::new("x").on_click(cx.listener(|s, _, w, cx| s.dismiss(w, cx))))
```

### menu (`component::menu::{PopupMenu, PopupMenuItem, DropdownMenu, ContextMenuExt, ContextMenu, ContextMenuState, AppMenuBar}`)
```rust
PopupMenu::build(window, cx: &mut App, f: impl FnOnce(Self, &mut Window, &mut Context<PopupMenu>) -> Self) -> Entity<Self>
// builder (self -> Self):
.menu(label, action: Box<dyn Action>)  .menu_with_icon(label, icon, action)  .menu_with_check(label, checked: bool, action)
.menu_with_disabled(label, action, disabled) .menu_with_enable(..) .menu_with_icon_and_disabled(..) .menu_with_check_and_disabled(..)
.menu_element(action, |window, cx| E) .menu_element_with_icon(icon, action, f) .menu_element_with_check(checked, action, f)
.link(label, href: impl Into<String>) .link_with_icon(label, icon, href) .link_with_disabled(..)
.label(label) .separator()
.submenu(label, window, cx: &mut Context<Self>, f: impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static)
.submenu_with_icon(Option<Icon>, label, window, cx, f)
.item(impl Into<PopupMenuItem>)   // closure items: PopupMenuItem::new("X").on_click(Fn(&ClickEvent,&mut Window,&mut App))
.action_context(FocusHandle) .min_w(..) .max_w(..) .max_h(..) .scrollable(bool) .check_side(Side) .external_link_icon(bool)
PopupMenuItem::{new(label), element(|window, cx| E), submenu(label, Entity<PopupMenu>), separator(), label(l), link(l, href)}
  .icon(..) .action(Box<dyn Action>) .disabled(bool) .checked(bool) .on_click(..)
trait DropdownMenu: Styled + Selectable + InteractiveElement + IntoElement   // impl'd by Button, SidebarHeader/Footer...
  fn dropdown_menu(self, f: impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static) -> DropdownMenuPopover<Self>
  fn dropdown_menu_with_anchor(self, anchor: impl Into<Anchor>, f) -> DropdownMenuPopover<Self>
trait ContextMenuExt: InteractiveElement + ParentElement + Styled   // any Stateful<Div> (needs .id())
  fn context_menu(self, f: impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static) -> ContextMenu<Self>
```
An action-based item dispatches its action to the focused element, or to `action_context`. To run a closure instead, use `PopupMenuItem::on_click`. On main (not in 0.11.0) an empty context menu is skipped and does not steal focus.
```rust
// menu_story.rs
Button::new("edit").outline().label("Edit").dropdown_menu(move |this, window, cx| {
    this.min_w(250.)
        .item(PopupMenuItem::new("Handle Click").on_click(window.listener_for(&view, |this, _, _, cx| { cx.notify(); })))
        .separator().menu("Copy", Box::new(Copy)).menu_with_check("Wrap", wrap, Box::new(ToggleWrap))
        .menu_with_icon("Search", IconName::Search, Box::new(SearchAll))
        .submenu("Links", window, cx, |menu, _, _| menu.link("GPUI Kit", "https://gpui-kit.com"))
})
div().id("area").child("Right click").context_menu(|menu, _, _| menu.menu("Open", Box::new(Open)).separator())
```

### select (`component::select::*`)
```rust
SelectState::<D: SelectDelegate>::new(delegate: D, selected_index: Option<IndexPath>, window, cx: &mut Context<Self>) -> Self
  .searchable(bool)
  set_selected_index(Option<IndexPath>, window, cx); set_selected_value(&Value, window, cx); set_items(D, window, cx)
  selected_index(&self, cx) -> Option<IndexPath>; selected_value(&self) -> Option<&Value>; focus(window, cx: &mut App)
pub enum SelectEvent<D> { Confirm(Option<<D::Item as SelectItem>::Value>) }  // + EventEmitter<DismissEvent>
Select::new(&Entity<SelectState<D>>) .placeholder(..) .icon(..) .title_prefix(..) .cleanable(bool) .disabled(bool)
  .menu_width(impl Into<Length>) .menu_max_h(..) .search_placeholder(..) .highlight_matches(bool) .empty(|window,cx| E)
  .appearance(bool) .id(..) .accessibility_label(..)   // + Sizable, Styled
// aliases: SelectItem = SearchableListItem, SelectDelegate = SearchableListDelegate, SelectGroup = SearchableGroup, SelectListItem
pub trait SelectItem: Clone { type Value: Clone + PartialEq; fn title(&self) -> SharedString;
   fn display_title(&self) -> Option<AnyElement> {None} fn render(&self, &mut Window, &mut App) -> impl IntoElement {..}
   fn render_highlighted(&self, query: &str, ..) -> impl IntoElement {..}
   fn value(&self) -> &Self::Value; }   // required (see impl for &'static str: type Value = Self)
// impl'd for String, SharedString, &'static str. Delegates: Vec<T>, SearchableVec<T>, SearchableVec<SelectGroup<T>>
SearchableVec::new(impl Into<Vec<T>>) .push(T);  SelectGroup::new(title).item(i).items(iter)
```
```rust
let fruit = cx.new(|cx| SelectState::new(SearchableVec::new(vec!["Apple", "Orange"]), None, window, cx).searchable(true));
cx.subscribe_in(&fruit, window, |this, _, ev: &SelectEvent<SearchableVec<&'static str>>, _, cx| {
    let SelectEvent::Confirm(v) = ev; this.fruit = v.map(|s| s.to_string()); cx.notify(); }).detach();
// render: Select::new(&self.fruit).w(px(280.)).placeholder("Fruit").cleanable(true)
```
`IndexPath::default().row(8).section(2)`.

### command (`component::command::{Command, CommandState, CommandItem, CommandGroup, CommandEntry}`)
```rust
CommandState::new(window, cx: &mut Context<Self>) -> Self
  query(&self, cx) -> SharedString; set_query(..); selected_index() -> Option<IndexPath>; set_selected_index(..)
  matched_count() -> usize; focus(&self, window, cx: &mut App); set_loading(bool, window, cx); is_loading()
Command::new(&Entity<CommandState>)   // RenderOnce; entries are passed on every render
  .item(CommandItem) .items(iter) .group(CommandGroup) .separator()
  .searchable(bool) .filterable(bool) .highlight_matches(bool) .placeholder(..) .max_h(..) .bordered(bool) .scroll_bounce(bool)
  .on_query(Fn(&str, &mut Window, &mut App)) .on_select(Fn(IndexPath, &mut Window, &mut App))
  .on_confirm(Fn(IndexPath, &mut Window, &mut App)) .on_cancel(Fn(&mut Window, &mut App))
  .empty(|window, cx| E) .header(|&CommandState, &mut Window, &mut App| E) .footer(..)   // + Styled
CommandItem::new() .label(..) .icon(..) .action(Box<dyn Action>) /* dispatched on click/confirm, shows keybinding */
  .checked(bool) .disabled(bool) .keywords(iter) .child(|window, cx| E)
CommandGroup::new() .label(..) .item(..) .items(..)        enum CommandEntry { Item(CommandItem), Group(CommandGroup), Separator }
```
**There is no `CommandEvent` and CommandState is not an EventEmitter in 0.11.** Use the `on_*` callbacks instead.
```rust
// command_story.rs: palette in a dialog. Create the state once, outside the builder.
let command = self.palette.clone(); let owner = cx.weak_entity(); let first = Rc::new(Cell::new(true));
window.open_dialog(cx, move |dialog, _, _| {
    let (command, owner, first) = (command.clone(), owner.clone(), first.clone());
    dialog.close_button(false).p_0().content(move |content, window, cx| {
        if first.replace(false) {
            let c = command.clone();
            window.defer(cx, move |window, cx| c.read(cx).focus_handle(cx).focus(window, cx));
        }
        let owner = owner.clone();
        content.child(Command::new(&command).bordered(false).placeholder("Type a command or search...")
            .items(items()).on_confirm(move |ix, window, cx| { _ = owner.update(cx, |v, cx| v.run(ix, window, cx)); }))
    })
});
```

### dialog / AlertDialog / sheet / notification + `WindowExt` (`component::WindowExt`)
```rust
trait WindowExt (impl for Window):
  open_dialog<F: Fn(Dialog, &mut Window, &mut App) -> Dialog + 'static>(&mut self, cx: &mut App, build: F)
  open_alert_dialog<F: Fn(AlertDialog, &mut Window, &mut App) -> AlertDialog + 'static>(&mut self, cx, build)
  has_active_dialog(cx) -> bool; close_dialog(cx); close_all_dialogs(cx)
  open_sheet<F: Fn(Sheet, &mut Window, &mut App) -> Sheet>(cx, build); open_sheet_at(Placement, cx, build); has_active_sheet(cx); close_sheet(cx)
  push_notification(note: impl Into<Notification>, cx); remove_notification::<T>(cx); remove_notification1::<T>(key, cx)
  clear_notifications(cx); notifications(cx) -> Rc<Vec<Entity<Notification>>>; focused_input(cx); has_focused_input(cx)
```
- **The builders are render closures.** They may re-run whenever the layer rebuilds. Create `InputState` and other entities before opening and capture them. Focus is set on the dialog shell, so focus content only once, with a `Cell<bool>` guard.

`Dialog` (`dialog::Dialog`): `new(cx: &mut App)`, `.title(impl IntoElement)`, `.child(..)` (ParentElement), `.content(Fn(DialogContent, &mut Window, &mut App) -> DialogContent)`, `.footer(impl IntoElement)`, `.button_props(DialogButtonProps)`, `.on_ok(Fn(&ClickEvent,&mut Window,&mut App) -> bool)` (return `false` to keep it open), `.on_cancel(.. -> bool)`, `.on_close(Fn(&ClickEvent,..))`, `.close_button(bool)`, `.overlay(bool)`, `.overlay_closable(bool)`, `.keyboard(bool)` (Esc/Enter), `.centered()`, `.margin_top(px)`, `.w()/.width()/.max_w()`, `.entrance(DialogEntrance)`, and `.trigger(impl IntoElement)`. The `.trigger` form renders inline as a declarative dialog: `Dialog::new(cx).trigger(Button..)`.
Parts: `DialogHeader`, `DialogTitle`, `DialogDescription`, `DialogContent`, `DialogFooter`, `DialogClose::new().child(Button)` (closes / runs on_cancel) and `DialogAction::new().child(Button)` (runs on_ok). `DialogButtonProps` has `.ok_text .ok_variant(ButtonVariant) .cancel_text .cancel_variant .show_cancel .on_ok .on_cancel`.
`enum DialogEntrance { SlideDown (default), Fade, FadeSlide, None }`. Dialog motion uses `gpui_base::motion::Presence` with an exit transition; reduced motion, `None`, or a zero theme duration close it in one frame.

`AlertDialog` (**import `dialog::AlertDialog`; it is not at the component root despite its docstring**): `new(cx)`, `.icon(e) .title(e) .description(e) .child(..) .content(f) .footer(e) .confirm()` (shows Cancel + OK), `.ok_text .ok_variant(ButtonVariant) .cancel_text .cancel_variant .show_cancel(bool) .width(px) .close_button(bool) .keyboard(bool) .centered() .entrance(..) .on_ok(..->bool) .on_cancel(..->bool) .on_close(..) .trigger(..)`. **The docstring's `alert.warning()` does not exist in 0.11, and backdrop dismissal is disabled (`overlay_closable` is deprecated and does nothing).**
```rust
window.open_alert_dialog(cx, |alert, _, cx| alert
    .icon(Icon::new(IconName::Info).text_color(cx.theme().danger))
    .title("Delete File").description("This action cannot be undone.")
    .confirm().ok_text("Delete").ok_variant(ButtonVariant::Danger)
    .on_ok(|_, window, cx| { window.push_notification("File deleted", cx); true }));
window.open_dialog(cx, move |dialog, _, _| dialog.title("Import").child(Input::new(&input))
    .footer(h_flex().gap_2()
        .child(DialogClose::new().child(Button::new("cancel").label("Cancel").outline()))
        .child(DialogAction::new().child(Button::new("ok").primary().label("Import"))))
    .on_ok(|_, _, _| true));
```

`Sheet` (`sheet::Sheet`, normally built through `open_sheet`): `new(window, cx)`, `.title(e) .footer(e) .size(impl Into<DefiniteLength>) .resizable(bool) .overlay(bool) .overlay_closable(bool) .on_close(Fn(&ClickEvent,&mut Window,&mut App))`, and it implements ParentElement and Styled. It emits `DismissEvent`. The `Placement` enum (`gpui_base`) is `{ Top, Bottom, Left, Right }`. It slides in over 150ms with `with_animation("slide", ..)`.
```rust
window.open_sheet_at(Placement::Left, cx, move |sheet, _, _| sheet.size(px(400.)).title("Details").child(list.clone()));
```

`Notification` (`notification::{Notification, NotificationType, NotificationDelivery, NotificationSettings}`):
```rust
Notification::new() | info(msg) | success(msg) | warning(msg) | error(msg)
  .message(..) .title(..) .icon(..) .with_type(NotificationType::{Info,Success,Warning,Error})
  .id::<T>() .id1::<T>(key)   // dedupe / targeted removal
  .placement(Anchor) .autohide(bool) .delivery(NotificationDelivery::{InApp, System, InAppAndSystem}) .system() .in_app_and_system()
  .on_click(Fn(&ClickEvent,&mut Window,&mut App)) .on_close(Fn(&mut Window,&mut App))
  .action(Fn(&mut Notification, &mut Window, &mut Context<Notification>) -> Button)  // one action button
  .content(Fn(&mut Self, &mut Window, &mut Context<Self>) -> AnyElement)
  dismiss(&mut self, window, cx)
From<&str | String | SharedString | Cow<str> | (NotificationType, T: Into<SharedString>)>
```
Global settings live in `cx.theme().notification: NotificationSettings { placement: Anchor (TopRight), margins, max_items: 10, width: 382px, delivery }`. Change them via `Theme::global_mut(cx).notification...`. The enter/exit animation uses `with_animation` with `cubic_bezier(0.25,0.1,0.25,1.)`, a slide plus fade.
```rust
window.push_notification((NotificationType::Success, "Saved"), cx);
struct RetryNote;
window.push_notification(Notification::error("Request failed").id::<RetryNote>().title("Uh oh!")
    .action(|_, _, cx| Button::new("retry").primary().label("Retry")
        .on_click(cx.listener(|this, _, window, cx| this.dismiss(window, cx)))), cx);
```

### tooltip (`component::tooltip::Tooltip`)
```rust
Tooltip::new(text: impl Into<Text>) -> Self;  Tooltip::element(|window, cx| E) -> Self
  .action(&dyn Action, context: Option<&str>)   // renders the action's keybinding as Kbd
  .key_binding(Option<Kbd>)
  .build(self, window, cx: &mut App) -> AnyView
// GPUI: StatefulInteractiveElement::tooltip(impl Fn(&mut Window, &mut App) -> AnyView + 'static)  (element needs .id())
// Components: Button/Toggle/... .tooltip(impl Into<SharedString>), Button::tooltip_with_action(text, &action, ctx)
```
```rust
div().id("t2").child("Hover me").tooltip(|window, cx| Tooltip::new("Copy").action(&Copy, Some("Editor")).build(window, cx))
Button::new("copy").ghost().icon(IconName::Copy).tooltip("Copy to clipboard")
```
Root manages tooltips (it dismisses them when the trigger is removed or pressed). Enter and slide use `EffectTransition`.

## 2B. Feedback, rich text, chat and diff components (gpui-component 0.11.0)

All paths are relative to `gpui_kit::component::`. Signatures were copied from the `~/.cargo/registry/.../gpui-component-0.11.0/src` sources (the behavior lives in `gpui-base-0.11.0`). Snippets are adapted from `crates/story/src/stories/*` and `examples/stream-markdown`. Outside input/menu/sidebar/form, the repo's main branch matches 0.11 byte for byte (CRLF ignored).

### Small display (all `RenderOnce`; none emit events)

| Item | Import | Constructor and builders |
|---|---|---|
| Kbd | `kbd::Kbd` | `Kbd::new(stroke: Keystroke)`, `From<Keystroke>`; `.appearance(bool)` (default true), `.outline()`; helpers: `Kbd::binding_for_action(&dyn Action, Option<&str> /*context*/, &Window) -> Option<Kbd>`, `Kbd::binding_for_action_in(&dyn Action, &FocusHandle, &Window) -> Option<Kbd>` (resolves against the *previous* frame), `Kbd::global_binding_for_action(&dyn Action, &Window) -> Option<Kbd>`, `Kbd::format(&Keystroke) -> String`. `Styled`. |
| Badge | `badge::Badge` | `Badge::new()`; `.dot()`, `.count(usize)`, `.max(usize)`, `.icon(impl Into<Icon>)`, `.color(impl Into<Hsla>)`. `ParentElement` (wraps the child it decorates), `Sizable`. |
| Tag | `tag::{Tag, TagVariant}` | `Tag::new()`, `Tag::primary()`, `secondary()`, `danger()`, `success()`, `warning()`, `info()`, `Tag::custom(color: Hsla, foreground: Hsla, border: Hsla)`, `Tag::color(impl Into<ColorName>)`; `.with_variant(TagVariant)`, `.outline()`, `.rounded(impl Into<AbsoluteLength>)`, `.rounded_full()`. `ParentElement + Styled + Sizable`. Tags have no close button or `on_close` in 0.11. `TagVariant { Primary, Secondary(default), Danger, Success, Warning, Info, Color(ColorName), Custom{color,foreground,border} }` |
| Spinner | `spinner::Spinner` | `Spinner::new()` (icon `IconName::Loader`, 0.8 s, `ease_in_out`); `.icon(impl Into<Icon>)`, `.color(Hsla)`, `.ease(impl Fn(f32)->f32 + 'static)`. `Sizable`. Draws a static icon when `cx.reduce_motion()` is set. |
| Skeleton | `skeleton::Skeleton` | `Skeleton::new()`, `.secondary()`. `Styled`, so you size it yourself: `Skeleton::new().w_full().h_4().rounded(cx.theme().radius)` |
| ShimmerText | `shimmer::{ShimmerText, ShimmerStyle, ShimmerSpread}` | `ShimmerText::new(impl Into<SharedString>)`; `.id(impl Into<ElementId>)`, `.with_shimmer_style(ShimmerStyle)`, `.duration(Duration)`, `.highlight_color(impl Into<Hsla>)`, `.spread(impl Into<ShimmerSpread>)`, `.reverse(bool)`, `.once(bool)`. `Styled` (e.g. `.text_color(..)`). `ShimmerStyle::new()` has the same builders. `ShimmerSpread { Relative(f32), Absolute(Pixels) }` with `From<f32>` and `From<Pixels>`. |
| Progress | `progress::{Progress, ProgressCircle}` | `Progress::new(id: impl Into<ElementId>)` / `ProgressCircle::new(id)`; `.value(f32)` (**0..=100**, clamped), `.loading(bool)` (indeterminate; ignores value), `.color(impl Into<Hsla>)`, `.accessibility_label(impl Into<SharedString>)`. `Styled + Sizable`. Both determinate and indeterminate exist. `ProgressCircle` takes children (center label) per the story. |
| Marker | `marker::{Marker, MarkerVariant, MarkerAlignment, MarkerLoadingStyle, MarkerIcon, MarkerContent}` | `Marker::new()`; `.id(..)`, `.role(..)`, `.with_variant(MarkerVariant)`, `.alignment(MarkerAlignment)`, `.loading(bool)`, `.with_loading_style(MarkerLoadingStyle)`, `.with_shimmer_style(ShimmerStyle)`, `.separator_style(StyleRefinement)`, `.icon(MarkerIcon)`, `.content(MarkerContent)`. `MarkerContent::new().text(..)` or `.child(..)`; `MarkerIcon::new().child(..)`. Enums: `MarkerVariant { Plain(default), Separator, Border }`, `MarkerLoadingStyle { Spinner(default), Shimmer }`, `MarkerAlignment { Start, Center, End }`. Use it for transcript dividers ("Earlier messages") and "Thinking…" rows. |
| Alert | `alert::{Alert, AlertVariant}` | `Alert::new(id, message: impl Into<Text>)`, `Alert::info/success/warning/error(id, message)`; `.with_variant(AlertVariant)`, `.icon(..)`, `.title(impl Into<SharedString>)`, `.banner()`, `.visible(bool)`, `.on_close(impl Fn(&ClickEvent,&mut Window,&mut App)+'static)`. `Sizable + Styled`. The message is `text::Text` (`&str`/`String`/`SharedString`/`TextView`), so `markdown("…")` works. `AlertVariant { Default, Info, Success, Warning, Error }` |

```rust
use gpui_kit::component::{kbd::Kbd, badge::Badge, tag::Tag, spinner::Spinner,
    progress::{Progress, ProgressCircle}, shimmer::ShimmerText,
    marker::{Marker, MarkerVariant, MarkerContent}, alert::Alert, Icon, IconName, Sizable as _};
h_flex().gap_2()
  .child(Kbd::new(Keystroke::parse("cmd-shift-p").unwrap()))
  .child(Badge::new().count(3).child(Icon::new(IconName::Bell)))
  .child(Tag::success().small().child("Done"))
  .child(Spinner::new().small().color(cx.theme().blue))
  .child(Progress::new("upload").value(self.pct).loading(self.pct == 0.))
  .child(ShimmerText::new("Thinking…").text_color(cx.theme().muted_foreground))
  .child(Marker::new().with_variant(MarkerVariant::Separator)
      .content(MarkerContent::new().child("Earlier messages")))
  .child(Alert::info("maint", "Starts 22:00 UTC").title("Maintenance"))
```

### text::TextView (Markdown/HTML), `use gpui_kit::component::text::*`

Exports: `TextView, TextViewState, TextViewStyle, TextViewMotion, Text, markdown, html, SelectionFormat, MarkdownExtensions, MarkdownPlugin, FrontmatterPlugin, TaskToggleEvent, RangeHighlight, RenderedText, markdown_ast, MarkdownNode, …`. The component `TextView` is a thin wrapper over `gpui_base::TextView`, which owns parsing, layout and selection.

Constructors:
```rust
TextView::new(state: &Entity<TextViewState>) -> TextView            // explicit, retained state (use for streaming)
TextView::markdown(id: impl Into<ElementId>, text: impl Into<SharedString>) -> TextView
TextView::html(id: impl Into<ElementId>, text: impl Into<SharedString>) -> TextView
text::markdown(source) / text::html(source)   // #[track_caller]: id = ElementId::CodeLocation; do NOT use inside loops/lists
```
Builders: `.style(TextViewStyle)`, `.selectable(bool)` (default true), `.selection_format(SelectionFormat)`, `.scrollable(bool)` (default false; true means virtualized `gpui::list` with a scrollbar, and the parent needs a fixed height), `.max_lines(usize)` (fit-content mode only), `.stream_fade(bool)`, `.motion(TextViewMotion)`, `.inline_code_style(StyleRefinement)`, `.list_style(..)`, `.blockquote_style(..)`, `.image_source(F: Fn(&SharedUri)->ImageSource + Send+Sync)`, `.code_block_actions(F: Fn(&CodeBlock,&mut Window,&mut App)->E + Send+Sync)` (a corner element on every fenced block, e.g. a copy button), `.table_actions(..)`, `.on_link_click(F: Fn(&SharedString,&ClickEvent,&mut Window,&mut App)+Send+Sync)` (replaces the default open-URL), `.link(F)`, `.link_tooltip(F -> AnyView)`, `.on_task_toggle(F: Fn(&TaskToggleEvent,..))`, `.on_reveal(F)`, `.markdown_extensions(MarkdownExtensions)`, `.markdown_mdx()`, `.markdown_block_parser(..)`, `.markdown_block_renderer(..)`, `.plugin(P)`. `Styled`.

`TextViewState` (an entity; it emits no events; callbacks go on the builder):
```rust
cx.new(|cx| TextViewState::markdown(text: &str, cx))   // or ::html
// builders: .selectable(bool) .selection_format(..) .scrollable(bool) .motion(TextViewMotion)
state.set_text(&str, cx)      // if the new text has the old text as a prefix (markdown), it is routed to push_str
state.push_str(&str, cx)      // append
state.set_motion(TextViewMotion) / set_selectable / set_scrollable / set_selection_format
state.selected_text() -> String; selected_source_range(); clear_selection(cx); select_all(cx)
state.set_range_highlights(..), reveal_range(..), clear_range_highlights(cx)
state.rendered_text() -> RenderedText; is_clamped(); list_state() -> &ListState; focus_handle()
```
`TextViewMotion::default()` has no fade. `.with_stream_fade(Duration)`, `.with_stream_fade_stagger(Duration)`, `.with_stream_fade_easing(Easing)`. `.stream_fade(true)` gives a Claude-style per-word fade: 280 ms, 10 ms stagger, turned off by reduced motion.

`TextViewStyle` is a public-field struct with builders: `paragraph_gap: Rems` (0.75rem), `heading_base_font_size`, `heading_font_size: Option<Arc<Fn(u8,Pixels)->Pixels>>`, `highlight_theme: Arc<HighlightTheme>`, code block / inline code / table refinements. Builders: `.paragraph_gap(Rems)`, `.heading_font_size(F)`, `.code_block(StyleRefinement)`, `.inline_code(HighlightStyle)`, `.table(..)`, `.table_head(..)`, `.table_cell(..)`. Fields you leave at their defaults keep the theme-derived values.

Code-block syntax highlighting is a **feature gate**. Without `tree-sitter-<lang>` (or `syntect`) on `gpui-kit`, code blocks render unhighlighted. With it, highlights of unchanged blocks are cached.

**Streaming: how it works and what it costs (verified in `gpui-base/src/text/state.rs`)**
- **Incremental: yes.** `push_str(delta)` (or `set_text(longer)` with the old text as a prefix) sends only the delta to a **background parser task** (`cx.background_spawn(UpdateFuture)`). The parser **pops the last block and re-parses only `source[last_block.start..] + delta`** ("so an unclosed list / fenced code block is not split"). Earlier blocks are kept as they are (`Arc` blocks). The cost is O(last block + delta), not O(whole document).
- Updates are **coalesced**. Several pending appends merge into one parse (`MAX_COALESCED_UPDATES_PER_PARSE`). The result comes back through a channel and is committed on the main thread, which then notifies.
- A full replace (text that does not extend the old text) of **≤ 4 KiB parses synchronously** on the UI thread, so the first layout has the exact height. Larger replaces parse in the background. HTML always parses fully, because HTML blocks have no spans.
- **State retention by id:** `TextView::markdown(id, text)` keeps its `TextViewState` in `window.use_keyed_state(format!("{id}/state"))`. Each frame it calls `set_element_text`, which skips the work when you pass the **same `SharedString` allocation** (a pointer and length check). A new allocation that extends the old text goes through the incremental path, but the `strip_prefix` check is an O(n) byte compare per frame. Keyed state lives only while the element renders, so a virtualized row that scrolls out and back **reparses from scratch**. For streaming, or for rows in a virtual list, hold an `Entity<TextViewState>` in your model and use `TextView::new(&state)` + `push_str`. Use a stable, domain-derived id: never an index or `text::markdown()` in a loop.
- `.scrollable(false)` (the default) lays out the whole document. `.scrollable(true)` virtualizes blocks with `gpui::list`.

Recommended streaming pattern (from `examples/stream-markdown`):
```rust
struct Chat { md: Entity<TextViewState>, scroll: ScrollHandle, _rx_task: Task<()> }
let md = cx.new(|cx| TextViewState::markdown("", cx));
let _rx_task = cx.spawn(async move |this, cx| {
    while let Ok(chunk) = rx.recv().await {
        _ = this.update(cx, |this, cx| {
            this.md.update(cx, |s, cx| s.push_str(&chunk, cx));   // reparses tail block in background
            this.scroll.scroll_to_bottom();
        });
    }
});
// render:
div().id("contents").flex_1().track_scroll(&self.scroll).overflow_y_scroll()
    .child(TextView::new(&self.md).selectable(true).stream_fade(true))
// restart: self.md.update(cx, |s, cx| s.set_text("", cx));
```

### message::* and bubble::* (stateless slot layout, no events)

```rust
use gpui_kit::component::message::{Message, MessageGroup, MessageAlignment, MessageAvatar, MessageHeader, MessageContent, MessageFooter};
use gpui_kit::component::bubble::{Bubble, BubbleContent, BubbleGroup, BubbleVariant, BubbleReactions, BubbleReactionSide};
```
- `Message::new()`: `.alignment(MessageAlignment)`, `.id(..)`, `.role(..)`, `.with_stack_style(StyleRefinement)`, `.avatar(impl IntoElement)`, `.avatar_slot(MessageAvatar)`, `.header(MessageHeader)`, `.content(MessageContent)`, `.footer(MessageFooter)`. `Styled`. It is **not** `ParentElement`: content goes only into the slots.
- `MessageAlignment { Start(default), End }` (RTL-aware). `MessageGroup::new()` is `ParentElement + Styled` and holds consecutive messages from the same sender.
- `MessageAvatar::new()`, `MessageHeader::new().content_inset(bool)`, `MessageFooter::new().content_inset(bool)`: all `ParentElement + Styled`. `MessageContent::new().bubble(Bubble)`; plain `.child(..)` also works.
- `Bubble::new()`: `.alignment(MessageAlignment)`, `.with_variant(BubbleVariant)`, `.content(BubbleContent)`, `.reactions(BubbleReactions)`. `ParentElement` (children go into its content) `+ Styled`.
- `BubbleVariant { Filled(default), Secondary, Muted, Tinted, Outline, Ghost, Destructive }`. `BubbleContent::new()`, `BubbleGroup::new()`, `BubbleReactions::new().side(BubbleReactionSide{Top, Bottom(default)}).alignment(..).action(Button)`.
```rust
Message::new()
  .alignment(MessageAlignment::End)
  .avatar(Avatar::new().name("You").size_8())
  .header(MessageHeader::new().child("You").child("10:25 AM"))
  .content(MessageContent::new().bubble(Bubble::new().child("Sure — notes shortly.")))
  .footer(MessageFooter::new().child("Delivered"))
// assistant reply with markdown:
Message::new().content(MessageContent::new().bubble(
  Bubble::new().with_variant(BubbleVariant::Ghost).child(TextView::new(&msg.md))))
```

### attachment::* (stateless)

`use gpui_kit::component::attachment::{Attachment, AttachmentStatus, AttachmentMedia, AttachmentContent, AttachmentTitle, AttachmentDescription, AttachmentActions, AttachmentGroup};`
- `Attachment::new()`: `.id(..)` (**required** for remove/retry controls), `.on_click(Fn(&ClickEvent,&mut Window,&mut App))`, `.on_remove(same)` (a corner X on hover), `.on_retry(same)` (only while `Failed`), `.progress(percent: f32 /*0..100*/)`, `.tooltip(..)`, `.status(AttachmentStatus)`, `.axis(Axis)`, `.media(AttachmentMedia)`, `.content(AttachmentContent)`, `.actions(AttachmentActions)`. `Sizable + Styled`.
- `AttachmentStatus { Pending, Uploading, Processing, Failed, Complete(default) }` with `is_pending/is_uploading/is_processing/is_failed/is_complete/is_in_progress()`.
- `AttachmentMedia::new().src(impl Into<ImageSource>).overlay(..)`; `AttachmentContent::new().title(AttachmentTitle::new(t).status(..).with_shimmer_style(..)).description(AttachmentDescription::new(t).status(..))`; `AttachmentActions::new()` is a `ParentElement`.
- `AttachmentGroup::new(id).track_scroll(&ScrollHandle).with_edge_fade(color)` is `ParentElement`: a horizontal strip of chips above the composer.

### message_scroller::{MessageScroller, MessageScrollerState}

This is a virtualized, variable-height chat list built on `gpui::list` + `ListState` (`ListAlignment::Top`, 400 px overdraw, `FollowMode::Tail`). **Your app owns the data.** The scroller only tracks the row count, and a row renderer closure is called by index. There is **no delegate trait and no `EventEmitter`**: `MessageScrollerState` emits no events, so poll `is_scrolled_up()` / `is_following_tail()` during render.

```rust
// state
cx.new(|cx| MessageScrollerState::new(item_count: usize, cx))   // tail-follow on
s.item_count(); s.is_scrolled_up(); s.is_following_tail(); s.logical_scroll_top() -> ListOffset
s.append(count, cx) -> bool; s.prepend(count, cx) -> bool (keeps anchor); s.splice(old_range, count, cx) -> bool
s.reset(item_count, cx) (re-enables tail follow); s.remeasure(cx); s.remeasure_items(range, cx) -> bool
s.scroll_to_item(ix, cx) -> bool; s.scroll_to_offset(ListOffset, cx) -> bool; s.scroll_to_end(cx) (resume follow)
// element
MessageScroller::new(id, state: Entity<MessageScrollerState>,
    renderer: impl FnMut(usize, &mut Window, &mut App) -> E + 'static)
  .scrollbar(bool)                     // default true
  .jump_button(bool)                   // default true: built-in "Jump to latest" button, shown when scrolled up
  .with_jump_button_label(..) .with_jump_button_style(StyleRefinement)
  .with_jump_button_renderer(FnOnce(Button)->Button) .with_jump_button_transition(Duration /*200ms*/)
  .with_bottom_fade(color)             // fade when away from the live edge
  .with_content_style(..) .with_list_style(..) .with_row_style(..)    // + Styled
```
Behavior: it follows the tail while the reader sits at the live edge, so streamed tokens land in place. When the reader scrolls up it keeps their position, and the jump button appears (it animates in and out, and respects reduced motion). The jump button calls `scroll_to_end`.

Streaming pattern (from `message_scroller_story.rs`): the renderer is `'static`, so snapshot the data in `render` (`Rc::new(self.messages.clone())`) or capture an `Entity`/`Rc`.
```rust
// new message
self.messages.push(msg); self.scroller.update(cx, |s, cx| _ = s.append(1, cx));
// each token: mutate the row's data, then remeasure ONLY that row
self.scroller.update(cx, |s, cx| _ = s.remeasure_items(ix..ix + 1, cx)); cx.notify();
// render
let rows = Rc::new(self.messages.clone());
MessageScroller::new("chat", self.scroller.clone(), move |ix, _, _| {
    rows.get(ix).map(|m| render_row(m)).unwrap_or_else(|| div().into_any_element())
}).with_list_style(StyleRefinement::default().p_4()).with_bottom_fade(cx.theme().background)
```

### diff::{Diff, DiffState, DiffFile, …}

Exports: `Diff, DiffState, DiffEvent, DiffFile, DiffFileStatus, DiffLinePosition, DiffLineRange, DiffSide, DiffTextPosition, DiffMode, DiffHoverHighlight, DiffHunkSeparator, DiffChangeIndicator, DiffAnnotation, DiffDecoration, DiffGutterMarker, DiffSpanHighlight, DiffConflict, DiffConflictResolution, DiffParseError, DiffParseOptions, DiffInlineUnit`. This is a read-only review surface. All files share **one virtualized list** with a header per file. It owns its scrolling, so **give it a bounded height**.

Feeding data (you supply the patch; it never runs `git`):
```rust
DiffFile::parse(patch: &str) -> Result<Vec<DiffFile>, DiffParseError>      // unified or `git diff`, multi-file
DiffFile::parse_with(patch, DiffParseOptions::new().truncated_hunks(true))
DiffFile::unchanged(path, text: &str) -> DiffFile                          // whole file for reference
DiffFile::parse_conflicts(path, text) -> Result<DiffFile, DiffParseError>  // <<<<<<< / ||||||| / >>>>>>> markers
file.with_language("rust")   // override detection; readers: path(), original_path(), modified_path(), status(),
                             // additions(), deletions(), is_binary(), is_truncated(), has_changes(), conflicts()
```
There is no two-text compare constructor in 0.11 (no `DiffFile::from_texts`). Produce the patch yourself.
`DiffFileStatus { Added, Deleted, Modified, Renamed, Copied, Unchanged, Conflicted }`. `DiffSide { Original, Modified }`.

State (`impl EventEmitter<DiffEvent> for DiffState`):
```rust
cx.new(|cx| DiffState::new(files: impl IntoIterator<Item = DiffFile>, cx)
    .with_mode(DiffMode::Split)                 // DiffMode { Split, Unified(default) }  (non_exhaustive)
    .with_context_lines(Some(3)) .with_expansion_lines(n) .with_min_collapsed_lines(n)
    .with_inline_unit(Some(DiffInlineUnit::Word))   // word-level emphasis: { Word(default), Character }; None = off
    .with_inline_max_line_length(n) .with_syntax_max_line_length(n)
    .with_syntax_highlighter(|lang: &str, text: &str| -> Vec<(Range<usize>, HighlightStyle)>))  // Send+Sync, background
s.set_files(files, cx); s.set_mode(mode, cx); s.mode(); s.set_context_lines(..); s.expand_unchanged(cx); s.collapse_unchanged(cx)
s.set_file_collapsed(path, bool, cx); s.set_all_files_collapsed(bool, cx); s.is_file_collapsed(path)
s.set_selected_lines(Option<DiffLineRange>, cx); s.selected_lines(); s.selected_text(cx)
s.scroll_to_line(DiffLinePosition, cx); s.scroll_to_file(path, cx); s.next_change(cx); s.previous_change(cx); s.has_line(&pos)
s.resolve_conflict(path, ix, Option<DiffConflictResolution{Current,Incoming,Both}>, cx); s.conflict_resolution(path, ix); s.resolved_text(path)
```
`DiffEvent { SelectionStarted(DiffLineRange), SelectionChanged(Option<DiffLineRange>), SelectionEnded(DiffLineRange), FileExpanded(SharedString), FileCollapsed(SharedString), ConflictResolved(SharedString, usize) }`

Element `Diff::new(&Entity<DiffState>)` (`Styled`): `.line_number(bool)`, `.syntax_highlight(bool)`, `.header_visible(bool)`, `.hover_highlight(DiffHoverHighlight{None,Line,LineNumber,Both})`, `.hunk_separator(DiffHunkSeparator{Metadata(default),LineInfo,Simple})`, `.change_indicator(DiffChangeIndicator{Signs(default),Bars,None})`, `.change_background(bool)`, `.soft_wrap(bool)`, `.split_ratio(f32)`.
- **Line comments**: `.annotations(impl IntoIterator<Item = DiffAnnotation>)` + `.render_annotation(Fn(&DiffAnnotation,&mut Window,&mut App)->E)`. Annotations are remeasured whenever visible, so their height can change freely. Build them with `DiffAnnotation::line(id, DiffLinePosition::new(path, DiffSide::Modified, line /*1-based*/))` or `DiffAnnotation::file(id, path)`.
- **"+ comment" affordance**: `.on_add_annotation(Fn(&DiffLineRange,&mut Window,&mut App))` shows an add button beside the hovered line and at the end of the selection.
- Other hooks: `.on_line_click(Fn(&DiffLinePosition,&ClickEvent,..))`, `.on_line_hover(Fn(Option<&DiffLinePosition>,..))`, `.on_text_hover(..)`, `.on_text_click(Fn(&DiffTextPosition,&ClickEvent,..)->bool)`, `.text_cursor(CursorStyle)`.
- Highlights: `.decorations([DiffDecoration::new(DiffLineRange::new(path, side, start, end)).with_background(c)])`, `.span_highlights([DiffSpanHighlight::new(pos, byte_range).with_underline().with_background(c).with_foreground(c)])`, `.gutter_markers([DiffGutterMarker::new(id, pos).with_label(..)])` + `.render_gutter_marker(Fn(&DiffGutterMarker,..)->E)`.
- File header slots: `.render_header(F)` (replaces the whole header), `.render_header_prefix(F)`, `.render_header_title_suffix(F)`, `.render_header_suffix(F)`, each `Fn(&DiffFile,&mut Window,&mut App)->E`. Older names (`header`, `header_prefix`, …, `annotation_content`) are kept as compatibility aliases.
- **Per-hunk actions: there is no hunk-level API in 0.11** (no `on_hunk_*`, no stage/revert). The available hooks are per-file header slots, per-line gutter markers, line clicks and line-range selection (`DiffEvent::SelectionEnded`).
- Keybindings are installed by `init` in context `"Diff"`: arrows, page up/down, home/end, shift-up/down, ctrl/cmd-c, ctrl/cmd-a.
- Syntax highlighting: the built-in highlighter needs the `tree-sitter-<lang>` features (it is a no-op without them). Or supply `with_syntax_highlighter`, which runs on the background executor once per hunk side.

```rust
let state = cx.new(|cx| DiffState::new(DiffFile::parse(&patch)?, cx).with_mode(DiffMode::Unified));
let _sub = cx.subscribe(&state, |this, _, ev: &DiffEvent, cx| if let DiffEvent::SelectionEnded(r) = ev { this.pending = Some(r.clone()); cx.notify(); });
// render
let comments = Rc::new(self.comments.clone()); let me = cx.entity().downgrade();
Diff::new(&self.diff).w_full().h(rems(32.)).hover_highlight(DiffHoverHighlight::Both)
  .annotations(self.comments.iter().map(|c| DiffAnnotation::line(c.id.clone(),
      DiffLinePosition::new(c.range.path().clone(), c.range.end_side(), c.range.end()))))
  .render_annotation(move |a, _, cx| render_comment(&comments, a.id(), cx))
  .on_add_annotation(move |range, _, cx| { _ = me.update(cx, |this, cx| this.add_comment(range, cx)); })
// toggle: self.diff.update(cx, |s, cx| s.set_mode(DiffMode::Split, cx));
```

## 2C. Components: navigation, data, controls, icons, theme, traits (gpui-component 0.11.0)

Paths are relative to `gpui_kit::component::`. Verified against `~/.cargo/registry/src/*/gpui-component-0.11.0`, `gpui-base-0.11.0`, `gpui-kit-assets-0.11.0`. The repo-main stories match 0.11; only 6 component files differ, none in this group except `sidebar/menu.rs` (a11y ids).

### Common imports
```rust
use gpui_kit::*;                                   // GPUI (Fast with the gpui-fast feature)
use gpui_kit::prelude::FluentBuilder as _;         // .when/.when_some/.when_none/.when_else/.map. NOT in the root glob
use gpui_kit::component::{h_flex, v_flex, ActiveTheme as _, Sizable as _, Size, Disableable as _,
    Selectable as _, StyledExt as _, ThemeStyled as _, Icon, IconName};
```
- `h_flex()` / `v_flex()` -> `Div`: re-exported from `gpui_base`, available as `gpui_kit::component::{h_flex, v_flex}`.
- `FluentBuilder` (gpui `util`): `map(self, f: impl FnOnce(Self)->U)->U`, `when(self, cond: bool, then)`, `when_else(self, cond, then, else_fn)`, `when_some<T>(self, Option<T>, then: FnOnce(Self,T)->Self)`, `when_none<T>(self, &Option<T>, then)`.

### Traits
- `Sizable` (`sizing.rs`): `fn with_size(self, size: impl Into<Size>) -> Self`, plus `xsmall()`, `small()`, `large()`. **There is no `.medium()` in 0.11.** Use `.with_size(Size::Medium)` (Medium is the default).
- `enum Size { Size(Pixels), XSmall, Small, Medium, Large }`, with `impl From<Pixels> for Size`. `Size::as_str()` returns `"xs" | "sm" | "md" | "lg" | "custom"`.
- `Disableable` (gpui_base): `fn disabled(self, disabled: bool) -> Self`.
- `Selectable` (gpui_base): `fn selected(self, bool) -> Self`, `fn is_selected(&self) -> bool`, `fn secondary_selected(self, bool) -> Self` (has a default).
- `Collapsible` (gpui_base::component_traits): `fn collapsed(self, bool) -> Self`, `fn is_collapsed(&self) -> bool`.
- `FocusableExt`: `focus_ring(self, bool)`, `is_focus_ring_enabled()`.
- `StyledExt` (gpui_base): `refine_style(&StyleRefinement)`, `h_flex()`, `v_flex()`, `paddings(impl Into<Edges<L>>)`, `margins(..)`, `debug_red/blue/yellow/green/pink()`, `debug_focused(&FocusHandle, &Window, &App)`, `corner_radii(Corners<Pixels>)`. **`popover_style` is not on StyledExt.**
- `ThemeStyled` (component `styled.rs`, implemented for every `Styled`): `focus_ring_style(self, &Window, &App)`, `popover_style(self, &App)`, `rounded_full_style(self, &App)`.
- `StyleSized<T>` (internal sizing helpers): `input_text_size/input_size/input_pl/pr/px/py/input_h/list_size/list_px/list_py/size_with/table_cell_size/button_text_size(Size)`, ...

---

### tab::{Tab, TabBar, TabVariant}
```rust
pub enum TabVariant { #[default] Tab, Outline, Pill, Segmented, Underline, Folder }
// Tab (RenderOnce; From<&'static str|String|SharedString|Icon|IconName>)
Tab::new() -> Self      // no id argument
.label(impl Into<SharedString>) .aria_label(..) .icon(impl Into<Icon>)
.with_variant(TabVariant) .pill() .outline() .segmented() .underline() .folder()
.prefix(impl IntoElement) .suffix(impl IntoElement) .disabled(bool)
.on_click(impl Fn(&ClickEvent, &mut Window, &mut App) + 'static)
// also ParentElement, Selectable, InteractiveElement, StatefulInteractiveElement, Styled, Sizable
// TabBar
TabBar::new(id: impl Into<ElementId>)
.with_variant(..) .pill() .outline() .segmented() .underline() .folder()
.menu(bool) .max_width(impl Into<Pixels>) .track_scroll(&ScrollHandle)
.prefix(impl IntoElement) .suffix(impl IntoElement) .last_empty_space(impl IntoElement)
.child(impl Into<Tab>) .children(impl IntoIterator<Item = impl Into<Tab>>)
.selected_index(usize)
.on_click<F: Fn(&usize, &mut Window, &mut App) + 'static>(F)  // setting this ignores each child's on_click
```
- **No `closable` in 0.11.** For a close button, put a `Button` in `Tab::suffix(...)`. Selection is controlled: store the index and pass `selected_index`.
```rust
TabBar::new("tabs").underline().selected_index(self.active_tab_ix)
    .on_click(cx.listener(|this, ix: &usize, window, cx| { this.active_tab_ix = *ix; cx.notify(); }))
    .child("Account").child("Profile")
    .child(Tab::new().label("Mail").suffix(Button::new("close-mail").ghost().xsmall().icon(IconName::Close)))
```

### collapsible::Collapsible
```rust
Collapsible::new() .open(bool) .motion_id(impl Into<ElementId>) /* animated measured reveal */ .content(impl IntoElement)
// Styled + ParentElement: .child() is the always-visible trigger row; .content() is hidden when closed.
```
State is owned by the app (a `bool` it stores). There is no event: wire your own trigger `Button::on_click`.

### accordion::{Accordion, AccordionItem}
```rust
Accordion::new(id) .multiple(bool) .bordered(bool) .disabled(bool)
  .item(|item: AccordionItem| -> AccordionItem)
  .on_toggle_click(impl Fn(&[usize], &mut Window, &mut App) + 'static)   // the indices that are open
  // Sizable
AccordionItem: .icon(impl Into<Icon>) .title(impl IntoElement) .open(bool) .disabled(bool)
  .title_style(StyleRefinement) .hover(FnOnce(StyleRefinement)->StyleRefinement) .content_style(StyleRefinement) .child(..)
```
```rust
Accordion::new("faq").multiple(false)
  .item(|this| this.open(self.open_ixs.contains(&0)).icon(IconName::Info).title("Is it accessible?").child("Yes."))
  .on_toggle_click(cx.listener(|this, open: &[usize], _, cx| { this.open_ixs = open.to_vec(); cx.notify(); }))
```

### tree::{Tree, tree, TreeState, TreeItem, TreeEntry, TreeEvent}
These are re-exported from `gpui_base`.
```rust
pub enum TreeEvent { Expanded(SharedString), Collapsed(SharedString) }   // impl EventEmitter<TreeEvent> for TreeState
// TreeItem (Clone; pub id: SharedString, pub label: SharedString, pub children: Vec<TreeItem>)
TreeItem::new(id: impl Into<SharedString>, label: impl Into<SharedString>)
  .child(TreeItem) .children(impl IntoIterator<Item=TreeItem>) .expanded(bool) .disabled(bool)
  .is_folder() .is_expanded() .is_disabled() .ancestors(&SharedString) -> Option<Vec<TreeItem>>
// TreeEntry (a flattened row): item() -> &TreeItem, depth(), is_root(), is_folder(), is_expanded(), is_disabled()
// TreeState
TreeState::new(cx: &mut App) -> Self      // cx.new(|cx| TreeState::new(cx))
  .items(impl Into<Vec<TreeItem>>)        // builder
  set_items(&mut self, items, cx: &mut Context<Self>)
  selected_index() -> Option<usize>; set_selected_index(Option<usize>, cx); set_selected_item(Option<&TreeItem>, cx)
  selected_item() -> Option<&TreeItem>; selected_entry() -> Option<&TreeEntry>; entry(ix) -> Option<&TreeEntry>
  scroll_handle() -> &UniformListScrollHandle; scroll_to_item(ix, ScrollStrategy); index_of(&SharedString) -> Option<usize>
  reveal_item(&SharedString, ScrollStrategy, cx)   // expands the ancestors, then scrolls
  focus(&mut self, window, cx: &mut App)
// Tree element (component)
tree(state: &Entity<TreeState>, render_item: R) -> Tree     // same as Tree::new(state, render_item)
  where R: Fn(usize, &TreeEntry, bool /*selected*/, &mut Window, &mut App) -> ListItem + 'static
.context_menu(Fn(usize, &TreeEntry, PopupMenu, &mut Window, &mut Context<TreeState>) -> PopupMenu)
.scroll_bounce(bool)  // also Styled
```
- The tree is uniform-list backed: every row has the same height.
- Selection emits no event. Use `ListItem::on_click` or read `selected_entry()` inside actions.
```rust
let tree_state = cx.new(|cx| TreeState::new(cx).items(vec![TreeItem::new("src", "src").expanded(true).child(TreeItem::new("src/main.rs", "main.rs"))]));
tree(&self.tree_state, move |ix, entry, _selected, _w, cx| {
    let icon = if !entry.is_folder() { IconName::File } else if entry.is_expanded() { IconName::FolderOpen } else { IconName::Folder };
    ListItem::new(ix).pl(px(16.) * entry.depth() + px(12.))
        .child(h_flex().gap_2().child(icon).child(entry.item().label.clone()))
})
```

### list::{List, ListState, ListDelegate, ListEvent, ListItem, ListSeparatorItem}; `IndexPath` (crate root)
```rust
pub enum ListEvent { Select(IndexPath), Confirm(IndexPath), Cancel }   // impl EventEmitter<ListEvent> for ListState<D>
pub trait ListDelegate: Sized + 'static {
    type Item: Selectable + IntoElement;
    fn perform_search(&mut self, query: &str, window: &mut Window, cx: &mut Context<ListState<Self>>) -> Task<()> { Task::ready(()) }
    fn sections_count(&self, cx: &App) -> usize { 1 }
    fn items_count(&self, section: usize, cx: &App) -> usize;                                  // REQUIRED
    fn render_item(&mut self, ix: IndexPath, window: &mut Window, cx: &mut Context<ListState<Self>>) -> Option<Self::Item>; // REQUIRED; every item has the same height
    fn render_section_header(&mut self, section: usize, window, cx) -> Option<impl IntoElement> { None }
    fn render_section_footer(&mut self, section: usize, window, cx) -> Option<impl IntoElement> { None }
    fn render_empty(&mut self, window, cx) -> impl IntoElement { /* Inbox icon */ }
    fn render_initial(&mut self, window, cx) -> Option<AnyElement> { None }
    fn loading(&self, cx: &App) -> bool { false }
    fn render_loading(&mut self, window, cx) -> impl IntoElement { Loading }
    fn set_selected_index(&mut self, ix: Option<IndexPath>, window: &mut Window, cx: &mut Context<ListState<Self>>); // REQUIRED
    fn set_right_clicked_index(&mut self, ix: Option<IndexPath>, window, cx) {}
    fn confirm(&mut self, secondary: bool, window: &mut Window, cx: &mut Context<ListState<Self>>) {}
    fn cancel(&mut self, window: &mut Window, cx: &mut Context<ListState<Self>>) {}
    fn has_more(&self, cx: &App) -> bool { false }
    fn load_more_threshold(&self) -> usize { 20 }
    fn load_more(&mut self, window: &mut Window, cx: &mut Context<ListState<Self>>) {}
}
// ListState<D>
ListState::new(delegate: D, window: &mut Window, cx: &mut Context<Self>) -> Self
  .searchable(bool) .selectable(bool)    // builders; set_searchable/set_selectable(&mut, bool, cx)
  delegate() / delegate_mut(); focus(window, cx: &mut App)
  set_selected_index(Option<IndexPath>, window, cx); selected_index() -> Option<IndexPath>
  set_right_clicked_index(..); right_clicked_index(); set_query(&str, window, cx)
  set_item_to_measure_index(IndexPath, window, cx); scroll_to_item(IndexPath, ScrollStrategy, window, cx)
  scroll_handle() -> &VirtualListScrollHandle; scroll_to_selected_item(window, cx)
// List element
List::new(&Entity<ListState<D>>) .scrollbar_visible(bool) .scroll_bounce(bool) .search_placeholder(impl Into<SharedString>)  // + Styled, Sizable
// ListItem (RenderOnce; Disableable, Selectable, Styled, ParentElement, InteractiveElement, StatefulInteractiveElement)
ListItem::new(id: impl Into<ElementId>) .separator() .check_icon(impl Into<Icon>) .selected(bool) .confirmed(bool)
  .accessibility_label(..) .disabled(bool) .suffix(Fn(&mut Window,&mut App)->E)
  .on_click(Fn(&ClickEvent,&mut Window,&mut App)) .on_mouse_down(MouseButton, Fn(&MouseDownEvent,..)) .on_mouse_enter(Fn(&MouseMoveEvent,..))
```
```rust
let list = cx.new(|cx| ListState::new(delegate, window, cx).searchable(true));
let _sub = cx.subscribe(&list, |_, _, ev: &ListEvent, _| match ev { ListEvent::Confirm(ix) => {}, _ => {} });
List::new(&self.list).flex_1().w_full()
```

### Virtual lists: v_virtual_list / h_virtual_list / VirtualListScrollHandle
These live in `gpui_base` and are re-exported at `gpui_kit::component::{v_virtual_list, h_virtual_list, VirtualList, VirtualListScrollHandle}`.
```rust
pub fn v_virtual_list<R: IntoElement, V: Render>(
    view: Entity<V>, id: impl Into<ElementId>,
    item_sizes: Rc<Vec<Size<Pixels>>>,      // yes, Rc<Vec<Size<Pixels>>>. Vertical uses only .height (width is measured)
    f: impl 'static + Fn(&mut V, Range<usize>, &mut Window, &mut Context<V>) -> Vec<R>,
) -> VirtualList                            // h_virtual_list: same signature, uses .width
VirtualList: .track_scroll(&VirtualListScrollHandle) .with_scroll_handle(&..) .with_sizing_behavior(ListSizingBehavior)
             .with_item_to_measure_index(usize)  // + Styled
VirtualListScrollHandle::new(); .base_handle() -> &ScrollHandle; .scroll_to_item(ix, ScrollStrategy); .scroll_to_bottom()
```
Sizes must be known up front: changing heights means rebuilding the `Rc<Vec<..>>`.
```rust
v_virtual_list(cx.entity(), "items", self.item_sizes.clone(), |this, range, _, cx| {
    range.map(|ix| div().h(px(30.)).child(this.items[ix].clone())).collect()
}).track_scroll(&self.scroll_handle)
// the parent is typically .relative().size_full(), with .vertical_scrollbar(&self.scroll_handle) on the parent
```

### scroll::{Scrollbar, ScrollableElement, Scrollable, ScrollbarAxis, ScrollbarMode, ScrollbarHandle, AutoScroll, ...}
```rust
pub enum ScrollbarAxis { Vertical, Horizontal, Both }
pub enum ScrollbarMode { #[default] Scrolling /*fade after idle*/, Hover, Always }
pub trait ScrollableElement: InteractiveElement + Styled + ParentElement + Element {   // blanket for Div/Stateful<Div>
    fn scrollbar<H: ScrollbarHandle + Clone>(self, &H, axis: impl Into<ScrollbarAxis>) -> Self;  // overlay for an external handle
    fn vertical_scrollbar<H: ScrollbarHandle + Clone>(self, &H) -> Self;
    fn horizontal_scrollbar<H: ScrollbarHandle + Clone>(self, &H) -> Self;
    fn overflow_scrollbar(self) -> Scrollable<Self>;     // == overflow_scroll + scrollbars (owns its own handle)
    fn overflow_x_scrollbar(self) -> Scrollable<Self>;
    fn overflow_y_scrollbar(self) -> Scrollable<Self>;
}
Scrollable: .id(..) .scroll_bounce(bool)
// ScrollbarHandle is implemented for ScrollHandle, UniformListScrollHandle, gpui ListState, VirtualListScrollHandle
Scrollbar::new(&H) / ::vertical(&H) / ::horizontal(&H) .id(..) .mode(ScrollbarMode) .scroll_size(Size<Pixels>)
  .viewport_bounds(Bounds<Pixels>) .viewport_from_layout() .styles(|s| s.track(..).thumb(..).thumb_hover(..).thumb_active(..))
```
How to scroll a div with a scrollbar in 0.11: `div().id("page").size_full().overflow_y_scrollbar().child(...)`. To use a gpui `ListState` or `ScrollHandle`, use `.vertical_scrollbar(&list_state)` on the container.
Set it globally with `Theme::set_scrollbar_mode(ScrollbarMode::Always, cx)`. `Theme::sync_scrollbar_appearance(cx)` follows the OS setting.

### switch::Switch / checkbox::Checkbox / radio::{Radio, RadioGroup}
These are all controlled. The handler receives the *requested* value; the owner stores it and calls `cx.notify()`. `on_click` is an alias of `on_change`: the last one set wins.
```rust
Switch::new(id) .checked(bool) .label(impl Into<Text>) .accessibility_label(..)
  .on_change/on_click(Fn(&bool,&mut Window,&mut App)) .color(impl Into<Hsla>) .tooltip(..) .tooltip_show_delay(Duration)
  .tab_stop(bool) .tab_index(isize)              // Styled, Sizable, Disableable, FocusableExt
Checkbox::new(id) .label(..) .checked(bool) .indeterminate(bool) .on_change/on_click(Fn(&bool,..)) .role(..) .tooltip(..)
  .tab_stop .tab_index                           // Disableable, Selectable, Sizable, ParentElement, Styled, Interactive
Radio::new(id) .label(..) .checked(bool) .disabled(bool) .on_change/on_click(Fn(&bool,..)) .tab_index .tab_stop .tooltip
  // Radio: From<&'static str|SharedString|String>; ParentElement (extra description under the label)
RadioGroup::new(id) | ::vertical(id) | ::horizontal(id) .layout(Axis) .selected_index(Option<usize>) .disabled(bool)
  .on_change/on_click(Fn(&usize,&mut Window,&mut App)) .child(impl Into<Radio>) .children(..)   // Styled
```
```rust
Switch::new("autosave").checked(self.autosave)
  .on_click(cx.listener(|this, checked: &bool, _, cx| { this.autosave = *checked; cx.notify(); }))
RadioGroup::horizontal("mode").selected_index(Some(self.mode_ix)).child("Unified").child("Split")
  .on_change(cx.listener(|this, ix: &usize, _, cx| { this.mode_ix = *ix; cx.notify(); }))
```

### form::{v_form, h_form, field, Form, Field}
```rust
v_form() -> Form; h_form() -> Form; field() -> Field   // Form::new()/::vertical()/::horizontal(); Field::new()
Form: .layout(Axis) .label_layout(Axis) .label_width(Pixels) .label_text_size(Rems) .columns(usize)
      .child(impl Into<Field>) .children(IntoIterator<Item=Field>) .footer(impl IntoElement)   // + Sizable
Field: .label(impl Into<FieldBuilder>) .label_fn(Fn(&mut Window,&mut App)->E) .label_indent(bool)
       .description(..) .description_fn(..) .visible(bool) .required(bool)
       .items_start() .items_end() .items_center() .col_span(u16) .col_start(i16) .col_end(i16) .child(..)
```
```rust
v_form().label_width(px(140.))
  .child(field().label("Email").required(true).child(Input::new(&self.email_input)))
```

### setting::{Settings, SettingPage, SettingGroup, SettingItem, SettingField, NumberFieldOptions, RenderOptions}
```rust
Settings::new(id) .sidebar_width(..) .sidebar_size_range(Range<Pixels>) .page(SettingPage) .pages(IntoIterator)
  .with_group_variant(GroupBoxVariant) .sidebar_style(&StyleRefinement) .header_style(&..) .default_selected_index(SelectIndex)  // + Sizable
SettingPage::new(title) .title(..) .title_suffix(Fn(&mut Window,&mut App)->E) .icon(impl Into<Icon>) .description(..)
  .default_open(bool) .resettable(bool) .group(SettingGroup) .groups(IntoIterator) .header_style(&..)
SettingGroup::new() .title(..) .description(..) .variant(GroupBoxVariant) .footer(Fn->E) .item(SettingItem) .items(IntoIterator)
SettingItem::new(title, field: impl AnySettingField)   // enum SettingItem { Item{..}, Element{..} }
SettingItem::render(Fn(&RenderOptions,&mut Window,&mut App)->E)
  .on_reset(is_dirty, reset) .keywords(IntoIterator<S>) .disabled(bool) .description(impl Into<Text>) .layout(Axis)
SettingField::switch(value: Fn(&App)->bool, set: Fn(bool,&mut App))   // also ::checkbox
SettingField::input(Fn(&App)->SharedString, Fn(SharedString,&mut App))
SettingField::dropdown(Vec<(SharedString /*value*/, SharedString /*label*/)>, value, set)   // ::scrollable_dropdown for long lists
SettingField::number_input(NumberFieldOptions, value, set); ::element(E); ::render(Fn->E)
  .default_value(impl Into<T>) .on_reset(is_dirty, reset)
```
Values are read and written through closures over `&App`/`&mut App`, so they usually live in a `Global`. The search and sidebar are built in.
```rust
Settings::new("app-settings").pages(vec![SettingPage::new("General").icon(Icon::new(IconName::Settings2)).groups(vec![
  SettingGroup::new().title("Appearance").items(vec![SettingItem::new("Dark Mode",
    SettingField::switch(|cx: &App| cx.theme().mode.is_dark(),
      |v: bool, cx: &mut App| Theme::change(if v { ThemeMode::Dark } else { ThemeMode::Light }, None, cx))
    .default_value(false)).description("Switch between light and dark themes.")])])])
```

### status_bar::StatusBar
`StatusBar::new() .left(impl IntoElement) .right(impl IntoElement)` (ParentElement + Styled). The theme tokens are `status_bar` and `status_bar_border`.

### avatar::{Avatar, AvatarGroup}
`Avatar::new() .src(impl Into<ImageSource>) .name(impl Into<SharedString>) /*initials*/ .placeholder(impl Into<Icon>)` (Sizable, Styled, InteractiveElement).
`AvatarGroup::new() .child(Avatar) .children(..) .limit(usize) .ellipsis()`.

### clipboard::Clipboard
`Clipboard::new(id) .label(..) .tooltip(..) .tooltip_show_delay(Duration) .accessibility_label(..) .value(impl Into<SharedString>) .value_fn(Fn(&mut Window,&mut App)->SharedString) .on_copied(Fn(SharedString,&mut Window,&mut App))`. This renders a copy icon button that copies for you.

### separator::Separator
`Separator::horizontal() | ::vertical() | ::horizontal_dashed() | ::vertical_dashed()`, then `.label(..)`, `.color(impl Into<Hsla>)`, `.dashed()`. There is no `Separator::new` in 0.11.

### label::Label
`Label::new(impl Into<SharedString>) .id(..) .secondary(impl Into<SharedString>) .masked(bool) .highlights(impl Into<HighlightsMatch>)`. It is Styled. `enum HighlightsMatch` supports prefix and contains matching.

### group_box::{GroupBox, GroupBoxVariant, GroupBoxVariants}
`enum GroupBoxVariant { Normal (default), Fill, Outline }`. You need the trait `GroupBoxVariants` in scope for `.normal()`, `.fill()`, `.outline()`, `.with_variant()`.
`GroupBox::new() .id(..) .title(impl IntoElement) .title_style(..) .content_style(..) .footer(impl IntoElement)` (ParentElement + Styled).

---

### Icon / IconName
```rust
Icon::new(icon: impl Into<Icon>)      // accepts component::IconName, assets::IconName, any IconNamed
Icon::default().path("icons/foo.svg") // an asset path (resolved through set_icon_resolver)
Icon::default().data(&[u8])           // raw SVG bytes, copied
Icon::empty(); .rotate(impl Into<Radians>); .transform(Transformation); .view(cx) -> Entity<Icon>
// Styled (.size_4(), .text_color(...)), Sizable (.small(), .with_size(px(..)))
// IconName itself is RenderOnce + Into<AnyElement>: `.child(IconName::Check)` works
set_icon_resolver(cx, Fn(&str, &App) -> Option<SharedString>)   // swap icon paths app-wide; then cx.refresh_windows()
```
**Asset registration** is required, otherwise icons render blank (debug builds log a warning):
```rust
gpui_kit::application().with_assets(gpui_kit::assets::Assets)   // embeds ONLY the 106 default component icons
// gpui_kit::assets::AllAssets embeds all 1830 icons (+1 MiB)
// Or pick extras: gpui_kit::assets::icon_assets!(ExtraIcons, [Send, Paperclip, Sparkles]);
// then write your own AssetSource that tries ExtraIcons.load(path)? and falls back to Assets.load(path).
```
- There are **two enums**. `gpui_kit::component::IconName` is the legacy enum with exactly the default icons. `gpui_kit::assets::IconName` is the full Lucide 1.43 catalog (1818) plus 12 kit icons; it has `IconName::ALL` and `.path()`, and you need `component::IconNameExt` in scope for `.view(cx)`.
- Variant names are generated from the file stem: split on `-_.`, then each part is Capitalized and lowercased. For example `trash-2.svg` gives `Trash2`, `message-square.svg` gives `MessageSquare`, `git-branch.svg` gives `GitBranch`.

Full `component::IconName` list (106): ALargeSmall, ArrowDown, ArrowLeft, ArrowRight, ArrowUp, Asterisk, Ban, Battery, BatteryCharging, BatteryFull, BatteryLow, BatteryMedium, BatteryWarning, Bell, BookOpen, Bot, Building2, Calendar, CaseSensitive, ChartPie, Check, ChevronDown, ChevronLeft, ChevronRight, ChevronUp, ChevronsUpDown, CircleAlert, CircleCheck, CircleUser, CircleX, Close, Copy, Cpu, Dash, Delete, Ellipsis, EllipsisVertical, ExternalLink, Eye, EyeOff, File, FileText, Folder, FolderClosed, FolderOpen, Frame, GalleryVerticalEnd, Github, Globe, HardDrive, Heart, HeartOff, Inbox, Info, Inspector, LayoutDashboard, Loader, LoaderCircle, Map, Maximize, MemoryStick, Menu, Mic, Minimize, Minus, Moon, Network, Palette, PanelBottom, PanelBottomOpen, PanelLeft, PanelLeftClose, PanelLeftOpen, PanelRight, PanelRightClose, PanelRightOpen, Pause, Play, Plus, Redo, Redo2, RefreshCw, Replace, ResizeCorner, RotateCw, Search, Settings, Settings2, SortAscending, SortDescending, Square, SquareTerminal, Star, StarFill, StarOff, Sun, ThumbsDown, ThumbsUp, TriangleAlert, Undo, Undo2, User, WindowClose, WindowMaximize, WindowMinimize, WindowRestore.

The component enum has **no** Send, Paperclip, Trash, Sparkles, MessageSquare, GitBranch, Terminal, Clock, or History. History is not in the full catalog either. The others exist in `assets::IconName` (`Send`, `Paperclip`, `Trash`, `Sparkles`, `MessageSquare`, `GitBranch`, `GitPullRequest`, `Terminal`, `SquarePen`, `CircleStop`, `ArrowUpToLine`, `CopyCheck`, `Clock`, ...) and need `AllAssets` or `icon_assets!`.

---

### Theme: `ActiveTheme`, `Theme`, `ThemeColor`, `ThemeMode`, `ThemeRegistry`, `ThemeConfig`, `ThemeSet` (all at `gpui_kit::component::`)
```rust
pub trait ActiveTheme { fn theme(&self) -> &Theme; }   // impl for App, so it works through Context/Window deref: cx.theme()
#[non_exhaustive] pub struct Theme {           // Deref/DerefMut<Target = ThemeColor>, so cx.theme().primary works
  pub colors: ThemeColor, pub tokens: ThemeTokens /*legacy renderable (gradient) copies*/,
  pub highlight_theme: Arc<HighlightTheme>, pub light_theme: Rc<ThemeConfig>, pub dark_theme: Rc<ThemeConfig>,
  pub mode: ThemeMode, pub font_family: SharedString /* ".SystemUIFont" */, pub font_size: Pixels /*16px*/,
  pub mono_font_family: SharedString /* Win: Consolas */, pub mono_font_size: Pixels /*13px*/,
  pub radius: Pixels, pub radius_lg: Pixels, pub window_radius: Pixels, pub shadow: bool, pub focus_ring: bool,
  pub transparent: Hsla, pub scrollbar_mode: ScrollbarMode, pub notification: NotificationSettings,
  pub list: ListSettings, pub sheet: SheetSettings, pub motion: MotionTokens, pub cursor_blink: CursorBlink }
pub enum ThemeMode { #[default] Light, Dark }   // is_dark(), name() -> "light"|"dark"; From<WindowAppearance>
```
Methods:
- `Theme::global(cx) -> &Theme`
- `Theme::global_mut(cx) -> &mut Theme`: does NOT resync; call `Theme::sync_base(cx)` and refresh yourself.
- `Theme::update<R>(cx: &mut App, edit: impl FnOnce(&mut Theme) -> R) -> R`: the preferred write path. It reconciles tokens, syncs Base, and refreshes all windows.
- `Theme::change(mode: impl Into<ThemeMode>, _window: Option<&mut Window>, cx: &mut App)`: `window` is ignored and you can pass `None`. It reloads the registered config for that mode.
- `Theme::sync_system_appearance(window: Option<&mut Window>, cx)`: a one-shot read of `window.appearance()` (or the app's appearance).
- `Theme::sync_scrollbar_appearance(cx)`, `Theme::set_scrollbar_mode(ScrollbarMode, cx)`, `Theme::sync_base(cx)`
- `theme.is_dark()`, `theme.theme_name() -> &SharedString`, `theme.input_background() -> Hsla`, `theme.apply_config(&Rc<ThemeConfig>)`
- `radius_full()`, `radius_2xl()` (2.5x), `radius_3xl()` (3x), `radius_4xl()` (3.5x)
- `semantic_tokens() -> SemanticThemeTokens { colors: ColorTokens, radius: RadiusTokens, spacing: SpacingTokens, typography: TypographyTokens, shadow: ShadowTokens }`
- `color_tokens() -> ColorTokens { background, foreground, surface, surface_foreground, primary, primary_foreground, secondary, secondary_foreground, muted, muted_foreground, accent, accent_foreground, destructive, destructive_foreground, border, input, ring, selection }`
- `radius_tokens() -> RadiusTokens { none, sm (radius/2), md, lg, xl, full }`
- `spacing_tokens() -> SpacingTokens`: always `SpacingTokens::default()`, which is `{ xxs 2, xs 4, sm 8, md 12, lg 16, xl 24, xxl 32 }` px.
- `typography_tokens() -> TypographyTokens { sans, mono, xs(12/16), sm(14/20), md(16/24), lg(18/28), xl(20/28), mono_md(13/20) }` (`TextStyleToken { size, line_height }`)
- `shadow_tokens() -> ShadowTokens { sm, md, lg: Vec<BoxShadow> }` (empty when `shadow == false`)
- `motion_tokens() -> &MotionTokens`
- `apply_semantic_tokens(&SemanticThemeTokens)`, `apply_semantic_config(&SemanticThemeConfig)`, `apply_semantic_config_str(&str) -> anyhow::Result<..>`

**System dark mode is not followed automatically.** `component::init` calls `Theme::change(ThemeMode::Light, None, cx)`. Do it yourself:
```rust
Theme::sync_system_appearance(Some(window), cx);                // at startup, inside open_window's build
let _sub = cx.observe_window_appearance(window, |_, window, cx| Theme::sync_system_appearance(Some(window), cx));
```

`ThemeColor` tokens (141 `Hsla` fields): accent, accent_foreground, accordion, background, border, button, button_active, button_foreground, button_hover, button_danger, button_danger_active, button_danger_foreground, button_danger_hover, button_info, button_info_active, button_info_foreground, button_info_hover, button_primary, button_primary_active, button_primary_foreground, button_primary_hover, button_secondary, button_secondary_active, button_secondary_foreground, button_secondary_hover, button_success, button_success_active, button_success_foreground, button_success_hover, button_warning, button_warning_active, button_warning_foreground, button_warning_hover, group_box, group_box_foreground, caret, chart_1, chart_2, chart_3, chart_4, chart_5, chart_bullish, chart_bearish, chart_grid, danger, danger_active, danger_foreground, danger_hover, description_list_label, description_list_label_foreground, drag_border, drop_target, foreground, info, info_active, info_foreground, info_hover, input, link, link_active, link_hover, list, list_active, list_active_border, list_even, list_head, list_hover, list_highlight_foreground, muted, muted_foreground, popover, popover_foreground, primary, primary_active, primary_foreground, primary_hover, progress_bar, ring, scrollbar, scrollbar_thumb, scrollbar_thumb_hover, secondary, secondary_active, secondary_foreground, secondary_hover, selection, sidebar, sidebar_accent, sidebar_accent_foreground, sidebar_border, sidebar_foreground, sidebar_primary, sidebar_primary_foreground, skeleton, slider_bar, slider_thumb, success, success_foreground, success_hover, success_active, switch, switch_thumb, tab, tab_active, tab_active_foreground, tab_bar, tab_bar_segmented, tab_foreground, table, table_active, table_active_border, table_even, table_head, table_head_foreground, table_foot, table_foot_foreground, table_hover, table_row_border, title_bar, title_bar_border, status_bar, status_bar_border, warning, warning_active, warning_hover, warning_foreground, overlay, window_border, red, red_light, green, green_light, blue, blue_light, yellow, yellow_light, magenta, magenta_light, cyan, cyan_light.

There is no `destructive`, `card`, or `surface` field in `ThemeColor`: use `danger`, `popover`/`group_box`, and `secondary`. Those names exist only in `ColorTokens`.

**ThemeRegistry** (`ThemeRegistry::global(cx)` / `global_mut(cx)`):
- `watch_dir(themes_dir: PathBuf, cx: &mut App, on_load: impl Fn(&mut App) + 'static) -> Result<()>`: native only, hot-reloads `*.json`.
- `load_themes_from_str(&mut self, content: &str) -> anyhow::Result<()>`: one `ThemeSet` JSON, `{ name, author, themes: [ThemeConfig] }`.
- `themes() -> &HashMap<SharedString, Rc<ThemeConfig>>`, `sorted_themes()`, `default_themes()`, `default_light_theme()`, `default_dark_theme()`

The built-in themes are only "Default Light" and "Default Dark" (embedded `default-theme.json`). **The named themes are NOT bundled in the crate.** They are JSON files in the repo's `themes/` folder (`nsh-research/gpui-kit/themes/*.json`), and you copy them into the app and load them with `include_str!` + `load_themes_from_str`, or with `watch_dir`. Available: Adventure Time, Alduin, Asciinema, Aurora Light, Ayu Light/Dark, Catppuccin Latte/Frappe/Macchiato/Mocha, Everforest Light/Dark, Fahrenheit, Flexoki Light/Dark, Gruvbox Light/Dark, Harper, Hybrid Light/Dark, Jellybeans, Kibble, macOS Classic Light/Dark, Mellifluous Light/Dark, Molokai Light/Dark, Solarized Light/Dark, Spaceduck, Tokyo Night/Storm/Moon, Twilight.
```rust
// apply a named theme (story themes.rs)
let reg = ThemeRegistry::global_mut(cx);
reg.load_themes_from_str(include_str!("../themes/catppuccin.json")).ok();
if let Some(cfg) = ThemeRegistry::global(cx).themes().get("Catppuccin Mocha").cloned() {
    Theme::update(cx, |theme| theme.apply_config(&cfg));   // apply_config also switches the mode
}
Theme::update(cx, |t| { t.radius = px(8.); t.radius_lg = px(10.); });
```
