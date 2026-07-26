#![cfg(target_os = "macos")]

//! macOS global hotkeys through Carbon's permission-free hotkey API.

use std::cell::RefCell;
use std::ffi::c_void;
use std::mem::{MaybeUninit, size_of};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;

use tokio::sync::mpsc::UnboundedSender;

use crate::hotkey::{HotkeyError, ResolvedHotkeyStatus};
use crate::hotkey_macos_common::{
    CLAIM_HOTKEY_ID, HOTKEY_SIGNATURE, carbon_accelerator, carbon_event_action,
};
use crate::menu::Action;

type OSStatus = i32;
type EventTargetRef = *mut c_void;
type EventRef = *mut c_void;
type EventHandlerCallRef = *mut c_void;
type EventHandlerRef = *mut c_void;
type EventHotKeyRef = *mut c_void;

const NO_ERR: OSStatus = 0;
const EVENT_NOT_HANDLED_ERR: OSStatus = -9874;
const EVENT_CLASS_KEYBOARD: u32 = u32::from_be_bytes(*b"keyb");
const EVENT_HOT_KEY_PRESSED: u32 = 6;
const EVENT_PARAM_DIRECT_OBJECT: u32 = u32::from_be_bytes(*b"----");
const TYPE_EVENT_HOT_KEY_ID: u32 = u32::from_be_bytes(*b"hkid");

#[repr(C)]
struct EventTypeSpec {
    event_class: u32,
    event_kind: u32,
}

/// Carbon declares this as two consecutive `OSType`/`UInt32` fields.
#[repr(C)]
struct EventHotKeyId {
    signature: u32,
    id: u32,
}

const _: () = assert!(size_of::<EventTypeSpec>() == 8);
const _: () = assert!(size_of::<EventHotKeyId>() == 8);

type EventHandler = unsafe extern "C" fn(EventHandlerCallRef, EventRef, *mut c_void) -> OSStatus;

// These declarations mirror CarbonEvents.h. Keeping the raw surface here prevents
// Carbon types from leaking into the cross-platform lifecycle manager.
#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    #[link_name = "GetApplicationEventTarget"]
    fn get_application_event_target() -> EventTargetRef;
    #[link_name = "InstallEventHandler"]
    fn install_event_handler(
        target: EventTargetRef,
        handler: EventHandler,
        event_type_count: u32,
        event_types: *const EventTypeSpec,
        user_data: *mut c_void,
        out_handler: *mut EventHandlerRef,
    ) -> OSStatus;
    #[link_name = "RemoveEventHandler"]
    fn remove_event_handler(handler: EventHandlerRef) -> OSStatus;
    #[link_name = "RegisterEventHotKey"]
    fn register_event_hot_key(
        key_code: u32,
        modifiers: u32,
        id: EventHotKeyId,
        target: EventTargetRef,
        options: u32,
        out_hotkey: *mut EventHotKeyRef,
    ) -> OSStatus;
    #[link_name = "UnregisterEventHotKey"]
    fn unregister_event_hot_key(hotkey: EventHotKeyRef) -> OSStatus;
    #[link_name = "GetEventParameter"]
    fn get_event_parameter(
        event: EventRef,
        name: u32,
        desired_type: u32,
        out_actual_type: *mut u32,
        buffer_size: u32,
        out_actual_size: *mut u32,
        out_data: *mut c_void,
    ) -> OSStatus;
}

struct CallbackState {
    action_tx: UnboundedSender<Action>,
    registration: RefCell<Option<String>>,
}

/// Main-thread-owned Carbon registration and callback lifetime.
pub(crate) struct CarbonHotkeyRegistrar {
    callback: Option<Box<CallbackState>>,
    event_target: EventTargetRef,
    handler_ref: EventHandlerRef,
    hotkey_ref: Option<EventHotKeyRef>,
}

impl CarbonHotkeyRegistrar {
    pub(crate) fn new(action_tx: UnboundedSender<Action>) -> Result<Self, HotkeyError> {
        let mut callback = Box::new(CallbackState {
            action_tx,
            registration: RefCell::new(None),
        });
        let target = unsafe { get_application_event_target() };
        if target.is_null() {
            return Err(HotkeyError::CarbonError {
                operation: "GetApplicationEventTarget",
                status: EVENT_NOT_HANDLED_ERR,
            });
        }
        let event_type = EventTypeSpec {
            event_class: EVENT_CLASS_KEYBOARD,
            event_kind: EVENT_HOT_KEY_PRESSED,
        };
        let mut handler_ref = ptr::null_mut();
        let status = unsafe {
            install_event_handler(
                target,
                carbon_hotkey_handler,
                1,
                &raw const event_type,
                (&raw mut *callback).cast(),
                &raw mut handler_ref,
            )
        };
        if status != NO_ERR || handler_ref.is_null() {
            return Err(HotkeyError::CarbonError {
                operation: "InstallEventHandler",
                status: if status == NO_ERR {
                    EVENT_NOT_HANDLED_ERR
                } else {
                    status
                },
            });
        }
        Ok(Self {
            callback: Some(callback),
            event_target: target,
            handler_ref,
            hotkey_ref: None,
        })
    }

    pub(crate) fn replace(&mut self, status: ResolvedHotkeyStatus) {
        if let Err(error) = self.unregister_claim() {
            tracing::warn!(
                %error,
                event = "hotkey_register_failed",
                reason = "unregister_failed",
                "old Carbon hotkey could not be removed; replacement was not registered"
            );
            return;
        }

        let (accelerator, target) = match status {
            ResolvedHotkeyStatus::Disabled => return,
            ResolvedHotkeyStatus::Ambiguous { count } => {
                tracing::warn!(
                    count,
                    event = "hotkey_register_failed",
                    reason = "ambiguous_target",
                    "switch hotkey requires exactly one switch-capable shared display"
                );
                return;
            }
            ResolvedHotkeyStatus::Register {
                accelerator,
                target,
            } => (accelerator, target),
        };

        let accelerator = match carbon_accelerator(&accelerator) {
            Ok(accelerator) => accelerator,
            Err(error) => {
                tracing::warn!(
                    %error,
                    event = "hotkey_register_failed",
                    reason = "invalid_accelerator",
                    "Carbon hotkey conversion failed; manual menu path remains available"
                );
                return;
            }
        };
        let mut hotkey_ref = ptr::null_mut();
        let status = unsafe {
            register_event_hot_key(
                accelerator.key_code,
                accelerator.modifiers,
                EventHotKeyId {
                    signature: HOTKEY_SIGNATURE,
                    id: CLAIM_HOTKEY_ID,
                },
                self.event_target,
                0,
                &raw mut hotkey_ref,
            )
        };
        if status != NO_ERR || hotkey_ref.is_null() {
            let error = HotkeyError::CarbonError {
                operation: "RegisterEventHotKey",
                status: if status == NO_ERR {
                    EVENT_NOT_HANDLED_ERR
                } else {
                    status
                },
            };
            tracing::warn!(
                %error,
                event = "hotkey_register_failed",
                reason = "carbon_error",
                "Carbon hotkey registration failed; manual menu path remains available"
            );
            return;
        }

        *self
            .callback
            .as_ref()
            .expect("callback exists until handler removal")
            .registration
            .borrow_mut() = Some(target.clone());
        self.hotkey_ref = Some(hotkey_ref);
        tracing::info!(%target, "switch hotkey registered");
    }

    pub(crate) fn unregister(&mut self) {
        if let Err(error) = self.unregister_claim() {
            tracing::warn!(%error, event = "hotkey_unregister_failed");
        }
    }

    fn unregister_claim(&mut self) -> Result<(), HotkeyError> {
        *self
            .callback
            .as_ref()
            .expect("callback exists until handler removal")
            .registration
            .borrow_mut() = None;
        let Some(hotkey_ref) = self.hotkey_ref.take() else {
            return Ok(());
        };
        let status = unsafe { unregister_event_hot_key(hotkey_ref) };
        if status == NO_ERR {
            Ok(())
        } else {
            self.hotkey_ref = Some(hotkey_ref);
            Err(HotkeyError::CarbonError {
                operation: "UnregisterEventHotKey",
                status,
            })
        }
    }
}

impl Drop for CarbonHotkeyRegistrar {
    fn drop(&mut self) {
        self.unregister();
        let status = unsafe { remove_event_handler(self.handler_ref) };
        if status != NO_ERR {
            tracing::warn!(status, event = "hotkey_handler_remove_failed");
            // Carbon may still invoke the registered callback after a failed removal.
            // Leaking a shutdown-only allocation is safer than leaving dangling user data.
            if let Some(callback) = self.callback.take() {
                let _ = Box::leak(callback);
            }
        }
    }
}

unsafe extern "C" fn carbon_hotkey_handler(
    _next_handler: EventHandlerCallRef,
    event: EventRef,
    user_data: *mut c_void,
) -> OSStatus {
    catch_unwind(AssertUnwindSafe(|| unsafe {
        dispatch_carbon_event(event, user_data)
    }))
    .unwrap_or(EVENT_NOT_HANDLED_ERR)
}

unsafe fn dispatch_carbon_event(event: EventRef, user_data: *mut c_void) -> OSStatus {
    if event.is_null() || user_data.is_null() {
        return EVENT_NOT_HANDLED_ERR;
    }
    let mut hotkey_id = MaybeUninit::<EventHotKeyId>::uninit();
    let status = unsafe {
        get_event_parameter(
            event,
            EVENT_PARAM_DIRECT_OBJECT,
            TYPE_EVENT_HOT_KEY_ID,
            ptr::null_mut(),
            u32::try_from(size_of::<EventHotKeyId>()).expect("EventHotKeyId fits UInt32"),
            ptr::null_mut(),
            hotkey_id.as_mut_ptr().cast(),
        )
    };
    if status != NO_ERR {
        return status;
    }
    let hotkey_id = unsafe { hotkey_id.assume_init() };
    let callback = unsafe { &*user_data.cast::<CallbackState>() };
    let registration = callback.registration.borrow();
    let Some(target) = registration.as_ref() else {
        return EVENT_NOT_HANDLED_ERR;
    };
    let Some(action) = carbon_event_action(hotkey_id.signature, hotkey_id.id, target) else {
        return EVENT_NOT_HANDLED_ERR;
    };
    let _ = callback.action_tx.send(action);
    NO_ERR
}
