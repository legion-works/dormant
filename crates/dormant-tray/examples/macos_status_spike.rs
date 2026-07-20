//! Throwaway `AppKit` status-item spike for the operator's macOS GO/NO-GO check.

#[cfg(not(target_os = "macos"))]
fn main() {
    println!("macos_status_spike: unsupported on this platform");
}

#[cfg(target_os = "macos")]
fn main() {
    use objc2::MainThreadMarker;
    use objc2::rc::Retained;
    use objc2::runtime::{NSObject, NSObjectProtocol};
    use objc2::{MainThreadOnly, define_class, msg_send, sel};
    use objc2_app_kit::{
        NSApplication, NSApplicationActivationPolicy, NSImage, NSMenu, NSMenuItem, NSStatusBar,
        NSVariableStatusItemLength,
    };
    use objc2_foundation::NSString;

    struct SpikeTargetIvars;

    define_class!(
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[ivars = SpikeTargetIvars]
        struct SpikeTarget;

        impl SpikeTarget {
            #[unsafe(method(performAction:))]
            fn perform_action(&self, sender: &NSMenuItem) {
                println!("macos_status_spike_click tag={}", sender.tag());
            }
        }

        unsafe impl NSObjectProtocol for SpikeTarget {}
    );

    let mtm = MainThreadMarker::new().expect("macos_status_spike must run on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let status_bar = NSStatusBar::systemStatusBar();
    let status_item = status_bar.statusItemWithLength(NSVariableStatusItemLength);
    let button = status_item
        .button(mtm)
        .expect("new status item must have a button");
    let symbol_name = NSString::from_str("moon.fill");
    let image = NSImage::imageWithSystemSymbolName_accessibilityDescription(&symbol_name, None)
        .expect("moon.fill SF Symbol must be available");
    image.setTemplate(true);
    button.setImage(Some(&image));

    let this = SpikeTarget::alloc(mtm).set_ivars(SpikeTargetIvars);
    let spike_target: Retained<SpikeTarget> = unsafe { msg_send![super(this), init] };

    let menu = NSMenu::new(mtm);
    let click_title = NSString::from_str("Click");
    let key_equivalent = NSString::from_str("");
    let menu_item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &click_title,
            Some(sel!(performAction:)),
            &key_equivalent,
        )
    };
    menu_item.setTag(1);
    unsafe {
        menu_item.setTarget(Some(&spike_target));
    }
    unsafe {
        menu_item.setAction(Some(sel!(performAction:)));
    }
    menu.addItem(&menu_item);
    status_item.setMenu(Some(&menu));

    let _retained = (spike_target, status_item, menu, menu_item);
    app.run();
}
