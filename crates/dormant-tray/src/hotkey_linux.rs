//! Linux global hotkeys through `KGlobalAccel`, with the XDG portal fallback.

#![cfg(target_os = "linux")]

mod kglobalaccel;
mod portal;

use async_trait::async_trait;
use tokio::sync::mpsc::UnboundedSender;
use tracing::debug;

use self::kglobalaccel::KGlobalAccelHotkeyRegistrar;
use self::portal::PortalHotkeyRegistrar;
use crate::hotkey::{
    Accelerator, HotkeyError, HotkeyRegistrar, accelerator_tokens, switch_to_local_action,
};
use crate::menu::Action;

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
        tx: UnboundedSender<Action>,
    ) -> Result<(), HotkeyError> {
        self.unregister_claim().await;
        match self
            .primary
            .register_claim(accelerator, target, tx.clone())
            .await
        {
            Ok(()) => {
                self.active = Some(ActiveBackend::Primary);
                Ok(())
            }
            Err(primary_error) => {
                self.primary.unregister_claim().await;
                debug!(error = %primary_error, "KGlobalAccel unavailable; trying portal");
                match self.fallback.register_claim(accelerator, target, tx).await {
                    Ok(()) => {
                        self.active = Some(ActiveBackend::Fallback);
                        Ok(())
                    }
                    Err(fallback_error) => {
                        self.fallback.unregister_claim().await;
                        Err(HotkeyError::BothBackendsFailed {
                            primary: Box::new(primary_error),
                            fallback: Box::new(fallback_error),
                        })
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
    fn switch_to_local_action_returns_switch_to_local() {
        assert_eq!(
            switch_to_local_action("monitor"),
            Action::SwitchToLocal("monitor".into())
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
            .register_claim(&Accelerator::parse("Meta+F12").unwrap(), "monitor", tx)
            .await
            .unwrap();

        assert_eq!(
            *calls.lock().unwrap(),
            vec!["kglobalaccel", "kglobalaccel:unregister", "portal"]
        );
    }

    #[tokio::test]
    async fn combined_failure_preserves_each_backend_error() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let primary = FakeBackend {
            name: "kglobalaccel",
            fail: true,
            calls: Arc::clone(&calls),
        };
        let fallback = FakeBackend {
            name: "portal",
            fail: true,
            calls,
        };
        let mut registrar = FallbackHotkeyRegistrar::new(Box::new(primary), Box::new(fallback));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        let error = registrar
            .register_claim(&Accelerator::parse("Meta+F12").unwrap(), "monitor", tx)
            .await
            .unwrap_err();

        let HotkeyError::BothBackendsFailed { primary, fallback } = error else {
            panic!("combined failure must retain backend identity");
        };
        assert_eq!(primary.to_string(), "D-Bus error: kglobalaccel unavailable");
        assert_eq!(fallback.to_string(), "D-Bus error: portal unavailable");
    }

    #[test]
    fn create_linux_registrar_returns_valid_object() {
        drop(create_linux_registrar());
    }
}
