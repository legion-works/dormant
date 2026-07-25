//! mDNS advertisement and browsing for short-lived instance-pairing windows.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::net::{IpAddr, SocketAddr, UdpSocket};

use anyhow::{Context as _, Result, bail};
use base64::Engine as _;
use dormant_core::claim::Epoch;
use dormant_core::coordination::CoordinationHandle;
use dormant_core::peers::{DiscoverAnnounce, PAIR_PROTOCOL_VERSION};
use mdns_sd::{ResolvedService, ServiceDaemon, ServiceEvent, ServiceInfo};

/// DNS-SD service type used exclusively for dormant instance pairing.
pub const PAIR_SERVICE_TYPE: &str = "_dormant._tcp.local.";

/// DNS-SD service type used for paired peers' always-on claim endpoints.
pub const CLAIM_SERVICE_TYPE: &str = "_dormant-claim._tcp.local.";

/// Privacy-minimal claim endpoint announcement carried by the presence service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimPresenceAnnounce {
    /// Claim protocol version advertised by the endpoint.
    pub protocol_version: u16,
    /// Stable identity of the paired instance.
    pub instance_id: String,
    /// TCP port on which the authenticated claim listener is bound.
    pub port: u16,
    /// Daemon boot epoch so peers can address frames to this instance.
    /// Unset in mDNS records from older daemons that predate the per-peer
    /// addressing fix (Bug C/D).
    ///
    /// Advisory-only — carried over unauthenticated mDNS, never a credential.
    /// A wrong epoch from a hostile advertiser causes a rejected frame at
    /// the recipient, not an accepted one.
    pub boot_epoch: Option<String>,
}

/// One validated discovery update from a browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowseEvent {
    /// A responder is advertising a live pairing window.
    Resolved {
        /// Validated TXT announcement carried by the service.
        peer: DiscoverAnnounce,
        /// Socket addresses reported by the mDNS resolver for this service.
        addresses: BTreeSet<SocketAddr>,
    },
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

/// Discovery update from the always-on claim-presence browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimPresenceEvent {
    /// A paired claim endpoint became resolvable.
    Resolved {
        /// Validated, privacy-minimal endpoint announcement.
        peer: ClaimPresenceAnnounce,
        /// Resolver-reported endpoint addresses.
        addresses: BTreeSet<SocketAddr>,
    },
}

/// Narrow mDNS seam for the always-on claim-presence service.
pub trait ClaimPresenceBackend: Send + Sync {
    /// Publish a live claim endpoint.
    ///
    /// # Errors
    ///
    /// Returns an error when the mDNS daemon cannot register the service.
    fn advertise_claim(
        &self,
        service: ClaimPresenceAnnounce,
    ) -> Result<Box<dyn AdvertisementHandle>>;

    /// Browse claim endpoints advertised by paired peers.
    ///
    /// # Errors
    ///
    /// Returns an error when the mDNS daemon cannot start the browse.
    fn browse_claim(&self) -> Result<Box<dyn ClaimPresenceStream>>;
}

/// Delivers passive claim-presence updates.
pub trait ClaimPresenceStream: Send + Sync {
    /// Return an immediately available update, if any.
    ///
    /// # Errors
    ///
    /// Returns an error when the backing mDNS receiver cannot provide an update.
    fn try_next(&mut self) -> Result<Option<ClaimPresenceEvent>>;
}

/// Listener-coupled advertisement and known-peer-only address refresh state.
pub struct ClaimPresence<B> {
    local_instance_id: String,
    boot_epoch: String,
    advertise: bool,
    known_peers: BTreeSet<String>,
    advertised_port: Option<u16>,
    advertisement: Option<Box<dyn AdvertisementHandle>>,
    browse: Option<Box<dyn ClaimPresenceStream>>,
    backend: B,
}

impl<B: ClaimPresenceBackend> ClaimPresence<B> {
    /// Create an idle service. It starts no LAN activity until reconciled.
    #[must_use]
    pub fn new(backend: B, local_instance_id: String, boot_epoch: String, advertise: bool) -> Self {
        Self {
            local_instance_id,
            boot_epoch,
            advertise,
            known_peers: BTreeSet::new(),
            advertised_port: None,
            advertisement: None,
            browse: None,
            backend,
        }
    }

    /// Align advertisement and browsing with the listener and paired-peer snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend cannot advertise or browse.
    pub fn reconcile<I>(&mut self, listener_port: Option<u16>, peers: I) -> Result<()>
    where
        I: IntoIterator<Item = String>,
    {
        self.known_peers = peers.into_iter().collect();
        if self.advertise && listener_port != self.advertised_port {
            self.advertisement = None;
            self.advertised_port = None;
            if let Some(port) = listener_port {
                self.advertisement = Some(self.backend.advertise_claim(ClaimPresenceAnnounce {
                    protocol_version: PAIR_PROTOCOL_VERSION,
                    instance_id: self.local_instance_id.clone(),
                    port,
                    boot_epoch: Some(self.boot_epoch.clone()),
                })?);
                self.advertised_port = Some(port);
                tracing::info!(event = "claim_presence_registered", port);
            } else {
                tracing::info!(event = "claim_presence_unregistered");
            }
        }
        if self.known_peers.is_empty() {
            self.browse = None;
        } else if self.browse.is_none() {
            self.browse = Some(self.backend.browse_claim()?);
        }
        Ok(())
    }

    /// Refresh known peers from pending browser events.
    ///
    /// The callback receives the peer's validated boot epoch when the
    /// remote advertised it — unvalidated mDNS data, purely advisory.
    ///
    /// # Errors
    ///
    /// Returns an error when the active browse stream fails while receiving updates.
    pub fn drain_browse(
        &mut self,
        on_peer_addr: impl FnMut(String, SocketAddr, Option<Epoch>),
    ) -> Result<()> {
        let Some(browse) = self.browse.as_mut() else {
            return Ok(());
        };
        let mut on_peer_addr = on_peer_addr;
        while let Some(ClaimPresenceEvent::Resolved { peer, addresses }) = browse.try_next()? {
            if peer.instance_id == self.local_instance_id
                || !self.known_peers.contains(&peer.instance_id)
            {
                tracing::debug!(event = "claim_presence_unknown_peer", peer = %peer.instance_id);
                continue;
            }
            if let Some(address) = select_resolved_addr(&addresses, peer.port) {
                let epoch = peer
                    .boot_epoch
                    .as_deref()
                    .and_then(|raw| Epoch::try_from(raw).ok());
                tracing::info!(event = "claim_presence_peer_refreshed", peer = %peer.instance_id, %address);
                on_peer_addr(peer.instance_id, address, epoch);
            }
        }
        Ok(())
    }
}

/// Window-gated mDNS pairing discovery state.
pub struct PairDiscovery<B> {
    local_instance_id: String,
    coordination: CoordinationHandle,
    advertisement: Option<Box<dyn AdvertisementHandle>>,
    browse: Option<Box<dyn BrowseStream>>,
    resolved_addrs: BTreeMap<String, SocketAddr>,
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
            resolved_addrs: BTreeMap::new(),
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
                BrowseEvent::Resolved { peer, addresses }
                    if peer.instance_id != self.local_instance_id && valid_announce(&peer) =>
                {
                    self.resolved_addrs.remove(&peer.instance_id);
                    if let Some(address) = select_resolved_addr(&addresses, peer.pairing_port) {
                        self.resolved_addrs
                            .insert(peer.instance_id.clone(), address);
                    }
                    self.coordination.upsert_discovered_peer(peer);
                }
                BrowseEvent::Expired { instance_id } => {
                    self.coordination.expire_discovered_peer(&instance_id);
                    self.resolved_addrs.remove(&instance_id);
                }
                BrowseEvent::Resolved { .. } => {}
            }
        }
        Ok(())
    }

    /// Return the ephemeral discovery state; ownership remains in the poll cache.
    #[must_use]
    pub fn discovered_peers(&self) -> BTreeMap<String, DiscoverAnnounce> {
        self.coordination.discovered_peers().into_iter().collect()
    }

    /// Return the ephemeral connect address for one currently discovered peer.
    #[must_use]
    pub fn discovered_peer_addr(&self, instance_id: &str) -> Option<SocketAddr> {
        self.resolved_addrs.get(instance_id).copied()
    }
}

fn select_resolved_addr(addresses: &BTreeSet<SocketAddr>, pairing_port: u16) -> Option<SocketAddr> {
    addresses
        .iter()
        .copied()
        .find(|address| !address.ip().is_loopback() && !address.ip().is_unspecified())
        // The TXT port is authenticated by the ratified announcement; the
        // resolver's service port is not trusted as a connect target.
        .map(|address| SocketAddr::new(address.ip(), pairing_port))
}

/// Discover the primary LAN address selected by the operating system's route table.
#[allow(
    dead_code,
    reason = "Task 14a-2 resolves the bind target while opening a pairing listener."
)]
pub(crate) fn primary_lan_bind_ip() -> Result<IpAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0").context("bind UDP socket for LAN address probe")?;
    socket
        .connect("192.0.2.1:9")
        .context("resolve a primary LAN address through the route table")?;
    let address = socket
        .local_addr()
        .context("read primary LAN address from UDP socket")?
        .ip();
    ensure_bindable_lan_ip(address)
}

/// Resolve a validated configuration override or discover the primary LAN address.
#[allow(
    dead_code,
    reason = "Task 14a-2 resolves the bind target while opening a pairing listener."
)]
pub(crate) fn resolve_bind_ip(config_override: Option<&str>) -> Result<IpAddr> {
    match config_override {
        Some(address) => address
            .parse()
            .context("parse coordination pairing_bind_address")
            .and_then(ensure_bindable_lan_ip),
        None => primary_lan_bind_ip(),
    }
}

#[allow(
    dead_code,
    reason = "Task 14a-2 reaches this through the pairing listener bind resolver."
)]
fn ensure_bindable_lan_ip(address: IpAddr) -> Result<IpAddr> {
    if address.is_loopback() || address.is_unspecified() {
        bail!("pairing listener requires a non-loopback, non-wildcard LAN address");
    }
    Ok(address)
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

/// Translate a claim endpoint into its deliberately minimal TXT surface.
#[must_use]
pub fn claim_txt_records(service: &ClaimPresenceAnnounce) -> BTreeMap<String, String> {
    let mut records = BTreeMap::from([
        ("v".to_owned(), service.protocol_version.to_string()),
        ("instance_id".to_owned(), service.instance_id.clone()),
        ("port".to_owned(), service.port.to_string()),
    ]);
    if let Some(ref epoch) = service.boot_epoch {
        records.insert("boot_epoch".to_owned(), epoch.clone());
    }
    records
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

fn valid_claim_announce(service: &ClaimPresenceAnnounce) -> bool {
    service.protocol_version == PAIR_PROTOCOL_VERSION
        && service.port != 0
        && base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&service.instance_id)
            .is_ok_and(|bytes| bytes.len() == 32)
}

fn announce_from_txt(records: &BTreeMap<String, String>) -> Option<DiscoverAnnounce> {
    let announcement = DiscoverAnnounce {
        protocol_version: records.get("v")?.parse().ok()?,
        instance_id: records.get("instance_id")?.to_owned(),
        display_name: records.get("display_name")?.to_owned(),
        pairing_port: records.get("pairing_port")?.parse().ok()?,
        window_id: records.get("window_id")?.to_owned(),
    };
    valid_announce(&announcement).then_some(announcement)
}

fn claim_announce_from_txt(records: &BTreeMap<String, String>) -> Option<ClaimPresenceAnnounce> {
    let announcement = ClaimPresenceAnnounce {
        protocol_version: records.get("v")?.parse().ok()?,
        instance_id: records.get("instance_id")?.to_owned(),
        port: records.get("port")?.parse().ok()?,
        boot_epoch: records.get("boot_epoch").cloned(),
    };
    valid_claim_announce(&announcement).then_some(announcement)
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

impl ClaimPresenceBackend for MdnsSdBackend {
    fn advertise_claim(
        &self,
        service: ClaimPresenceAnnounce,
    ) -> Result<Box<dyn AdvertisementHandle>> {
        let instance_name = format!("dormant-claim-{}", &service.instance_id[..8]);
        let service_info = ServiceInfo::new(
            CLAIM_SERVICE_TYPE,
            &instance_name,
            "dormant.local.",
            (),
            service.port,
            HashMap::from_iter(claim_txt_records(&service)),
        )
        .context("build mDNS claim-presence service")?
        .enable_addr_auto();
        let fullname = service_info.get_fullname().to_owned();
        self.daemon
            .register(service_info)
            .context("register mDNS claim-presence service")?;
        Ok(Box::new(MdnsSdAdvertisement {
            daemon: self.daemon.clone(),
            fullname,
        }))
    }

    fn browse_claim(&self) -> Result<Box<dyn ClaimPresenceStream>> {
        let receiver = self
            .daemon
            .browse(CLAIM_SERVICE_TYPE)
            .context("browse mDNS claim-presence services")?;
        Ok(Box::new(MdnsSdClaimBrowse {
            daemon: self.daemon.clone(),
            receiver,
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

struct MdnsSdClaimBrowse {
    daemon: ServiceDaemon,
    receiver: mdns_sd::Receiver<ServiceEvent>,
}

impl ClaimPresenceStream for MdnsSdClaimBrowse {
    fn try_next(&mut self) -> Result<Option<ClaimPresenceEvent>> {
        let Ok(event) = self.receiver.try_recv() else {
            return Ok(None);
        };
        let ServiceEvent::ServiceResolved(resolved) = event else {
            return Ok(None);
        };
        let records = ["v", "instance_id", "port"]
            .into_iter()
            .map(|key| {
                Some((
                    key.to_owned(),
                    resolved.get_property_val_str(key)?.to_owned(),
                ))
            })
            .collect::<Option<BTreeMap<_, _>>>();
        let Some(peer) = records.as_ref().and_then(claim_announce_from_txt) else {
            return Ok(None);
        };
        Ok(Some(ClaimPresenceEvent::Resolved {
            peer,
            addresses: resolved
                .get_addresses()
                .iter()
                .map(mdns_sd::ScopedIp::to_ip_addr)
                .map(|address| SocketAddr::new(address, resolved.get_port()))
                .collect(),
        }))
    }
}

impl Drop for MdnsSdClaimBrowse {
    fn drop(&mut self) {
        if let Err(error) = self.daemon.stop_browse(CLAIM_SERVICE_TYPE) {
            tracing::warn!(event = "mdns_stop_browse_failed", %error);
        }
    }
}

impl BrowseStream for MdnsSdBrowse {
    fn try_next(&mut self) -> Result<Option<BrowseEvent>> {
        let Ok(event) = self.receiver.try_recv() else {
            return Ok(None);
        };
        match event {
            ServiceEvent::ServiceResolved(resolved) => {
                let fullname = resolved.get_fullname().to_owned();
                let resolved_peer = announce_from_resolved(&resolved);
                if let Some((peer, addresses)) = resolved_peer {
                    self.fullname_to_instance
                        .insert(fullname, peer.instance_id.clone());
                    Ok(Some(BrowseEvent::Resolved { peer, addresses }))
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

fn announce_from_resolved(
    service: &ResolvedService,
) -> Option<(DiscoverAnnounce, BTreeSet<SocketAddr>)> {
    let records = [
        "v",
        "instance_id",
        "display_name",
        "pairing_port",
        "window_id",
    ]
    .into_iter()
    .map(|key| {
        Some((
            key.to_owned(),
            service.get_property_val_str(key)?.to_owned(),
        ))
    })
    .collect::<Option<BTreeMap<_, _>>>()?;
    let announcement = announce_from_txt(&records)?;
    Some((
        announcement,
        service
            .get_addresses()
            .iter()
            .map(mdns_sd::ScopedIp::to_ip_addr)
            .map(|address| SocketAddr::new(address, service.get_port()))
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, VecDeque};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::{Arc, Mutex};

    use base64::Engine as _;
    use dormant_core::coordination::CoordinationHandle;
    use dormant_core::peers::{DiscoverAnnounce, PAIR_PROTOCOL_VERSION};
    use dormant_core::types::DisplayId;

    use super::{
        AdvertisementHandle, BrowseEvent, BrowseStream, ClaimPresence, ClaimPresenceAnnounce,
        ClaimPresenceBackend, ClaimPresenceEvent, ClaimPresenceStream, MdnsBackend, PairDiscovery,
        claim_announce_from_txt, claim_txt_records, primary_lan_bind_ip, resolve_bind_ip,
        txt_records,
    };

    #[derive(Clone, Default)]
    struct FakeBackend {
        state: Arc<Mutex<FakeState>>,
    }

    #[derive(Default)]
    struct FakeState {
        advertisements: Vec<DiscoverAnnounce>,
        events: VecDeque<BrowseEvent>,
        browse_calls: usize,
        claim_advertisements: Vec<ClaimPresenceAnnounce>,
        claim_events: VecDeque<ClaimPresenceEvent>,
    }

    struct FakeAdvertisement;

    impl AdvertisementHandle for FakeAdvertisement {}

    struct FakeBrowse {
        state: Arc<Mutex<FakeState>>,
    }

    struct FakeClaimBrowse {
        state: Arc<Mutex<FakeState>>,
    }

    impl ClaimPresenceStream for FakeClaimBrowse {
        fn try_next(&mut self) -> anyhow::Result<Option<ClaimPresenceEvent>> {
            Ok(self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .claim_events
                .pop_front())
        }
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

    impl ClaimPresenceBackend for FakeBackend {
        fn advertise_claim(
            &self,
            service: ClaimPresenceAnnounce,
        ) -> anyhow::Result<Box<dyn AdvertisementHandle>> {
            self.state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .claim_advertisements
                .push(service);
            Ok(Box::new(FakeAdvertisement))
        }

        fn browse_claim(&self) -> anyhow::Result<Box<dyn ClaimPresenceStream>> {
            Ok(Box::new(FakeClaimBrowse {
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

    #[test]
    fn claim_presence_txt_contains_only_the_privacy_ratified_keys() {
        let announcement = ClaimPresenceAnnounce {
            protocol_version: 1,
            instance_id: instance_id(1),
            port: 42_001,
            boot_epoch: None,
        };

        let keys: Vec<_> = claim_txt_records(&announcement).keys().cloned().collect();
        assert!(keys.contains(&"instance_id".to_owned()));
        assert!(keys.contains(&"port".to_owned()));
        assert!(keys.contains(&"v".to_owned()));
        // boot_epoch is only included when Some — privacy-ratified default.
        assert!(!keys.contains(&"boot_epoch".to_owned()));
    }

    #[test]
    fn pairing_and_presence_txt_records_are_disjoint() {
        let pairing = txt_records(&service(1));
        let presence = claim_txt_records(&ClaimPresenceAnnounce {
            protocol_version: 1,
            instance_id: instance_id(2),
            port: 42_001,
            boot_epoch: None,
        });

        assert!(claim_announce_from_txt(&pairing).is_none());
        assert!(super::announce_from_txt(&presence).is_none());
    }

    #[test]
    fn presence_browser_refreshes_only_known_paired_peers() {
        let backend = FakeBackend::default();
        let known = instance_id(1);
        let stranger = instance_id(2);
        backend
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .claim_events
            .extend([
                ClaimPresenceEvent::Resolved {
                    peer: ClaimPresenceAnnounce {
                        protocol_version: 1,
                        instance_id: stranger,
                        port: 42_001,
                        boot_epoch: None,
                    },
                    addresses: BTreeSet::from([SocketAddr::from((Ipv4Addr::new(192, 0, 2, 2), 9))]),
                },
                ClaimPresenceEvent::Resolved {
                    peer: ClaimPresenceAnnounce {
                        protocol_version: 1,
                        instance_id: known.clone(),
                        port: 42_001,
                        boot_epoch: None,
                    },
                    addresses: BTreeSet::from([SocketAddr::from((Ipv4Addr::new(192, 0, 2, 3), 9))]),
                },
            ]);
        let mut presence = ClaimPresence::new(
            backend,
            instance_id(3),
            "epoch-000000000000".to_owned(),
            false,
        );
        presence.reconcile(Some(42_000), [known.clone()]).unwrap();
        let mut refreshed = Vec::new();
        presence
            .drain_browse(|peer, address, _epoch| refreshed.push((peer, address)))
            .unwrap();

        assert_eq!(
            refreshed,
            vec![(
                known,
                SocketAddr::from((Ipv4Addr::new(192, 0, 2, 3), 42_001))
            )]
        );
    }

    #[test]
    fn disabled_presence_advertisement_never_registers() {
        let backend = FakeBackend::default();
        let mut presence = ClaimPresence::new(
            backend.clone(),
            instance_id(1),
            "epoch-000000000000".to_owned(),
            false,
        );

        presence.reconcile(Some(42_000), [instance_id(2)]).unwrap();

        assert!(
            backend
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .claim_advertisements
                .is_empty()
        );
    }

    fn resolved(peer: DiscoverAnnounce) -> BrowseEvent {
        BrowseEvent::Resolved {
            peer,
            addresses: BTreeSet::new(),
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
            .push_back(resolved(service(1)));

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
            .push_back(resolved(peer.clone()));
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
    fn resolved_addr_retained_and_expired_with_peer() {
        let backend = FakeBackend::default();
        let mut discovery = discovery(backend.clone(), instance_id(1));
        let peer = service(2);
        let resolved_addr = Ipv4Addr::new(192, 168, 1, 50);
        discovery.start_browse().unwrap();
        backend
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .events
            .push_back(BrowseEvent::Resolved {
                peer: peer.clone(),
                addresses: BTreeSet::from([SocketAddr::from((resolved_addr, 41_000))]),
            });

        discovery.drain_browse().unwrap();
        assert_eq!(
            discovery.discovered_peer_addr(&peer.instance_id),
            Some(SocketAddr::from((resolved_addr, peer.pairing_port)))
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
        assert!(discovery.discovered_peer_addr(&peer.instance_id).is_none());
    }

    #[test]
    fn malformed_resolved_addr_dropped() {
        let backend = FakeBackend::default();
        let mut discovery = discovery(backend.clone(), instance_id(1));
        let peer = service(2);
        discovery.start_browse().unwrap();
        backend
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .events
            .push_back(BrowseEvent::Resolved {
                peer: peer.clone(),
                addresses: BTreeSet::from([
                    SocketAddr::from((Ipv4Addr::LOCALHOST, 41_000)),
                    SocketAddr::from((Ipv4Addr::UNSPECIFIED, 41_000)),
                ]),
            });

        discovery.drain_browse().unwrap();
        assert!(discovery.discovered_peer_addr(&peer.instance_id).is_none());
    }

    #[test]
    fn resolved_addr_uses_txt_port_on_mismatch() {
        let backend = FakeBackend::default();
        let mut discovery = discovery(backend.clone(), instance_id(1));
        let peer = service(2);
        let resolved_addr = Ipv4Addr::new(10, 0, 0, 9);
        discovery.start_browse().unwrap();
        backend
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .events
            .push_back(BrowseEvent::Resolved {
                peer: peer.clone(),
                addresses: BTreeSet::from([SocketAddr::from((resolved_addr, 41_000))]),
            });

        discovery.drain_browse().unwrap();
        assert_eq!(
            discovery.discovered_peer_addr(&peer.instance_id),
            Some(SocketAddr::from((resolved_addr, peer.pairing_port)))
        );
    }

    #[test]
    fn primary_lan_address_is_non_loopback_or_errs_gracefully() {
        match primary_lan_bind_ip() {
            Ok(ip) => assert!(!ip.is_loopback() && !ip.is_unspecified()),
            Err(error) => assert!(!error.to_string().is_empty()),
        }
    }

    #[test]
    fn resolve_bind_ip_prefers_override() {
        assert_eq!(
            resolve_bind_ip(Some("10.1.1.5")).unwrap(),
            IpAddr::V4(Ipv4Addr::new(10, 1, 1, 5))
        );
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
            state.events.push_back(resolved(peer));
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
            .push_back(resolved(peer));

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
            .push_back(resolved(peer));

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
        state.events.push_back(resolved(peer.clone()));
        state.events.push_back(BrowseEvent::Expired {
            instance_id: peer.instance_id,
        });
        drop(state);

        discovery.drain_browse().unwrap();
        assert_eq!(handle.snapshot(), before);
        assert!(handle.discovered_peers().is_empty());
    }
}
