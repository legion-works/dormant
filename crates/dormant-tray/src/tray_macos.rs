#![cfg(target_os = "macos")]

//! `AppKit` status-item backend for the macOS tray frontend.

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};

use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol};
use objc2::{AllocAnyThread, DeclaredClass, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSEvent, NSEventModifierFlags, NSEventType,
    NSImage, NSMenu, NSMenuItem, NSStatusBar, NSStatusItem, NSVariableStatusItemLength,
};
use objc2_foundation::{NSData, NSPoint, NSSize, NSString};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::DEFAULT_WEB_PORT;
use crate::action_table::{ActionTable, TaggedMenuEntry};
use crate::dispatch::{self, SystemCapabilities};
use crate::ipc_loop;
use crate::menu::{self, Action};
use crate::template_icon;
use crate::tooltip::{self, TooltipInputs};
use crate::tray_state::TrayState;

enum MainMessage {
    Refresh,
    Quit,
}

struct MacUiState {
    mac_tray: MacTray,
    state: Arc<Mutex<TrayState>>,
}

struct MainThreadState {
    receiver: Receiver<MainMessage>,
    ui: MacUiState,
    cancel: CancellationToken,
}

thread_local! {
    static MAIN_STATE: RefCell<Option<MainThreadState>> = const { RefCell::new(None) };
}

fn post_main_message(sender: &Sender<MainMessage>, message: MainMessage) {
    let _ = sender.send(message);
    dispatch2::DispatchQueue::main().exec_async(drain_mailbox);
}

fn drain_mailbox() {
    loop {
        let next = MAIN_STATE.with(|slot| {
            let slot = slot.borrow();
            slot.as_ref().map(|state| state.receiver.try_recv())
        });
        match next {
            Some(Ok(MainMessage::Refresh)) => refresh_main_state(),
            Some(Ok(MainMessage::Quit)) => {
                quit_main_state();
                break;
            }
            Some(Err(TryRecvError::Empty | TryRecvError::Disconnected)) | None => break,
        }
    }
}

fn refresh_main_state() {
    let snapshot = MAIN_STATE.with(|slot| {
        let slot = slot.borrow();
        slot.as_ref()
            .map(|state| state.ui.state.try_lock().map(|guard| guard.clone()))
    });

    let Some(snapshot) = snapshot else {
        return;
    };
    let Ok(snapshot) = snapshot else {
        tracing::debug!("tray state busy while refreshing macOS UI");
        return;
    };

    let mtm = MainThreadMarker::new().expect("macOS refresh must run on the main thread");
    MAIN_STATE.with(|slot| {
        if let Some(state) = slot.borrow_mut().as_mut() {
            state.ui.mac_tray.refresh(mtm, &snapshot);
        }
    });
}

fn quit_main_state() {
    let mtm = MainThreadMarker::new().expect("macOS quit must run on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.stop(None);

    MAIN_STATE.with(|slot| {
        if let Some(state) = slot.borrow().as_ref() {
            state.cancel.cancel();
        }
    });

    let event = NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
        NSEventType::ApplicationDefined,
        NSPoint::new(0.0, 0.0),
        NSEventModifierFlags::empty(),
        0.0,
        0,
        None,
        0_i16,
        0,
        0,
    )
    .expect("application-defined wake event");
    app.postEvent_atStart(&event, true);
}

/// Run the macOS status item and its Tokio-backed IPC runtime.
///
/// # Errors
///
/// Returns an error if the Tokio runtime cannot be built, its thread cannot
/// be spawned, or that thread panics.
///
/// # Panics
///
/// Panics if called without owning the process main thread or if `AppKit`
/// cannot construct its application-defined wake event.
pub fn run() -> anyhow::Result<()> {
    let mtm = MainThreadMarker::new().expect("dormant-tray must run on the macOS main thread");
    let app = NSApplication::sharedApplication(mtm);
    let _ = app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let socket_path = dormant_core::paths::resolve_socket_path(None);
    let state = Arc::new(Mutex::new(TrayState::new(socket_path.clone())));
    let (refresh_tx, mut refresh_rx) = ipc_loop::refresh_channel();
    let cancel = CancellationToken::new();
    let (action_tx, mut action_rx) = tokio::sync::mpsc::unbounded_channel::<Action>();
    let (main_tx, receiver) = std::sync::mpsc::channel::<MainMessage>();
    let mac_tray = MacTray::new(mtm, action_tx.clone());
    let refresh_main_tx = main_tx.clone();
    let quit_main_tx = main_tx.clone();

    MAIN_STATE.with(|slot| {
        *slot.borrow_mut() = Some(MainThreadState {
            receiver,
            ui: MacUiState {
                mac_tray,
                state: state.clone(),
            },
            cancel: cancel.clone(),
        });
    });

    let capabilities: Arc<dyn dispatch::DispatchCapabilities> =
        Arc::new(SystemCapabilities::new(Arc::new(move || {
            post_main_message(&quit_main_tx, MainMessage::Quit);
        })));
    let runtime_socket = socket_path.clone();
    let runtime_state = state.clone();
    let runtime_cancel = cancel.clone();
    let runtime_capabilities = capabilities.clone();
    let runtime = std::thread::Builder::new()
        .name("dormant-tray-rt".into())
        .spawn(move || -> anyhow::Result<()> {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            rt.block_on(async move {
                let ipc_task = tokio::spawn(ipc_loop::run(
                    runtime_socket.clone(),
                    runtime_state.clone(),
                    runtime_cancel.clone(),
                    refresh_tx.clone(),
                ));
                let refresh_task = tokio::spawn(async move {
                    post_main_message(&refresh_main_tx, MainMessage::Refresh);
                    loop {
                        tokio::select! {
                            () = runtime_cancel.cancelled() => break,
                            changed = refresh_rx.changed() => {
                                if changed.is_err() {
                                    break;
                                }
                                post_main_message(&refresh_main_tx, MainMessage::Refresh);
                            }
                        }
                    }
                });
                let action_task = tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            () = cancel.cancelled() => break,
                            action = action_rx.recv() => {
                                let Some(action) = action else {
                                    break;
                                };
                                let (snapshot, unreachable) = {
                                    let state = state.lock().await;
                                    (state.snapshot.clone(), state.unreachable)
                                };
                                let plan = dispatch::plan_action(&action, snapshot.as_ref(), unreachable);
                                if let Err(error) = dispatch::execute_plan(
                                    plan,
                                    socket_path.clone(),
                                    runtime_capabilities.clone(),
                                ).await {
                                    tracing::warn!(%error, "tray action failed");
                                }
                            }
                        }
                    }
                });
                let (ipc_result, refresh_result, action_result) =
                    tokio::join!(ipc_task, refresh_task, action_task);
                ipc_result.map_err(|error| anyhow::anyhow!("IPC task panicked: {error}"))?;
                refresh_result.map_err(|error| anyhow::anyhow!("refresh task panicked: {error}"))?;
                action_result.map_err(|error| anyhow::anyhow!("action task panicked: {error}"))?;
                Ok(())
            })
        })?;

    app.run();
    let runtime_result = runtime
        .join()
        .map_err(|_| anyhow::anyhow!("dormant-tray runtime thread panicked"));
    MAIN_STATE.with(|slot| drop(slot.borrow_mut().take()));
    runtime_result??;
    Ok(())
}

struct MenuTargetIvars {
    actions: RefCell<ActionTable>,
    action_tx: tokio::sync::mpsc::UnboundedSender<Action>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = MenuTargetIvars]
    struct MenuTarget;

    impl MenuTarget {
        #[unsafe(method(performAction:))]
        fn perform_action(&self, sender: &NSMenuItem) {
            let tag = sender.tag();
            if let Some(action) = self.ivars().actions.borrow().resolve(tag) {
                let _ = self.ivars().action_tx.send(action);
            }
        }
    }

    unsafe impl NSObjectProtocol for MenuTarget {}
);

impl MenuTarget {
    fn new(
        mtm: MainThreadMarker,
        actions: ActionTable,
        action_tx: tokio::sync::mpsc::UnboundedSender<Action>,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(MenuTargetIvars {
            actions: RefCell::new(actions),
            action_tx,
        });
        unsafe { msg_send![super(this), init] }
    }
}

/// Retained `AppKit` state backing the macOS status item.
pub struct MacTray {
    status_item: Retained<NSStatusItem>,
    menu: Retained<NSMenu>,
    menu_target: Retained<MenuTarget>,
    image: Retained<NSImage>,
}

impl MacTray {
    /// Create an empty status item and its retained target/action receiver.
    #[must_use]
    pub fn new(
        mtm: MainThreadMarker,
        action_tx: tokio::sync::mpsc::UnboundedSender<Action>,
    ) -> Self {
        let status_item =
            NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
        let menu = NSMenu::new(mtm);
        let image = NSImage::new();
        status_item.setMenu(Some(&menu));

        Self {
            status_item,
            menu,
            menu_target: MenuTarget::new(mtm, ActionTable::default(), action_tx),
            image,
        }
    }

    /// Rebuild the retained `NSMenu`, template image, and tooltip from tray state.
    ///
    /// # Panics
    ///
    /// Panics if the status item has no button or if the template PNG fails
    /// to decode into an `NSImage`.
    pub fn refresh(&mut self, mtm: MainThreadMarker, state: &TrayState) {
        let entries =
            menu::build_menu(state.snapshot.as_ref(), state.unreachable, DEFAULT_WEB_PORT);
        let tagged = self
            .menu_target
            .ivars()
            .actions
            .borrow_mut()
            .replace_from_menu(&entries);
        let menu = build_menu(mtm, &tagged, &self.menu_target);

        self.status_item.setMenu(Some(&menu));
        self.menu = menu;

        let pixels = template_icon::render_snapshot(state.snapshot.as_ref(), state.unreachable);
        let png = encode_png(pixels.width, pixels.height, &pixels.rgba);
        let data = NSData::with_bytes(&png);
        let image = NSImage::initWithData(NSImage::alloc(), &data)
            .expect("template icon PNG must decode as an NSImage");
        image.setTemplate(true);
        image.setSize(NSSize::new(18.0, 18.0));
        let button = self
            .status_item
            .button(mtm)
            .expect("status item must provide a button");
        button.setImage(Some(&image));
        self.image = image;

        let tooltip = tooltip::build_tooltip(&TooltipInputs {
            snapshot: state.snapshot.as_ref(),
            unreachable: state.unreachable,
        });
        let text = [tooltip.title, tooltip.body]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        let tooltip = NSString::from_str(&text);
        button.setToolTip(Some(&tooltip));
    }
}

fn build_menu(
    mtm: MainThreadMarker,
    entries: &[TaggedMenuEntry],
    target: &MenuTarget,
) -> Retained<NSMenu> {
    let menu = NSMenu::new(mtm);
    for entry in entries {
        let item = build_menu_item(mtm, entry, target);
        menu.addItem(&item);
    }
    menu
}

fn build_menu_item(
    mtm: MainThreadMarker,
    entry: &TaggedMenuEntry,
    target: &MenuTarget,
) -> Retained<NSMenuItem> {
    match entry {
        TaggedMenuEntry::Action {
            label,
            enabled,
            tag,
            ..
        } => {
            let title = NSString::from_str(label);
            let key_equivalent = NSString::from_str("");
            let item = unsafe {
                NSMenuItem::initWithTitle_action_keyEquivalent(
                    NSMenuItem::alloc(mtm),
                    &title,
                    Some(sel!(performAction:)),
                    &key_equivalent,
                )
            };
            unsafe {
                item.setEnabled(*enabled);
                item.setTag(*tag);
                item.setTarget(Some(target));
                item.setAction(Some(sel!(performAction:)));
            }
            item
        }
        TaggedMenuEntry::Separator => NSMenuItem::separatorItem(mtm),
        TaggedMenuEntry::Submenu {
            label,
            enabled,
            entries,
        } => {
            let title = NSString::from_str(label);
            let key_equivalent = NSString::from_str("");
            let item = unsafe {
                NSMenuItem::initWithTitle_action_keyEquivalent(
                    NSMenuItem::alloc(mtm),
                    &title,
                    None,
                    &key_equivalent,
                )
            };
            let submenu = build_menu(mtm, entries, target);
            item.setEnabled(*enabled);
            item.setSubmenu(Some(&submenu));
            item
        }
        TaggedMenuEntry::Info { label, .. } => {
            let title = NSString::from_str(label);
            let key_equivalent = NSString::from_str("");
            let item = unsafe {
                NSMenuItem::initWithTitle_action_keyEquivalent(
                    NSMenuItem::alloc(mtm),
                    &title,
                    None,
                    &key_equivalent,
                )
            };
            item.setEnabled(false);
            item
        }
    }
}

fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    let mut png = Vec::new();
    let mut encoder = png::Encoder::new(&mut png, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .expect("template icon PNG header must encode");
    writer
        .write_image_data(rgba)
        .expect("template icon PNG pixels must encode");
    drop(writer);
    png
}
