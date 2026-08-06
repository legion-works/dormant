//! Linux-only [`ksni::Tray`] implementation for `dormant-tray`.
//!
//! The tray reads its state from a shared [`TrayState`] (updated by
//! [`crate::ipc_loop`]) and exposes a fresh [`ksni::Icon`] + menu on
//! every `ksni` refresh callback.  All the heavy lifting — state
//! derivation, menu construction, tooltip rendering, icon overlays —
//! lives in the platform-neutral modules; this file is the thin
//! glue that hands the data to `ksni` and dispatches menu actions.

use std::sync::{Arc, Mutex as StdMutex};

use dormant_core::rules::StateSnapshot;
use ksni::menu::{MenuItem, StandardItem, SubMenu};
use ksni::{Icon, Tray, TrayMethods};
use tokio::sync::Mutex;
use tracing::warn;

use crate::dispatch::{DispatchCapabilities, SystemCapabilities, execute_plan, plan_action};
use crate::icon::{IconSet, SIZES};
use crate::menu::{Action, MenuEntry};
use crate::state::IconState;
use crate::tooltip::{TooltipInputs, build_tooltip};
use crate::tray_state::TrayState;

/// The `ksni::Tray` impl.  Holds an `Arc<TrayState>` (cheap to clone for
/// the IPC loop) plus the pre-baked [`IconSet`].
pub struct DormantTray {
    /// Shared state the ksni callbacks read.
    pub state: Arc<Mutex<TrayState>>,
    /// Last complete state observed by a synchronous ksni callback.
    view_cache: StdMutex<TrayView>,
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
            view_cache: StdMutex::new(TrayView {
                snapshot: None,
                unreachable: true,
                icon_state: IconState::Unreachable,
            }),
            icons: IconSet::load(),
            web_port,
        }
    }

    /// Snapshot a view of the shared state for synchronous menu / icon
    /// building.  Returns the cached values without awaiting.
    fn view(&self) -> TrayView {
        // ksni invokes this callback synchronously inside the runtime, so it
        // cannot await the IPC mutex. Contention only means an IPC update is
        // mid-publication; reuse the last complete IPC-observed view rather
        // than treating it as a daemon connectivity change.
        let s = self.state.try_lock();
        match s {
            Ok(s) => {
                let view = TrayView {
                    snapshot: s.snapshot.clone(),
                    unreachable: s.unreachable,
                    icon_state: s.icon_state,
                };
                *self
                    .view_cache
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = view.clone();
                view
            }
            Err(_) => self
                .view_cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        }
    }
}

#[derive(Clone)]
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
            IconState::Failure => &self.icons.failure,
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
            IconState::Failure => "dormant-failure".into(),
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
            IconState::Failure => "dormant-failure",
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
            icon,
            action,
        } => {
            let mut item = action_item(action);
            item.label = label;
            item.enabled = enabled;
            item.icon_data = icon.png_bytes().to_vec();
            item.into()
        }
        MenuEntry::Info { label, icon } => {
            // Info lines are non-clickable status lines; ksni expresses
            // "always disabled" via the existing StandardItem with a
            // never-called activate closure.
            let mut item: StandardItem<DormantTray> = StandardItem {
                label,
                enabled: false,
                ..Default::default()
            };
            item.icon_data = icon.png_bytes().to_vec();
            item.into()
        }
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

/// Build a `StandardItem` whose `activate` closure dispatches the given action.
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
            let (socket, snapshot, unreachable) = {
                let current = state.lock().await;
                (
                    current.socket_path.clone(),
                    current.snapshot.clone(),
                    current.unreachable,
                )
            };
            let plan = plan_action(&action_clone, snapshot.as_ref(), unreachable);
            let capabilities: Arc<dyn DispatchCapabilities> =
                Arc::new(SystemCapabilities::new(Arc::new(crate::tray::request_quit)));
            if let Err(e) = execute_plan(plan, socket, capabilities).await {
                warn!(error = %e, "menu action dispatch failed");
            }
        });
    });
    item
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

#[cfg(test)]
mod tests {
    use super::*;
    use dormant_core::rules::DisplaySnapshot;

    fn disp(id: &str, phase: &str) -> (String, DisplaySnapshot) {
        (
            id.into(),
            DisplaySnapshot {
                phase: phase.into(),
                inhibited: false,
                paused: false,
                cmd_gen: 0,
                scope: dormant_core::config::DisplayScope::Private,
                owned: true,
                observed_input_code: None,
                panel_state: None,
                controllers: vec![],
                wake_attempts: 0,
                last_blank_failed: false,
                stage: None,
            },
        )
    }

    fn snap(displays: Vec<(String, DisplaySnapshot)>) -> StateSnapshot {
        StateSnapshot {
            sensors: vec![],
            zones: vec![],
            displays,
            pending_reload: None,
            rollback: None,
            kvm: None,
            wear_sampling_status: None,
        }
    }

    async fn reachable_tray() -> (DormantTray, Arc<Mutex<TrayState>>) {
        let state = Arc::new(Mutex::new(TrayState::new("/tmp/dormant.sock".into())));
        {
            let mut current = state.lock().await;
            current.snapshot = Some(snap(vec![disp("monitor", "active")]));
            current.unreachable = false;
            current.icon_state = IconState::Normal;
        }
        (DormantTray::new(state.clone(), 8137), state)
    }

    #[tokio::test]
    async fn menu_contention_keeps_cached_display_submenu_and_enabled_mutations() {
        let (tray, state) = reachable_tray().await;
        let _ = Tray::menu(&tray);

        let guard = state.lock().await;
        let menu = Tray::menu(&tray);
        drop(guard);

        let submenu = menu
            .iter()
            .find_map(|entry| match entry {
                MenuItem::SubMenu(submenu) if submenu.label == "● monitor — active" => {
                    Some(submenu)
                }
                _ => None,
            })
            .expect("cached monitor submenu");
        assert!(
            submenu.submenu.iter().any(|entry| {
                matches!(entry, MenuItem::Standard(item) if item.label == "Force blank now" && item.enabled)
            }),
            "cached submenu should enable Force blank now"
        );
        assert!(
            submenu.submenu.iter().any(|entry| {
                matches!(entry, MenuItem::Standard(item) if item.label == "Wake now" && item.enabled)
            }),
            "cached submenu should enable Wake now"
        );
    }

    #[tokio::test]
    async fn icon_name_contention_keeps_cached_icon_state() {
        let (tray, state) = reachable_tray().await;
        assert_eq!(Tray::icon_name(&tray), "dormant");

        let guard = state.lock().await;
        let icon_name = Tray::icon_name(&tray);
        drop(guard);

        assert_eq!(icon_name, "dormant");
    }

    #[tokio::test]
    async fn tooltip_contention_keeps_cached_display_details() {
        let (tray, state) = reachable_tray().await;
        let _ = Tray::tool_tip(&tray);

        let guard = state.lock().await;
        let tooltip = Tray::tool_tip(&tray);
        drop(guard);

        assert_eq!(tooltip.title, "dormant — 1 display");
        assert!(
            tooltip.description.contains("monitor: active"),
            "cached tooltip should retain display details: {tooltip:?}"
        );
    }

    #[tokio::test]
    async fn first_callback_under_contention_keeps_genuine_startup_unreachable_state() {
        let state = Arc::new(Mutex::new(TrayState::new("/tmp/dormant.sock".into())));
        let tray = DormantTray::new(state.clone(), 8137);

        let guard = state.lock().await;
        let menu = Tray::menu(&tray);
        drop(guard);

        assert!(
            !menu
                .iter()
                .any(|entry| matches!(entry, MenuItem::SubMenu(_))),
            "startup state should not invent displays"
        );
        assert!(
            menu.iter().any(|entry| {
                matches!(entry, MenuItem::Standard(item) if item.label == "Pause 30m" && !item.enabled)
            }),
            "startup unreachable state should disable mutations"
        );
    }

    // The unreachable end state matches the cache's initial value, so replacement is covered by contention tests instead.
    #[tokio::test]
    async fn uncontended_read_renders_a_real_unreachable_state() {
        let (tray, state) = reachable_tray().await;
        let _ = Tray::menu(&tray);
        {
            let mut current = state.lock().await;
            current.snapshot = None;
            current.unreachable = true;
            current.icon_state = IconState::Unreachable;
        }

        let menu = Tray::menu(&tray);

        assert!(
            !menu
                .iter()
                .any(|entry| matches!(entry, MenuItem::SubMenu(_))),
            "a real unreachable state should remove display submenus"
        );
        assert!(
            menu.iter().any(|entry| {
                matches!(entry, MenuItem::Standard(item) if item.label == "Force blank all…" && !item.enabled)
            }),
            "a real unreachable state should disable mutations"
        );
    }
}
