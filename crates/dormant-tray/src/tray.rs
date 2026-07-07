//! Linux-only [`ksni::Tray`] implementation for `dormant-tray`.
//!
//! The tray reads its state from a shared [`TrayState`] (updated by
//! [`crate::ipc_loop`]) and exposes a fresh [`ksni::Icon`] + menu on
//! every `ksni` refresh callback.  All the heavy lifting — state
//! derivation, menu construction, tooltip rendering, icon overlays —
//! lives in the platform-neutral modules; this file is the thin
//! glue that hands the data to `ksni` and dispatches menu actions.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use dormant_core::ipc_proto::IpcRequest;
use dormant_core::rules::StateSnapshot;
use dormantctl::client;
use ksni::menu::{MenuItem, StandardItem, SubMenu};
use ksni::{Icon, Tray, TrayMethods};
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::icon::{IconSet, SIZES};
use crate::menu::{Action, MenuEntry};
use crate::state::IconState;
use crate::tooltip::{TooltipInputs, build_tooltip};

/// Shared state between the IPC loop and the `ksni::Tray` impl.
///
/// `ksni` calls `&self` methods (`icon_pixmap`, `menu`, `title`, …) from
/// its D-Bus thread, while the IPC loop runs on a tokio task and wants
/// `async` access.  A `tokio::sync::Mutex` is the simplest meeting point:
/// the ksni side grabs a synchronous lock via `try_lock()` (no await,
/// never blocks the D-Bus thread for long), and the IPC side awaits.
pub struct TrayState {
    /// Path to the daemon's Unix socket.
    pub socket_path: PathBuf,
    /// Latest snapshot from the daemon (or `None` until the first Status).
    pub snapshot: Option<StateSnapshot>,
    /// Whether the IPC loop currently reports the daemon as unreachable.
    pub unreachable: bool,
    /// The current icon state derived from `snapshot` / `unreachable`.
    pub icon_state: IconState,
}

impl TrayState {
    /// Create a fresh state with the given socket path; everything else
    /// starts as "starting up" / unreachable until the IPC loop lands.
    #[must_use]
    pub fn new(socket_path: PathBuf) -> Self {
        Self {
            socket_path,
            snapshot: None,
            unreachable: true,
            icon_state: IconState::Unreachable,
        }
    }
}

/// The `ksni::Tray` impl.  Holds an `Arc<TrayState>` (cheap to clone for
/// the IPC loop) plus the pre-baked [`IconSet`].
pub struct DormantTray {
    /// Shared state the ksni callbacks read.
    pub state: Arc<Mutex<TrayState>>,
    /// Pre-baked pixmaps at every tray size × state variant.
    pub icons: IconSet,
    /// Port for the "Open web UI" menu entry.
    pub web_port: u16,
}

impl DormantTray {
    /// Build a new tray instance from its three pieces.
    pub fn new(state: Arc<Mutex<TrayState>>, web_port: u16) -> Self {
        Self {
            state,
            icons: IconSet::load(),
            web_port,
        }
    }

    /// Snapshot a view of the shared state for synchronous menu / icon
    /// building.  Returns the cached values without awaiting.
    fn view(&self) -> TrayView {
        // `try_lock` keeps the ksni callback off the await path; on
        // contention (the IPC loop is mid-write) we return whatever was
        // there at the start of the callback — visually a no-op refresh.
        let s = self.state.try_lock();
        match s {
            Ok(s) => TrayView {
                snapshot: s.snapshot.clone(),
                unreachable: s.unreachable,
                icon_state: s.icon_state,
            },
            Err(_) => TrayView {
                snapshot: None,
                unreachable: true,
                icon_state: IconState::Unreachable,
            },
        }
    }
}

struct TrayView {
    snapshot: Option<StateSnapshot>,
    unreachable: bool,
    icon_state: IconState,
}

impl Tray for DormantTray {
    fn id(&self) -> String {
        // Stable identifier the compositor keys on.  Distinct from the
        // process name so a daemon restart doesn't drag the icon around.
        "dormant-tray".into()
    }

    fn title(&self) -> String {
        // Shown in the accessibility tree / tooltip header.
        "dormant".into()
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        let view = self.view();
        let blobs: &[(u32, Vec<u8>)] = match view.icon_state {
            IconState::Paused => &self.icons.paused,
            IconState::Unreachable => &self.icons.unreachable,
            // Normal and Attention share the base blob (the mark IS the
            // dormant green, so "Attention" reads as "this is the brand"
            // rather than a different colour).
            IconState::Normal | IconState::Attention => &self.icons.base,
        };

        blobs
            .iter()
            .map(|(size, data)| Icon {
                width: size.cast_signed(),
                height: size.cast_signed(),
                data: data.clone(),
            })
            .collect()
    }

    fn icon_name(&self) -> String {
        // Fallback theme name the compositor can pick when our pixmaps
        // are filtered out.  We always supply pixmaps; this is the safety
        // net.
        let view = self.view();
        match view.icon_state {
            IconState::Normal | IconState::Attention => "dormant".into(),
            IconState::Paused => "dormant-paused".into(),
            IconState::Unreachable => "dormant-unreachable".into(),
        }
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        let view = self.view();
        let tt = build_tooltip(&TooltipInputs {
            snapshot: view.snapshot.as_ref(),
            unreachable: view.unreachable,
        });
        let icon = match view.icon_state {
            IconState::Normal | IconState::Attention => "dormant",
            IconState::Paused => "dormant-paused",
            IconState::Unreachable => "dormant-unreachable",
        };
        ksni::ToolTip {
            icon_name: icon.into(),
            icon_pixmap: self.icon_pixmap(),
            title: tt.title,
            description: tt.body,
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let view = self.view();
        let entries =
            crate::menu::build_menu(view.snapshot.as_ref(), view.unreachable, self.web_port);
        entries.into_iter().map(menu_entry_to_ksni).collect()
    }
}

/// Convert one of our [`MenuEntry`] values into a ksni [`MenuItem`].
///
/// `Arc<TrayState>` is shared into every closure so the action handlers
/// can dispatch through the IPC client (synchronous `send_request` is
/// safe inside the ksni D-Bus thread).
fn menu_entry_to_ksni(entry: MenuEntry) -> MenuItem<DormantTray> {
    match entry {
        MenuEntry::Separator => MenuItem::Separator,
        MenuEntry::Action {
            label,
            enabled,
            action,
        } => StandardItem {
            label,
            enabled,
            ..action_item(action)
        }
        .into(),
        MenuEntry::Submenu {
            label,
            enabled: _,
            entries,
        } => SubMenu {
            label,
            enabled: true, // submenu itself stays openable; children carry enabled state
            submenu: entries.into_iter().map(menu_entry_to_ksni).collect(),
            ..Default::default()
        }
        .into(),
    }
}

/// Build a `StandardItem` whose `activate` closure dispatches the given
/// action through the shared IPC client.
fn action_item(action: Action) -> StandardItem<DormantTray> {
    let mut item: StandardItem<DormantTray> = StandardItem {
        label: String::new(),
        ..Default::default()
    };
    item.activate = Box::new(move |tray: &mut DormantTray| {
        let state = tray.state.clone();
        // ksni's activate callback is synchronous; spin up a one-shot
        // tokio task via the current runtime handle to do the IPC work
        // without blocking the D-Bus thread for long.
        let rt = match tokio::runtime::Handle::try_current() {
            Ok(h) => h,
            Err(e) => {
                warn!(error = %e, "no tokio runtime in activate; dropping action");
                return;
            }
        };
        let action_clone = action.clone();
        rt.spawn(async move {
            if let Err(e) = dispatch(&state, action_clone).await {
                warn!(error = %e, "menu action dispatch failed");
            }
        });
    });
    item
}

/// Dispatch a menu action through the IPC client.
async fn dispatch(state: &Arc<Mutex<TrayState>>, action: Action) -> Result<()> {
    let (socket, paused_active) = {
        let s = state.lock().await;
        (s.socket_path.clone(), s.unreachable)
    };
    if paused_active && !matches!(action, Action::OpenWebUi { .. } | Action::Quit) {
        // Defensive — the menu model already disables these in the UI;
        // the runtime check is a belt-and-braces guard.
        return Ok(());
    }

    let req = match action {
        Action::Pause(dur) => IpcRequest::Pause {
            rule: None,
            duration_s: dur.map(|d| d.as_secs()),
        },
        Action::Resume => IpcRequest::Resume { rule: None },
        Action::BlankAll => {
            // Fan out Blank to every display the snapshot currently knows
            // about.  Each call is independent.
            let displays: Vec<String> = {
                let s = state.lock().await;
                s.snapshot.as_ref().map_or(vec![], |snap| {
                    snap.displays.iter().map(|(id, _)| id.clone()).collect()
                })
            };
            for id in displays {
                send_one(&socket, &IpcRequest::Blank { display: id }).await?;
            }
            return Ok(());
        }
        Action::WakeAll => {
            let displays: Vec<String> = {
                let s = state.lock().await;
                s.snapshot.as_ref().map_or(vec![], |snap| {
                    snap.displays.iter().map(|(id, _)| id.clone()).collect()
                })
            };
            for id in displays {
                send_one(&socket, &IpcRequest::Wake { display: id }).await?;
            }
            return Ok(());
        }
        Action::BlankOne(id) => IpcRequest::Blank { display: id },
        Action::WakeOne(id) => IpcRequest::Wake { display: id },
        Action::OpenWebUi { port } => {
            open_web_ui(port);
            return Ok(());
        }
        Action::Quit => {
            // The ksni Handle lives in main; we surface a quit request
            // by cancelling the runtime's shutdown token.  We can't get
            // to it from here, so main installs a oneshot receiver.
            // (See main.rs for the wire-up.)
            crate::tray::request_quit();
            return Ok(());
        }
        Action::Separator => return Ok(()),
    };
    send_one(&socket, &req).await
}

async fn send_one(socket: &Path, req: &IpcRequest) -> Result<()> {
    let socket = socket.to_path_buf();
    let req = req.clone();
    let resp = tokio::task::spawn_blocking(move || client::send_request(&socket, &req)).await??;
    if !resp.ok {
        anyhow::bail!(
            "daemon returned error: {}",
            resp.error.as_deref().unwrap_or("unknown")
        );
    }
    Ok(())
}

fn open_web_ui(port: u16) {
    let url = format!("http://127.0.0.1:{port}");
    // Best-effort — `xdg-open` may not be installed everywhere; failures
    // are logged at WARN and not propagated back to the menu.
    match std::process::Command::new("xdg-open").arg(&url).spawn() {
        Ok(_) => info!(%url, "opened web UI"),
        Err(e) => warn!(error = %e, %url, "xdg-open failed"),
    }
}

// ── Quit signalling ──────────────────────────────────────────────────────────

use std::sync::OnceLock;
use tokio::sync::Notify;

static QUIT_SIGNAL: OnceLock<Notify> = OnceLock::new();

fn quit_signal() -> &'static Notify {
    QUIT_SIGNAL.get_or_init(Notify::new)
}

/// Notify the binary's main loop that the user picked Quit.
pub fn request_quit() {
    quit_signal().notify_waiters();
}

/// Await the next quit request.  Awakenable on every Quit click.
pub async fn wait_for_quit() {
    quit_signal().notified().await;
}

// ── Spawning the tray ────────────────────────────────────────────────────────

/// Spawn the ksni tray and return its handle.
///
/// `state` must already be wrapped in `Arc<Mutex<...>>` and shared with
/// the IPC loop (which updates it as snapshots arrive).
///
/// Must be called from inside a tokio runtime — `ksni::spawn` is async.
///
/// # Panics
///
/// Panics if the underlying `ksni` D-Bus connection cannot be
/// established (no running compositor session).
pub async fn spawn(state: Arc<Mutex<TrayState>>, web_port: u16) -> ksni::Handle<DormantTray> {
    let tray = DormantTray::new(state, web_port);
    // Sanity: we expect the icon set to have an entry for every size we
    // baked.  If this ever trips, the build script and runtime have
    // drifted and the operator will see no icon.
    debug_assert_eq!(
        SIZES.len(),
        tray.icons.base.len(),
        "icon set size mismatch — check build.rs SIZES constant"
    );
    tray.spawn().await.expect("ksni spawn failed")
}
