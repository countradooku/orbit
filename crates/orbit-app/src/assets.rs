//! Assets: GPUI Kit's default icons plus the Lucide icons Orbit uses.
//!
//! Only the listed icons are embedded (borrowed from static storage), not the
//! full 1,800-icon catalog, so the binary and memory stay small.

use std::borrow::Cow;

use gpui_kit::{AssetSource, Result, SharedString};

gpui_kit::assets::icon_assets!(pub OrbitIcons, [
    Send, Paperclip, Sparkles, MessageSquare, GitBranch, GitPullRequest, GitCommitHorizontal,
    Terminal, SquarePen, CircleStop, Clock, Hand, CircleDot, FileDiff, ListChecks, Brain, Wrench,
    ShieldCheck, ShieldAlert, Zap, Pencil, Command, Layers, Gauge, FolderPlus, Archive, Pin,
    GitFork, Hourglass, FilePen, FileSearch, Plug, ListTodo, RotateCcw, GitMerge, Orbit,
    CirclePause, Sparkle, AppWindow, SearchCode, Trash, Keyboard,
    // Shell: status glyphs, the deck, tiles, peek.
    CircleDashed, OctagonAlert, LayoutGrid, Reply, ScanSearch, OctagonX, BellRing, MessageCircleQuestionMark, CheckCheck,
    Columns2, Maximize2, Minimize2, SquareX, MousePointerClick, ListFilter, BellOff, Volume2,
    MessageSquarePlus,
    // Teams: mail between teammates.
    Mail,
    // Automations.
    CalendarClock, Package, Sunrise, CircleMinus,
    // Workspaces: files, run targets.
    FolderTree, FileCode, FileImage, FileSpreadsheet, Braces, ListCollapse, AtSign, FlaskConical,
]);

/// Orbit's icons first, then GPUI Kit's defaults.
pub struct OrbitAssets;

impl AssetSource for OrbitAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(bytes) = OrbitIcons.load(path)? {
            return Ok(Some(bytes));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut all = OrbitIcons.list(path)?;
        all.extend(gpui_kit::assets::Assets.list(path)?);
        Ok(all)
    }
}

/// The full Lucide catalog's names, for the icons registered above.
pub use gpui_kit::assets::IconName as Lucide;
