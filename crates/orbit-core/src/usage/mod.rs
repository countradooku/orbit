//! Usage, quota and accounts: the pure parts the engine (`engine/usage.rs`)
//! and the clients share.
//!
//! * [`claude`]: Claude Code's sign-in (its OAuth credentials, read-only)
//!   and its usage endpoint, the same one `/usage` in Claude Code shows.
//! * Codex limits come from `account/rateLimits/read` on a short-lived
//!   `codex app-server` ([`crate::provider::codex::read_limits`]).
//! * [`burn`]: the history of each window, its burn rate and when it runs
//!   out at that pace.
//! * [`ledger`]: one record per finished turn, and the stats and CSV built
//!   from it.
//! * [`failover`]: which account a thread at its limit can continue on.
//!
//! Nothing here refreshes a token or signs in: Orbit only reads what the
//! agents' own sign-in left on disk, so it can never log a CLI out.

pub mod burn;
pub mod claude;
pub mod failover;
pub mod ledger;

use orbit_proto::*;

/// One reading of an account's limits.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Reading {
    pub windows: Vec<QuotaWindow>,
    pub plan: Option<String>,
}

/// Why limits could not be read.
#[derive(Clone, Debug, PartialEq)]
pub struct ReadError {
    pub status: QuotaStatus,
    /// Seconds the provider asked to wait (`Retry-After`).
    pub retry_after: Option<u64>,
    /// Worth trying again soon (a network error, a timeout), as opposed to
    /// a state only the user can change (signed out, an API key).
    pub transient: bool,
}

impl ReadError {
    /// A state only the user can change.
    pub fn no_plan(message: impl Into<String>) -> Self {
        Self { status: QuotaStatus::NoPlan(message.into()), retry_after: None, transient: false }
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self { status: QuotaStatus::Unavailable(message.into()), retry_after: None, transient: false }
    }

    pub fn transient(message: impl Into<String>) -> Self {
        Self { status: QuotaStatus::Unavailable(message.into()), retry_after: None, transient: true }
    }
}

/// Whether a provider exposes plan limits Orbit can read on its own.
pub fn readable(kind: ProviderKind) -> bool {
    matches!(kind, ProviderKind::Claude | ProviderKind::Codex)
}
