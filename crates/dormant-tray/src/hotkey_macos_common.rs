//! FFI-independent Carbon hotkey parsing and event routing.

use crate::hotkey::{Accelerator, HotkeyError, accelerator_tokens, claim_action};
use crate::menu::Action;

pub(crate) const HOTKEY_SIGNATURE: u32 = u32::from_be_bytes(*b"dorm");
pub(crate) const CLAIM_HOTKEY_ID: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CarbonAccelerator {
    pub(crate) key_code: u32,
    pub(crate) modifiers: u32,
}

pub(crate) fn carbon_accelerator(
    accelerator: &Accelerator,
) -> Result<CarbonAccelerator, HotkeyError> {
    let (modifiers, key) = accelerator_tokens(accelerator)?;
    let modifiers = modifiers.iter().try_fold(0_u32, |mask, modifier| {
        let bit = match *modifier {
            "Meta" | "Super" => 0x0100,
            "Shift" => 0x0200,
            "Alt" => 0x0800,
            "Control" => 0x1000,
            _ => return Err(HotkeyError::InvalidAccelerator(accelerator.raw.clone())),
        };
        Ok(mask | bit)
    })?;
    let key_code = carbon_key_code(key)
        .ok_or_else(|| HotkeyError::InvalidAccelerator(accelerator.raw.clone()))?;
    Ok(CarbonAccelerator {
        key_code,
        modifiers,
    })
}

pub(crate) fn carbon_event_action(
    signature: u32,
    id: u32,
    target: &str,
    arm: bool,
) -> Option<Action> {
    if signature != HOTKEY_SIGNATURE || id != CLAIM_HOTKEY_ID {
        return None;
    }
    Some(claim_action(target, arm))
}

fn carbon_key_code(key: &str) -> Option<u32> {
    Some(match key {
        "A" => 0x00,
        "S" => 0x01,
        "D" => 0x02,
        "F" => 0x03,
        "H" => 0x04,
        "G" => 0x05,
        "Z" => 0x06,
        "X" => 0x07,
        "C" => 0x08,
        "V" => 0x09,
        "B" => 0x0b,
        "Q" => 0x0c,
        "W" => 0x0d,
        "E" => 0x0e,
        "R" => 0x0f,
        "Y" => 0x10,
        "T" => 0x11,
        "1" => 0x12,
        "2" => 0x13,
        "3" => 0x14,
        "4" => 0x15,
        "6" => 0x16,
        "5" => 0x17,
        "=" => 0x18,
        "9" => 0x19,
        "7" => 0x1a,
        "-" => 0x1b,
        "8" => 0x1c,
        "0" => 0x1d,
        "O" => 0x1f,
        "U" => 0x20,
        "I" => 0x22,
        "P" => 0x23,
        "Enter" => 0x24,
        "L" => 0x25,
        "J" => 0x26,
        "K" => 0x28,
        "N" => 0x2d,
        "M" => 0x2e,
        "Tab" => 0x30,
        "Space" => 0x31,
        "Backspace" => 0x33,
        "Escape" => 0x35,
        "F1" => 0x7a,
        "F2" => 0x78,
        "F3" => 0x63,
        "F4" => 0x76,
        "F5" => 0x60,
        "F6" => 0x61,
        "F7" => 0x62,
        "F8" => 0x64,
        "F9" => 0x65,
        "F10" => 0x6d,
        "F11" => 0x67,
        "F12" => 0x6f,
        "F13" => 0x69,
        "F14" => 0x6b,
        "F15" => 0x71,
        "F16" => 0x6a,
        "F17" => 0x40,
        "F18" => 0x4f,
        "F19" => 0x50,
        "F20" => 0x5a,
        "Home" => 0x73,
        "PageUp" => 0x74,
        "End" => 0x77,
        "PageDown" => 0x79,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::{self, Receiver, Sender};

    use super::*;

    struct FakeCarbon {
        events: Sender<Action>,
    }

    impl FakeCarbon {
        fn emit(&self, signature: u32, id: u32) {
            if let Some(action) = carbon_event_action(signature, id, "monitor", false) {
                self.events.send(action).unwrap();
            }
        }
    }

    fn fake_carbon() -> (FakeCarbon, Receiver<Action>) {
        let (events, receiver) = mpsc::channel();
        (FakeCarbon { events }, receiver)
    }

    #[test]
    fn carbon_event_posts_exactly_one_claim_action() {
        let (registrar, events) = fake_carbon();
        registrar.emit(HOTKEY_SIGNATURE, CLAIM_HOTKEY_ID);
        assert_eq!(events.recv().unwrap(), Action::ClaimOne("monitor".into()));
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn default_accelerator_converts_to_command_f12() {
        let accelerator = Accelerator::parse("Meta+F12").unwrap();
        assert_eq!(
            carbon_accelerator(&accelerator).unwrap(),
            CarbonAccelerator {
                key_code: 0x6f,
                modifiers: 0x0100,
            }
        );
    }

    #[test]
    fn control_alt_letter_accelerator_uses_carbon_masks() {
        let accelerator = Accelerator::parse("Control+Alt+K").unwrap();
        assert_eq!(
            carbon_accelerator(&accelerator).unwrap(),
            CarbonAccelerator {
                key_code: 0x28,
                modifiers: 0x1800,
            }
        );
    }
}
