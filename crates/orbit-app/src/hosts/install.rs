//! Installing `orbitd` on an SSH or WSL host.
//!
//! The host's platform comes from the probe (`uname -sm`). An `orbitd` for
//! it can come from:
//!
//! * this computer's own, when both run the same platform;
//! * a build shipped beside Orbit, `remote/<target triple>/orbitd`;
//! * the release manifest the updater reads (`docs/RELEASING.md`): the
//!   platform's bundle is downloaded, checked against its checksum and
//!   signature, and `orbitd` taken out of it. Only builds with a release key
//!   download anything;
//! * a file the user picks ("bring your own binary").
//!
//! It is copied to `~/.orbit/bin/orbitd` there ([`orbit_net::remote::Remote::install`])
//! and checked by running it once; nothing else on the host changes.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use orbit_net::remote::{Platform, Probe, ProbeInfo, Remote, RemoteLink};

use crate::app_state::AppState;
use crate::assets::Lucide;
use crate::update::{download, install, manifest, verify};

/// Where an `orbitd` comes from.
#[derive(Clone, Debug)]
enum Source {
    File(PathBuf),
    Release { manifest_url: String, triple: String },
}

/// An `orbitd` for `platform` that is already on this computer: its own
/// (same platform), or one shipped beside Orbit.
pub fn local_build(platform: &Platform) -> Option<(PathBuf, &'static str)> {
    if *platform == Platform::current() && platform.os != "windows"
        && let Some(own) = orbit_net::host::locate_binary()
    {
        return Some((own, "Copy this computer’s orbitd"));
    }
    let triple = platform.triple()?;
    let exe = std::env::current_exe().ok()?;
    let bundled = exe.parent()?.join("remote").join(triple).join("orbitd");
    bundled.is_file().then_some((bundled, "Copy the orbitd shipped with Orbit"))
}

/// Downloads the release's bundle for `triple`, checks it and returns the
/// `orbitd` inside (blocking).
fn release_binary(manifest_url: &str, triple: &str, dir: &Path) -> Result<PathBuf, String> {
    let key = verify::release_key().ok_or("This build of Orbit has no release key, so it downloads nothing.")?;
    let manifest = manifest::parse(&download::fetch_text(manifest_url)?)?;
    let artifact = manifest
        .platforms
        .get(triple)
        .ok_or_else(|| format!("Orbit {} has no release for {triple}.", manifest.version))?;
    std::fs::create_dir_all(dir).map_err(|e| format!("Couldn’t save the download: {e}"))?;
    let archive = dir.join(format!("orbit-{}-{triple}.tar.gz", manifest.version));
    download::download(&artifact.url, &archive, artifact.size, &download::Progress::new())?;
    verify::verify_file(&archive, &artifact.sha256, &artifact.signature, &manifest.version, triple, &key)?;
    let unpacked = dir.join(format!("orbit-{}-{triple}", manifest.version));
    install::untar_gz(&archive, &unpacked)?;
    find_file(&unpacked, "orbitd", 3).ok_or_else(|| format!("The Orbit {} bundle for {triple} has no orbitd.", manifest.version))
}

fn find_file(dir: &Path, name: &str, depth: usize) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut dirs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() && path.file_name().is_some_and(|n| n == name) {
            return Some(path);
        }
        if path.is_dir() {
            dirs.push(path);
        }
    }
    (depth > 0).then(|| dirs.iter().find_map(|d| find_file(d, name, depth - 1))).flatten()
}

/// The install dialog's content.
pub struct InstallView {
    app: Entity<AppState>,
    link: Arc<RemoteLink>,
    platform: Option<Platform>,
    /// What the host has now, when it is another version.
    outdated: Option<ProbeInfo>,
    local: Option<(PathBuf, &'static str)>,
    /// The release manifest can be asked (a release build).
    can_download: bool,
    busy: Option<String>,
    error: Option<String>,
    _task: Option<Task<()>>,
}

/// Opens the install dialog for the shown host.
pub fn open(app: &Entity<AppState>, window: &mut Window, cx: &mut App) {
    let Some(link) = app.read(cx).link().cloned() else { return };
    let probe = match link.phase() {
        orbit_net::remote::LinkPhase::NeedsInstall(probe) => probe,
        orbit_net::remote::LinkPhase::Ready(info) => Probe::Outdated(info),
        _ => Probe::Missing { platform: None },
    };
    let platform = probe.platform();
    let outdated = match &probe {
        Probe::Outdated(info) => Some(info.clone()),
        _ => None,
    };
    let local = platform.as_ref().and_then(local_build);
    let view = cx.new(|_| InstallView {
        app: app.clone(),
        link: link.clone(),
        platform,
        outdated,
        local,
        can_download: verify::release_key().is_some(),
        busy: None,
        error: None,
        _task: None,
    });
    let title = format!("Install orbitd on {}", link.entry().label());
    window.open_dialog(cx, move |dialog, _, _| dialog.title(title.clone()).w(px(540.)).child(view.clone()));
}

impl InstallView {
    fn start(&mut self, source: Source, window: &mut Window, cx: &mut Context<Self>) {
        let label = self.link.entry().label();
        self.busy = Some(match &source {
            Source::Release { .. } => "Downloading and checking orbitd…".to_owned(),
            Source::File(_) => format!("Copying orbitd to {label}…"),
        });
        self.error = None;
        cx.notify();
        let link = self.link.clone();
        let downloads = self.app.read(cx).data_dir().join("updates").join("remote");
        let work = cx.background_spawn(async move {
            let path = match source {
                Source::File(path) => path,
                Source::Release { manifest_url, triple } => release_binary(&manifest_url, &triple, &downloads)?,
            };
            Remote::new(link.entry(), link.tools()).install(&path).map_err(|f| f.message)
        });
        self._task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = work.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = None;
                match result {
                    Ok(info) => {
                        this.link.installed(info.clone());
                        this.app.read(cx).retry();
                        window.close_dialog(cx);
                        let note = format!("Orbit {} is installed on {}. Connecting…", info.version, this.link.entry().label());
                        window.push_notification(gpui_kit::component::notification::Notification::success(note).autohide(true), cx);
                    }
                    Err(message) => this.error = Some(message),
                }
                cx.notify();
            });
        }));
    }

    fn choose_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Install".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else { return };
            let Some(path) = paths.into_iter().next() else { return };
            let _ = this.update_in(cx, |this, window, cx| this.start(Source::File(path), window, cx));
        })
        .detach();
    }
}

impl Render for InstallView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let label = self.link.entry().label();
        let platform = self.platform.as_ref().map(Platform::label);
        let triple = self.platform.as_ref().and_then(Platform::triple);
        let intro = match (&self.outdated, &platform) {
            (Some(info), _) => format!(
                "{label} has orbitd from Orbit {} (protocol {}); this Orbit needs protocol {}. Install this version’s in its place: \
                 agents running there finish first.",
                info.version,
                info.protocol,
                orbit_proto::PROTOCOL_VERSION
            ),
            (None, Some(platform)) => format!(
                "Orbit runs agents on {label} through orbitd, a small program that runs as you. {label} runs {platform}. \
                 It is copied to ~/.orbit/bin; nothing else there changes."
            ),
            (None, None) => format!(
                "Orbit runs agents on {label} through orbitd, a small program that runs as you. Orbit couldn’t tell which \
                 system {label} runs (uname didn’t answer), so choose an orbitd built for it."
            ),
        };
        let busy = self.busy.is_some();
        let mut options = v_flex().gap_2();
        if let Some((path, what)) = self.local.clone() {
            options = options.child(
                Button::new("install-local")
                    .primary()
                    .icon(Icon::new(Lucide::Laptop))
                    .label(what)
                    .disabled(busy)
                    .on_click(cx.listener(move |this, _, window, cx| this.start(Source::File(path.clone()), window, cx))),
            );
        }
        if self.can_download
            && let Some(triple) = triple.clone()
        {
            let manifest_url = crate::update::manifest_url(cx);
            options = options.child(
                Button::new("install-release")
                    .when(self.local.is_none(), |b| b.primary())
                    .when(self.local.is_some(), |b| b.outline())
                    .icon(Icon::new(Lucide::Download))
                    .label(format!("Download for {}", platform.clone().unwrap_or_default()))
                    .disabled(busy)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let source = Source::Release { manifest_url: manifest_url.clone(), triple: triple.clone() };
                        this.start(source, window, cx)
                    })),
            );
        }
        options = options.child(
            Button::new("install-file")
                .outline()
                .icon(Icon::new(IconName::FolderOpen))
                .label("Choose an orbitd file…")
                .disabled(busy)
                .on_click(cx.listener(|this, _, window, cx| this.choose_file(window, cx))),
        );
        let none_here = self.local.is_none() && !self.can_download;
        let build_hint = triple.map(|t| format!("cargo build --release -p orbitd --target {t}"));
        v_flex()
            .gap_3()
            .text_sm()
            .child(div().text_color(theme.muted_foreground).child(intro))
            .when(none_here, |el| {
                el.child(
                    v_flex()
                        .gap_1p5()
                        .p_3()
                        .rounded(theme.radius)
                        .bg(theme.muted)
                        .text_xs()
                        .child(format!(
                            "No orbitd for {} comes with this Orbit. Copy it from an Orbit for that system, or build it from \
                             Orbit’s source, then choose the file here.",
                            platform.clone().unwrap_or_else(|| "that system".into())
                        ))
                        .when_some(build_hint, |el, hint| {
                            el.child(
                                h_flex()
                                    .gap_2()
                                    .child(div().font_family(theme.mono_font_family.clone()).child(hint.clone()))
                                    .child(gpui_kit::component::clipboard::Clipboard::new("install-build-hint").value(hint)),
                            )
                        }),
                )
            })
            .child(options)
            .when_some(self.busy.clone(), |el, text| {
                el.child(h_flex().gap_2().text_color(theme.muted_foreground).child(Spinner::new().xsmall()).child(text))
            })
            .when_some(self.error.clone(), |el, text| el.child(div().text_color(theme.danger).child(text)))
    }
}
