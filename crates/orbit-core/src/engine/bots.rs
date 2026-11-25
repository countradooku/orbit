//! The engine's side of the chat bots ([`Command::Bots`]) and of commands
//! made on someone's behalf ([`Command::Via`]).
//!
//! The bots themselves (`orbit-bots`) run in the engine's process as an
//! ordinary engine client. They attach a [`BotLink`] with
//! [`crate::EngineHandle::attach_bots`]; the engine forwards bot commands
//! to it and answers with what it replies. Tokens are the exception: the
//! engine keeps them in the integrations file and hands them to the bots
//! over the link, so they never travel to a client or into a log.

use crossfire::oneshot::{TxOneshot, oneshot};
use crossfire::{MTx, mpsc};
use orbit_proto::*;

use super::integrations;
use super::orchestrator::{CommandResult, Orchestrator, Outcome};

/// What the engine sends the bots.
pub enum BotCall {
    /// A [`Command::Bots`] to answer.
    Command { command: BotCommand, reply: TxOneshot<CommandResult> },
    /// The platforms' tokens: at attach and whenever they change.
    Secrets(BotSecrets),
}

pub type BotLink = MTx<mpsc::List<BotCall>>;

#[derive(Default)]
pub(crate) struct BotsLink {
    link: Option<BotLink>,
}

impl Orchestrator {
    pub(crate) fn attach_bots(&mut self, link: BotLink) {
        let secrets = integrations::load(&self.store.dir().join(integrations::FILE)).bots;
        let _ = link.send(BotCall::Secrets(secrets));
        self.bots.link = Some(link);
    }

    pub(crate) fn bots_command(&mut self, command: BotCommand) -> Outcome {
        if let BotCommand::SetTokens { platform, token, app_token, clear } = &command {
            let path = self.store.dir().join(integrations::FILE);
            let saved = integrations::update(&path, |i| i.bots.set(*platform, token, app_token, *clear));
            return match saved {
                Ok(saved) => {
                    if let Some(link) = &self.bots.link {
                        let _ = link.send(BotCall::Secrets(saved.bots));
                    }
                    Outcome::Ready(Ok(Reply::Done))
                }
                Err(e) => Outcome::Ready(Err(format!("Couldn’t save the token: {e}"))),
            };
        }
        let Some(link) = &self.bots.link else {
            return Outcome::Ready(Err("Chat bots don’t run on this Orbit host.".into()));
        };
        let (reply, rx) = oneshot();
        if link.send(BotCall::Command { command, reply }).is_err() {
            self.bots.link = None;
            return Outcome::Ready(Err("The chat bots have stopped.".into()));
        }
        Outcome::Deferred(Box::pin(async move { rx.await.unwrap_or_else(|_| Err("The chat bots have stopped.".into())) }))
    }

    /// `Command::Via`: an action for someone outside the app, noted in the
    /// timeline. Guarded approvals are refused; the person has to come to
    /// the app for those.
    pub(crate) fn via(&mut self, via: Via, action: ViaAction) -> CommandResult {
        let by = |what: &str| format!("{what} via {} by {}", via.channel, via.user);
        match action {
            ViaAction::Send { thread, text, steer } => {
                let sendable = self.store.thread(thread).is_some_and(|t| !t.archived) && !text.trim().is_empty();
                let steering = steer && self.runtimes.get(&thread).is_some_and(|r| r.turn.is_some());
                if sendable {
                    self.notice(thread, NoticeLevel::Info, by(if steering { "Steered" } else { "Sent" }));
                }
                self.send(thread, text, Vec::new(), steer)
            }
            ViaAction::Resolve { thread, request, decision } => {
                let approves = matches!(decision, Decision::Allow | Decision::AllowForSession);
                if approves && let Some(hit) = self.guard_hit(thread, request) {
                    return Err(format!("Guarded ({}): approve this in the Orbit app. You can still deny it here.", hit.label));
                }
                let what = match &decision {
                    Decision::Allow => "Approved",
                    Decision::AllowForSession => "Approved for this session",
                    Decision::Deny { .. } => "Denied",
                    Decision::Answer { .. } => "Answered",
                };
                let result = self.resolve_guarded(thread, request, decision);
                if result.is_ok() {
                    self.notice(thread, NoticeLevel::Info, by(what));
                }
                result
            }
            ViaAction::Stop { thread, force } => {
                let busy = self.runtimes.get(&thread).is_some_and(|r| r.turn.is_some());
                let result = if force { self.force_stop(thread) } else { self.interrupt(thread) };
                if result.is_ok() && busy {
                    self.notice(thread, NoticeLevel::Info, by(if force { "Force-stopped" } else { "Stopped" }));
                }
                result
            }
            ViaAction::NewSession { thread } => {
                let result = self.new_session(thread);
                if result.is_ok() {
                    self.notice(thread, NoticeLevel::Info, by("New session started"));
                }
                result
            }
            ViaAction::Start(start) => {
                let result = self.handle_command(Command::StartThread(start));
                if let Ok(Reply::Thread(thread)) = &result {
                    self.notice(*thread, NoticeLevel::Info, by("Started"));
                }
                result
            }
        }
    }
}
