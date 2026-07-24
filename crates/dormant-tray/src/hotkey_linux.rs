//! Linux global hotkeys through `KGlobalAccel`, with the XDG portal fallback.

#![cfg(target_os = "linux")]

mod kglobalaccel;
mod portal;

use async_trait::async_trait;
use tokio::sync::mpsc::UnboundedSender;
use tracing::debug;

use self::kglobalaccel::KGlobalAccelHotkeyRegistrar;
use self::portal::PortalHotkeyRegistrar;
use crate::hotkey::{Accelerator, HotkeyError, HotkeyRegistrar};
use crate::menu::Action;

fn accelerator_tokens(accelerator: &Accelerator) -> Result<(Vec<&str>, &str), HotkeyError> {
    let mut tokens: Vec<_> = accelerator.raw.split('+').collect();
    let key = tokens
        .pop()
        .filter(|key| !key.is_empty())
        .ok_or_else(|| HotkeyError::InvalidAccelerator(accelerator.raw.clone()))?;
    if tokens
        .iter()
        .any(|modifier| !matches!(*modifier, "Alt" | "Control" | "Meta" | "Shift" | "Super"))
    {
        return Err(HotkeyError::InvalidAccelerator(accelerator.raw.clone()));
    }
    let mut unique = tokens.clone();
    unique.sort_unstable();
    if unique.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(HotkeyError::InvalidAccelerator(accelerator.raw.clone()));
    }
    Ok((tokens, key))
}

fn claim_action(target: &str, arm: bool) -> Action {
    if arm {
        Action::ArmClaim(target.to_string())
    } else {
        Action::ClaimOne(target.to_string())
    }
}

/// Registrar that tries `KGlobalAccel` before the portal fallback.
pub struct FallbackHotkeyRegistrar {
    primary: Box<dyn HotkeyRegistrar>,
    fallback: Box<dyn HotkeyRegistrar>,
    active: Option<ActiveBackend>,
}

#[derive(Clone, Copy)]
enum ActiveBackend {
    Primary,
    Fallback,
}

impl FallbackHotkeyRegistrar {
    /// Build a registrar from injectable primary and fallback backends.
    #[must_use]
    pub fn new(primary: Box<dyn HotkeyRegistrar>, fallback: Box<dyn HotkeyRegistrar>) -> Self {
        Self {
            primary,
            fallback,
            active: None,
        }
    }
}

#[async_trait]
impl HotkeyRegistrar for FallbackHotkeyRegistrar {
    async fn register_claim(
        &mut self,
        accelerator: &Accelerator,
        target: &str,
        arm: bool,
        tx: UnboundedSender<Action>,
    ) -> Result<(), HotkeyError> {
        self.unregister_claim().await;
        match self
            .primary
            .register_claim(accelerator, target, arm, tx.clone())
            .await
        {
            Ok(()) => {
                self.active = Some(ActiveBackend::Primary);
                Ok(())
            }
            Err(primary_error) => {
                self.primary.unregister_claim().await;
                debug!(error = %primary_error, "KGlobalAccel unavailable; trying portal");
                match self
                    .fallback
                    .register_claim(accelerator, target, arm, tx)
                    .await
                {
                    Ok(()) => {
                        self.active = Some(ActiveBackend::Fallback);
                        Ok(())
                    }
                    Err(fallback_error) => {
                        self.fallback.unregister_claim().await;
                        Err(HotkeyError::DbusError(format!(
                            "KGlobalAccel: {primary_error}; portal: {fallback_error}"
                        )))
                    }
                }
            }
        }
    }

    async fn unregister_claim(&mut self) {
        match self.active.take() {
            Some(ActiveBackend::Primary) => self.primary.unregister_claim().await,
            Some(ActiveBackend::Fallback) => self.fallback.unregister_claim().await,
            None => {}
        }
    }
}

/// Create a Linux registrar with `KGlobalAccel` first and the portal fallback.
#[must_use]
pub fn create_linux_registrar() -> Box<dyn HotkeyRegistrar> {
    Box::new(FallbackHotkeyRegistrar::new(
        Box::new(KGlobalAccelHotkeyRegistrar::new()),
        Box::new(PortalHotkeyRegistrar::new()),
    ))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[test]
    fn activity_claim_policy_flag_selects_shared_or_arm_action() {
        assert_eq!(
            claim_action("monitor", false),
            Action::ClaimOne("monitor".into())
        );
        assert_eq!(
            claim_action("monitor", true),
            Action::ArmClaim("monitor".into())
        );
    }

    struct FakeBackend {
        name: &'static str,
        fail: bool,
        calls: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl HotkeyRegistrar for FakeBackend {
        async fn register_claim(
            &mut self,
            _accelerator: &Accelerator,
            _target: &str,
            _arm: bool,
            _tx: UnboundedSender<Action>,
        ) -> Result<(), HotkeyError> {
            self.calls.lock().unwrap().push(self.name.to_string());
            if self.fail {
                Err(HotkeyError::DbusError(format!("{} unavailable", self.name)))
            } else {
                Ok(())
            }
        }

        async fn unregister_claim(&mut self) {
            self.calls
                .lock()
                .unwrap()
                .push(format!("{}:unregister", self.name));
        }
    }

    #[tokio::test]
    async fn kglobalaccel_is_tried_before_portal_fallback() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let primary = FakeBackend {
            name: "kglobalaccel",
            fail: true,
            calls: Arc::clone(&calls),
        };
        let fallback = FakeBackend {
            name: "portal",
            fail: false,
            calls: Arc::clone(&calls),
        };
        let mut registrar = FallbackHotkeyRegistrar::new(Box::new(primary), Box::new(fallback));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        registrar
            .register_claim(
                &Accelerator::parse("Meta+F12").unwrap(),
                "monitor",
                false,
                tx,
            )
            .await
            .unwrap();

        assert_eq!(
            *calls.lock().unwrap(),
            vec!["kglobalaccel", "kglobalaccel:unregister", "portal"]
        );
    }

    #[test]
    fn create_linux_registrar_returns_valid_object() {
        drop(create_linux_registrar());
    }
}
