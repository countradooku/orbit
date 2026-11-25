//! The composer's side of drafts: restore on open, save after a pause.
//!
//! The host keeps drafts ([`Command::SaveDraft`]); this saves the text and
//! attachments 400 ms after the last edit, at once when the composer loses
//! focus or closes, and clears the draft when the message is sent. A draft
//! changed on another client is adopted only while this composer has no
//! unsaved edits and is not being typed in, so two windows never fight.

use std::time::Duration;

use gpui_kit::*;
use orbit_proto::*;

use super::Composer;

/// Quiet time after the last edit before the draft is saved.
const SAVE_AFTER: Duration = Duration::from_millis(400);

impl Composer {
    fn current_draft(&self, cx: &App) -> Draft {
        Draft { text: self.input.read(cx).value().to_string(), attachments: self.attachments.clone(), updated_ms: 0 }
    }

    /// Loads the host's draft for this composer, if there is one.
    pub(super) fn restore_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(draft) = self.app.read(cx).state().drafts.get(&self.target).cloned() else { return };
        self.attachments = draft.attachments.clone();
        self.replace_text(&draft.text, window, cx);
        self.saved = draft;
        self.save_task = None;
    }

    pub(super) fn schedule_draft_save(&mut self, cx: &mut Context<Self>) {
        self.save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_AFTER).await;
            let _ = this.update(cx, |this, cx| this.save_draft_now(cx));
        }));
    }

    /// Hands the draft to the host now if it changed since the last save.
    pub(super) fn save_draft_now(&mut self, cx: &mut Context<Self>) {
        self.save_task = None;
        self.flush_draft(cx);
    }

    /// Saves without a `Context` (the composer is being released).
    pub(super) fn flush_draft(&mut self, cx: &mut App) {
        let draft = self.current_draft(cx);
        let unchanged = self.saved.same_content(&draft) || (draft.is_empty() && self.saved.is_empty());
        if unchanged {
            return;
        }
        self.saved = draft.clone();
        self.app.read(cx).send(Command::SaveDraft { target: self.target, draft });
    }

    /// The message went out: nothing to keep.
    pub(super) fn clear_draft(&mut self, cx: &mut Context<Self>) {
        self.save_task = None;
        if !self.saved.is_empty() {
            self.saved = Draft::default();
            self.app.read(cx).send(Command::SaveDraft { target: self.target, draft: Draft::default() });
        }
    }

    /// Another client (or a restore) changed this composer's draft.
    pub(super) fn adopt_remote_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let remote = self.app.read(cx).state().drafts.get(&self.target).cloned().unwrap_or_default();
        if remote.same_content(&self.saved) {
            return; // our own save coming back
        }
        let local = self.current_draft(cx);
        let unsaved_edits = !local.same_content(&self.saved) || self.save_task.is_some();
        if unsaved_edits || self.input_focused(window, cx) {
            return;
        }
        self.attachments = remote.attachments.clone();
        self.replace_text(&remote.text, window, cx);
        self.saved = remote;
        self.save_task = None;
    }
}
