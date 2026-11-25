//! The one action an empty screen offers: a button that names the command,
//! and the keys that run it, read from the live keymap so the user's own
//! bindings show. Used with [`crate::ui::empty_state`].

use gpui_kit::component::button::Button;
use gpui_kit::component::{ActiveTheme as _, Icon, h_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

/// `[icon Label…]  Ctrl N`, dispatching `action`.
pub fn action(
    id: impl Into<ElementId>,
    icon: impl Into<Icon>,
    label: impl Into<SharedString>,
    action: Box<dyn Action>,
    cx: &App,
) -> AnyElement {
    let keys = crate::ui::keycaps(action.as_ref(), cx);
    h_flex()
        .gap_2()
        .child(
            Button::new(id)
                .outline()
                .icon(icon)
                .label(label)
                .on_click(move |_, window, cx| window.dispatch_action(action.boxed_clone(), cx)),
        )
        .when_some(keys, |el, keys| el.child(div().text_xs().text_color(cx.theme().muted_foreground).child(keys)))
        .into_any_element()
}
