#![cfg(target_os = "macos")]

//! `AppKit` status-item backend for the macOS tray frontend.

use std::cell::RefCell;

use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol};
use objc2::{AllocAnyThread, DeclaredClass, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSImage, NSMenu, NSMenuItem, NSStatusBar, NSStatusItem, NSVariableStatusItemLength,
};
use objc2_foundation::{NSData, NSSize, NSString};

use crate::DEFAULT_WEB_PORT;
use crate::action_table::{ActionTable, TaggedMenuEntry};
use crate::menu::{self, Action};
use crate::template_icon;
use crate::tooltip::{self, TooltipInputs};
use crate::tray_state::TrayState;

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
