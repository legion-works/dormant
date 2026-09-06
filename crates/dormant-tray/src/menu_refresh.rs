//! Refresh-channel consumption for the Linux tray menu.

use std::future::Future;

use tokio_util::sync::CancellationToken;

use crate::ipc_loop::RefreshReceiver;

/// Refreshes the tray menu after a visible tray-state change.
pub trait MenuRefresher: Send {
    /// Request a menu rebuild.
    ///
    /// Returns `false` when the backing tray service has shut down.
    fn refresh_menu(&mut self) -> impl Future<Output = bool> + Send;
}

/// Consume tray-state refresh notifications until cancellation or tray shutdown.
pub async fn consume_refresh<R, F>(
    mut refresh_rx: RefreshReceiver,
    cancel: CancellationToken,
    mut refresher: R,
    mut on_refresh: F,
) where
    R: MenuRefresher,
    F: FnMut(),
{
    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            changed = refresh_rx.changed() => {
                if changed.is_err() {
                    break;
                }
                // Notify other consumers before awaiting the tray service: a
                // menu rebuild is a round-trip through `ksni`, so doing it
                // first would make hotkey re-registration wait on the tray's
                // responsiveness.
                on_refresh();
                if !refresher.refresh_menu().await {
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::Duration;

    use tokio::sync::Notify;

    use super::*;

    struct FakeMenuRefresher {
        calls: Arc<AtomicUsize>,
        called: Arc<Notify>,
        remains_available: bool,
    }

    impl MenuRefresher for FakeMenuRefresher {
        fn refresh_menu(&mut self) -> impl Future<Output = bool> + Send {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.called.notify_one();
            std::future::ready(self.remains_available)
        }
    }

    fn fake_refresher(
        remains_available: bool,
    ) -> (FakeMenuRefresher, Arc<AtomicUsize>, Arc<Notify>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let called = Arc::new(Notify::new());
        (
            FakeMenuRefresher {
                calls: calls.clone(),
                called: called.clone(),
                remains_available,
            },
            calls,
            called,
        )
    }

    #[tokio::test]
    async fn refresh_notifications_invoke_the_menu_refresher() {
        let (refresh_tx, refresh_rx) = crate::ipc_loop::refresh_channel();
        let cancel = CancellationToken::new();
        let (refresher, calls, called) = fake_refresher(true);
        let hotkey_notifications = Arc::new(AtomicUsize::new(0));
        let sent_hotkey_notifications = hotkey_notifications.clone();
        let consumer = tokio::spawn(consume_refresh(
            refresh_rx,
            cancel.clone(),
            refresher,
            move || {
                sent_hotkey_notifications.fetch_add(1, Ordering::SeqCst);
            },
        ));

        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let first_call = called.notified();
        refresh_tx.send_replace(());
        tokio::time::timeout(Duration::from_secs(1), first_call)
            .await
            .expect("first sent refresh must reach the menu refresher");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(hotkey_notifications.load(Ordering::SeqCst), 1);

        let second_call = called.notified();
        refresh_tx.send_replace(());
        tokio::time::timeout(Duration::from_secs(1), second_call)
            .await
            .expect("second distinct refresh must reach the menu refresher");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(hotkey_notifications.load(Ordering::SeqCst), 2);

        cancel.cancel();
        consumer.await.expect("refresh consumer must not panic");
    }

    #[tokio::test]
    async fn cancellation_stops_the_refresh_consumer() {
        let (_refresh_tx, refresh_rx) = crate::ipc_loop::refresh_channel();
        let cancel = CancellationToken::new();
        let (refresher, calls, _called) = fake_refresher(true);
        let consumer = tokio::spawn(consume_refresh(
            refresh_rx,
            cancel.clone(),
            refresher,
            || {},
        ));

        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(1), consumer)
            .await
            .expect("cancellation must stop the refresh consumer")
            .expect("refresh consumer must not panic");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn unavailable_menu_refresher_stops_the_consumer() {
        let (refresh_tx, refresh_rx) = crate::ipc_loop::refresh_channel();
        let cancel = CancellationToken::new();
        let (refresher, calls, called) = fake_refresher(false);
        let consumer = tokio::spawn(consume_refresh(refresh_rx, cancel, refresher, || {}));

        let first_call = called.notified();
        refresh_tx.send_replace(());
        tokio::time::timeout(Duration::from_secs(1), first_call)
            .await
            .expect("sent refresh must reach the menu refresher");
        tokio::time::timeout(Duration::from_secs(1), consumer)
            .await
            .expect("unavailable menu refresher must stop the consumer")
            .expect("refresh consumer must not panic");

        refresh_tx.send_replace(());
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
