//! mDNS advertisement and browsing for short-lived instance-pairing windows.

use std::collections::{BTreeMap, HashMap};

use anyhow::{Context as _, Result};
use base64::Engine as _;
use dormant_core::coordination::CoordinationHandle;
use dormant_core::peers::{DiscoverAnnounce, PAIR_PROTOCOL_VERSION};
use mdns_sd::{ResolvedService, ServiceDaemon, ServiceEvent, ServiceInfo};

/// DNS-SD service type used exclusively for dormant instance pairing.
pub const PAIR_SERVICE_TYPE: &str = "_dormant._tcp.local.";

/// One validated discovery update from a browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowseEvent {
    /// A responder is advertising a live pairing window.
    Resolved(DiscoverAnnounce),
    /// A responder has withdrawn its pairing window.
    Expired {
        /// Stable identifier of the responder whose discovery record expired.
        instance_id: String,
    },
}

/// Owns one active mDNS advertisement.
pub trait AdvertisementHandle: Send + Sync {}

/// Delivers mDNS discovery updates without binding the coordinator to a runtime.
pub trait BrowseStream: Send + Sync {
    /// Return an immediately available update, if any.
    ///
    /// # Errors
    ///
    /// Returns an error when the backing mDNS receiver cannot provide an update.
    fn try_next(&mut self) -> Result<Option<BrowseEvent>>;
}

/// Narrow mDNS seam used by the coordinator and its deterministic tests.
pub trait MdnsBackend: Send + Sync {
    /// Publish one responder's pairing window.
    ///
    /// # Errors
    ///
    /// Returns an error when the mDNS daemon cannot register the service.
    fn advertise(&self, service: DiscoverAnnounce) -> Result<Box<dyn AdvertisementHandle>>;

    /// Start an on-demand browse for pairing responders.
    ///
    /// # Errors
    ///
    /// Returns an error when the mDNS daemon cannot start the browse.
    fn browse(&self) -> Result<Box<dyn BrowseStream>>;
}

/// Window-gated mDNS pairing discovery state.
pub struct PairDiscovery<B> {
    local_instance_id: String,
    coordination: CoordinationHandle,
    advertisement: Option<Box<dyn AdvertisementHandle>>,
    browse: Option<Box<dyn BrowseStream>>,
    backend: B,
}

impl<B: MdnsBackend> PairDiscovery<B> {
    /// Construct an idle adapter. Construction neither advertises nor browses.
    #[must_use]
    pub fn new(backend: B, local_instance_id: String, coordination: CoordinationHandle) -> Self {
        Self {
            local_instance_id,
            coordination,
            advertisement: None,
            browse: None,
            backend,
        }
    }

    /// Construct an adapter only when the operator has enabled coordination.
    #[must_use]
    pub fn new_if_enabled(
        enabled: bool,
        backend: B,
        local_instance_id: String,
        coordination: CoordinationHandle,
    ) -> Option<Self> {
        enabled.then(|| Self::new(backend, local_instance_id, coordination))
    }

    /// Advertise an operator-opened pairing window and retain its withdrawal handle.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend cannot publish the pairing service.
    pub fn open_pairing_window(&mut self, service: DiscoverAnnounce) -> Result<()> {
        let advertisement = self.backend.advertise(service)?;
        self.advertisement = Some(advertisement);
        Ok(())
    }

    /// Withdraw the pairing service when its window closes, expires, or succeeds.
    pub fn close_pairing_window(&mut self) {
        self.advertisement = None;
    }

    /// Start browsing only for an initiator's active pairing attempt.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend cannot start discovery.
    pub fn start_browse(&mut self) -> Result<()> {
        if self.browse.is_none() {
            self.browse = Some(self.backend.browse()?);
        }
        Ok(())
    }

    /// Stop the on-demand pairing browse.
    pub fn stop_browse(&mut self) {
        self.browse = None;
    }

    /// Drain immediately available discovery updates into the separate peer cache.
    ///
    /// # Errors
    ///
    /// Returns an error when the active browse stream fails while receiving updates.
    pub fn drain_browse(&mut self) -> Result<()> {
        let Some(browse) = self.browse.as_mut() else {
            return Ok(());
        };

        while let Some(event) = browse.try_next()? {
            match event {
                BrowseEvent::Resolved(peer)
                    if peer.instance_id != self.local_instance_id && valid_announce(&peer) =>
                {
                    self.coordination.upsert_discovered_peer(peer);
                }
                BrowseEvent::Expired { instance_id } => {
                    self.coordination.expire_discovered_peer(&instance_id);
                }
                BrowseEvent::Resolved(_) => {}
            }
        }
        Ok(())
    }

    /// Return the ephemeral discovery state; ownership remains in the poll cache.
    #[must_use]
    pub fn discovered_peers(&self) -> BTreeMap<String, DiscoverAnnounce> {
        self.coordination.discovered_peers().into_iter().collect()
    }
}

/// Translate a discovery announcement into the ratified TXT surface.
#[must_use]
pub fn txt_records(service: &DiscoverAnnounce) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("v".to_owned(), service.protocol_version.to_string()),
        ("instance_id".to_owned(), service.instance_id.clone()),
        ("display_name".to_owned(), service.display_name.clone()),
        ("pairing_port".to_owned(), service.pairing_port.to_string()),
        ("window_id".to_owned(), service.window_id.clone()),
    ])
}

fn valid_announce(service: &DiscoverAnnounce) -> bool {
    service.protocol_version == PAIR_PROTOCOL_VERSION
        && service.pairing_port != 0
        && base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&service.instance_id)
            .is_ok_and(|bytes| bytes.len() == 32)
        && !service.window_id.is_empty()
        && !service.display_name.is_empty()
}

/// Production backend backed by `mdns-sd`'s daemon thread.
pub struct MdnsSdBackend {
    daemon: ServiceDaemon,
}

impl MdnsSdBackend {
    /// Start the local mDNS daemon without advertising or browsing.
    ///
    /// # Errors
    ///
    /// Returns an error when the local mDNS daemon cannot acquire its socket.
    pub fn new() -> Result<Self> {
        ServiceDaemon::new()
            .map(|daemon| Self { daemon })
            .context("start mDNS daemon")
    }
}

impl Drop for MdnsSdBackend {
    fn drop(&mut self) {
        if let Err(error) = self.daemon.shutdown() {
            tracing::warn!(event = "mdns_shutdown_failed", %error);
        }
    }
}

impl MdnsBackend for MdnsSdBackend {
    fn advertise(&self, service: DiscoverAnnounce) -> Result<Box<dyn AdvertisementHandle>> {
        let instance_name = format!("dormant-{}", &service.instance_id[..8]);
        let service_info = ServiceInfo::new(
            PAIR_SERVICE_TYPE,
            &instance_name,
            "dormant.local.",
            (),
            service.pairing_port,
            HashMap::from_iter(txt_records(&service)),
        )
        .context("build mDNS service")?
        .enable_addr_auto();
        let fullname = service_info.get_fullname().to_owned();
        self.daemon
            .register(service_info)
            .context("register mDNS pairing service")?;
        Ok(Box::new(MdnsSdAdvertisement {
            daemon: self.daemon.clone(),
            fullname,
        }))
    }

    fn browse(&self) -> Result<Box<dyn BrowseStream>> {
        let receiver = self
            .daemon
            .browse(PAIR_SERVICE_TYPE)
            .context("browse mDNS pairing services")?;
        Ok(Box::new(MdnsSdBrowse {
            daemon: self.daemon.clone(),
            receiver,
            fullname_to_instance: BTreeMap::new(),
        }))
    }
}

struct MdnsSdAdvertisement {
    daemon: ServiceDaemon,
    fullname: String,
}

impl AdvertisementHandle for MdnsSdAdvertisement {}

impl Drop for MdnsSdAdvertisement {
    fn drop(&mut self) {
        if let Err(error) = self.daemon.unregister(&self.fullname) {
            tracing::warn!(event = "mdns_unregister_failed", %self.fullname, %error);
        }
    }
}

struct MdnsSdBrowse {
    daemon: ServiceDaemon,
    receiver: mdns_sd::Receiver<ServiceEvent>,
    fullname_to_instance: BTreeMap<String, String>,
}

impl BrowseStream for MdnsSdBrowse {
    fn try_next(&mut self) -> Result<Option<BrowseEvent>> {
        let Ok(event) = self.receiver.try_recv() else {
            return Ok(None);
        };
        match event {
            ServiceEvent::ServiceResolved(resolved) => {
                let fullname = resolved.get_fullname().to_owned();
                let peer = announce_from_resolved(&resolved);
                if let Some(peer) = peer {
                    self.fullname_to_instance
                        .insert(fullname, peer.instance_id.clone());
                    Ok(Some(BrowseEvent::Resolved(peer)))
                } else {
                    Ok(None)
                }
            }
            ServiceEvent::ServiceRemoved(_, fullname) => Ok(self
                .fullname_to_instance
                .remove(&fullname)
                .map(|instance_id| BrowseEvent::Expired { instance_id })),
            _ => Ok(None),
        }
    }
}

impl Drop for MdnsSdBrowse {
    fn drop(&mut self) {
        if let Err(error) = self.daemon.stop_browse(PAIR_SERVICE_TYPE) {
            tracing::warn!(event = "mdns_stop_browse_failed", %error);
        }
    }
}

fn announce_from_resolved(service: &ResolvedService) -> Option<DiscoverAnnounce> {
    let protocol_version = service.get_property_val_str("v")?.parse().ok()?;
    let pairing_port = service.get_property_val_str("pairing_port")?.parse().ok()?;
    let announcement = DiscoverAnnounce {
        protocol_version,
        instance_id: service.get_property_val_str("instance_id")?.to_owned(),
        display_name: service.get_property_val_str("display_name")?.to_owned(),
        pairing_port,
        window_id: service.get_property_val_str("window_id")?.to_owned(),
    };
    valid_announce(&announcement).then_some(announcement)
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, VecDeque};
    use std::sync::{Arc, Mutex};

    use base64::Engine as _;
    use dormant_core::coordination::CoordinationHandle;
    use dormant_core::peers::{DiscoverAnnounce, PAIR_PROTOCOL_VERSION};
    use dormant_core::types::DisplayId;

    use super::{AdvertisementHandle, BrowseEvent, BrowseStream, MdnsBackend, PairDiscovery};

    #[derive(Clone, Default)]
    struct FakeBackend {
        state: Arc<Mutex<FakeState>>,
    }

    #[derive(Default)]
    struct FakeState {
        advertisements: Vec<DiscoverAnnounce>,
        events: VecDeque<BrowseEvent>,
        browse_calls: usize,
    }

    struct FakeAdvertisement;

    impl AdvertisementHandle for FakeAdvertisement {}

    struct FakeBrowse {
        state: Arc<Mutex<FakeState>>,
    }

    impl BrowseStream for FakeBrowse {
        fn try_next(&mut self) -> anyhow::Result<Option<BrowseEvent>> {
            Ok(self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .events
                .pop_front())
        }
    }

    impl MdnsBackend for FakeBackend {
        fn advertise(
            &self,
            service: DiscoverAnnounce,
        ) -> anyhow::Result<Box<dyn AdvertisementHandle>> {
            self.state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .advertisements
                .push(service);
            Ok(Box::new(FakeAdvertisement))
        }

        fn browse(&self) -> anyhow::Result<Box<dyn BrowseStream>> {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.browse_calls += 1;
            drop(state);
            Ok(Box::new(FakeBrowse {
                state: Arc::clone(&self.state),
            }))
        }
    }

    fn instance_id(seed: u8) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([seed; 32])
    }

    fn service(seed: u8) -> DiscoverAnnounce {
        DiscoverAnnounce {
            protocol_version: PAIR_PROTOCOL_VERSION,
            instance_id: instance_id(seed),
            display_name: format!("Machine {seed}"),
            pairing_port: 42_000,
            window_id: format!("window-{seed}"),
        }
    }

    fn discovery(backend: FakeBackend, local_instance_id: String) -> PairDiscovery<FakeBackend> {
        PairDiscovery::new(backend, local_instance_id, CoordinationHandle::new([]))
    }

    #[test]
    fn advertisement_contains_only_ratified_txt_keys() {
        let backend = FakeBackend::default();
        let mut discovery = discovery(backend.clone(), instance_id(1));
        discovery.open_pairing_window(service(2)).unwrap();

        let advertised = backend
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .advertisements
            .pop()
            .unwrap();
        let keys: BTreeSet<_> = super::txt_records(&advertised).into_keys().collect();
        assert_eq!(
            keys,
            BTreeSet::from([
                "display_name".to_owned(),
                "instance_id".to_owned(),
                "pairing_port".to_owned(),
                "v".to_owned(),
                "window_id".to_owned(),
            ])
        );
    }

    #[test]
    fn browse_ignores_self() {
        let backend = FakeBackend::default();
        let self_id = instance_id(1);
        let mut discovery = discovery(backend.clone(), self_id);
        discovery.start_browse().unwrap();
        backend
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .events
            .push_back(BrowseEvent::Resolved(service(1)));

        discovery.drain_browse().unwrap();
        assert!(discovery.discovered_peers().is_empty());
    }

    #[test]
    fn browse_updates_and_expires_discovered_peer() {
        let backend = FakeBackend::default();
        let mut discovery = discovery(backend.clone(), instance_id(1));
        let peer = service(2);
        discovery.start_browse().unwrap();
        backend
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .events
            .push_back(BrowseEvent::Resolved(peer.clone()));
        discovery.drain_browse().unwrap();
        assert_eq!(
            discovery.discovered_peers().get(&peer.instance_id),
            Some(&peer)
        );

        backend
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .events
            .push_back(BrowseEvent::Expired {
                instance_id: peer.instance_id.clone(),
            });
        discovery.drain_browse().unwrap();
        assert!(discovery.discovered_peers().is_empty());
    }

    #[test]
    fn malformed_txt_is_ignored() {
        let backend = FakeBackend::default();
        let mut discovery = discovery(backend.clone(), instance_id(1));
        discovery.start_browse().unwrap();
        let mut bad_version = service(2);
        bad_version.protocol_version += 1;
        let mut bad_port = service(3);
        bad_port.pairing_port = 0;
        let mut bad_instance_id = service(4);
        bad_instance_id.instance_id =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([4; 31]);
        let mut missing_window_id = service(5);
        missing_window_id.window_id.clear();
        let mut state = backend
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for peer in [bad_version, bad_port, bad_instance_id, missing_window_id] {
            state.events.push_back(BrowseEvent::Resolved(peer));
        }
        drop(state);

        discovery.drain_browse().unwrap();
        assert!(discovery.discovered_peers().is_empty());
    }

    #[test]
    fn empty_display_name_is_ignored() {
        let backend = FakeBackend::default();
        let mut discovery = discovery(backend.clone(), instance_id(1));
        let mut peer = service(2);
        peer.display_name.clear();
        discovery.start_browse().unwrap();
        backend
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .events
            .push_back(BrowseEvent::Resolved(peer));

        discovery.drain_browse().unwrap();
        assert!(discovery.discovered_peers().is_empty());
    }

    #[test]
    fn non_base64_instance_id_is_ignored() {
        let backend = FakeBackend::default();
        let mut discovery = discovery(backend.clone(), instance_id(1));
        let mut peer = service(2);
        peer.instance_id = "not-base64url!".into();
        discovery.start_browse().unwrap();
        backend
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .events
            .push_back(BrowseEvent::Resolved(peer));

        discovery.drain_browse().unwrap();
        assert!(discovery.discovered_peers().is_empty());
    }

    #[test]
    fn coordination_disabled_does_not_advertise_or_browse() {
        let backend = FakeBackend::default();
        let discovery = PairDiscovery::new_if_enabled(
            false,
            backend.clone(),
            instance_id(1),
            CoordinationHandle::new([]),
        );

        assert!(discovery.is_none());
        let state = backend
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(state.advertisements.is_empty());
        assert_eq!(state.browse_calls, 0);
    }

    #[test]
    fn mdns_loss_never_changes_ownership() {
        let backend = FakeBackend::default();
        let display = DisplayId("shared".into());
        let handle = CoordinationHandle::new([display.clone()]);
        handle.record_success(&display, 2, 1, None);
        let before = handle.snapshot();
        let mut discovery = PairDiscovery::new(backend.clone(), instance_id(1), handle.clone());
        let peer = service(2);
        discovery.start_browse().unwrap();
        let mut state = backend
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.events.push_back(BrowseEvent::Resolved(peer.clone()));
        state.events.push_back(BrowseEvent::Expired {
            instance_id: peer.instance_id,
        });
        drop(state);

        discovery.drain_browse().unwrap();
        assert_eq!(handle.snapshot(), before);
        assert!(handle.discovered_peers().is_empty());
    }
}
