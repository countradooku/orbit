//! Errors with a stable code, a message for people and an exit status.

use serde_json::{Value, json};

use crate::spec::exit;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliError {
    /// Process exit status, one of [`exit`].
    pub exit: i32,
    pub message: String,
    /// What to do next, when there is something useful to say.
    pub hint: Option<String>,
}

impl CliError {
    pub fn new(exit: i32, message: impl Into<String>) -> Self {
        Self { exit, message: message.into(), hint: None }
    }

    pub fn usage(message: impl Into<String>) -> Self {
        Self::new(exit::USAGE, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(exit::NOT_FOUND, message)
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(exit::UNAVAILABLE, message)
    }

    /// The engine refused or failed a command.
    pub fn engine(message: impl Into<String>) -> Self {
        Self::new(exit::ERROR, message)
    }

    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    /// The stable name of the exit code.
    pub fn code(&self) -> &'static str {
        exit::ALL.iter().find(|(code, _, _)| *code == self.exit).map(|(_, name, _)| *name).unwrap_or("error")
    }

    pub fn to_json(&self) -> Value {
        let mut error = json!({"code": self.code(), "exit": self.exit, "message": self.message});
        if let Some(hint) = &self.hint {
            error["hint"] = Value::String(hint.clone());
        }
        json!({"ok": false, "error": error})
    }

    /// `error: message` and the hint on its own line.
    pub fn human(&self) -> String {
        match &self.hint {
            Some(hint) => format!("error: {}\n{hint}", self.message),
            None => format!("error: {}", self.message),
        }
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CliError {}
