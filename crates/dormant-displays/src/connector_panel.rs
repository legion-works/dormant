//! Connector panel identity from Linux DRM sysfs, independent of DDC availability.

/// Read the uniquely connected panel on a compositor connector.
#[must_use]
pub fn read(connector: &str) -> Option<String> {
    read_at(std::path::Path::new("/sys/class/drm"), connector)
}

/// Read from an injectable DRM root; unavailable or ambiguous evidence is `None`.
#[must_use]
#[cfg(not(target_os = "linux"))]
pub fn read_at(_root: &std::path::Path, _connector: &str) -> Option<String> {
    None
}

/// Read from an injectable DRM root; unavailable or ambiguous evidence is `None`.
#[must_use]
#[cfg(target_os = "linux")]
pub fn read_at(root: &std::path::Path, connector: &str) -> Option<String> {
    let mut connected = None;
    for entry in std::fs::read_dir(root).ok()? {
        let entry = entry.ok()?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(card) = name
            .strip_suffix(connector)
            .and_then(|s| s.strip_suffix('-'))
        else {
            continue;
        };
        if !card
            .strip_prefix("card")
            .is_some_and(|number| !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()))
        {
            continue;
        }
        let path = entry.path();
        if std::fs::read_to_string(path.join("status"))
            .ok()
            .as_deref()
            .map(str::trim)
            != Some("connected")
        {
            continue;
        }
        if connected.replace(path).is_some() {
            return None;
        }
    }
    let edid = std::fs::read(connected?.join("edid")).ok()?;
    let identity = crate::vcp_ops::parse_edid_identity(&edid)?;
    crate::vcp_ops::VcpDisplayInfo {
        ident_string: String::new(),
        manufacturer: identity.manufacturer,
        model: identity.model,
        serial: identity.serial,
    }
    .claim_identity()
}

/// Injectable async boundary shared by ownership and capture gates.
#[async_trait::async_trait]
pub trait ConnectorPanelReader: Send + Sync {
    /// Return the connector's claim identity, or inconclusive evidence.
    async fn identity(&self, connector: &str) -> Option<String>;
}

/// Production DRM reader; filesystem reads do not block the async executor.
pub struct SystemConnectorPanelReader;

#[async_trait::async_trait]
impl ConnectorPanelReader for SystemConnectorPanelReader {
    async fn identity(&self, connector: &str) -> Option<String> {
        let connector = connector.to_owned();
        tokio::task::spawn_blocking(move || read(&connector))
            .await
            .ok()
            .flatten()
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::read_at;

    fn fixture(root: &std::path::Path, card: &str, status: &str, edid: &[u8]) {
        let path = root.join(format!("{card}-HDMI-A-1"));
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("status"), status).unwrap();
        std::fs::write(path.join("edid"), edid).unwrap();
    }

    #[test]
    fn connected_connector_identity() {
        let root = tempfile::tempdir().unwrap();
        fixture(
            root.path(),
            "card1",
            "connected\n",
            &crate::vcp_ops::tests::ag326uzd_edid(),
        );
        assert_eq!(
            read_at(root.path(), "HDMI-A-1").as_deref(),
            Some("AOC:AG326UZD:XK2R9JA000013")
        );
    }

    #[test]
    fn disconnected_connector_is_unknown() {
        let root = tempfile::tempdir().unwrap();
        fixture(
            root.path(),
            "card1",
            "disconnected\n",
            &crate::vcp_ops::tests::ag326uzd_edid(),
        );
        assert_eq!(read_at(root.path(), "HDMI-A-1"), None);
    }

    #[test]
    fn absent_connector_is_unknown() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(read_at(root.path(), "HDMI-A-1"), None);
    }

    #[test]
    fn short_edid_is_unknown() {
        let root = tempfile::tempdir().unwrap();
        fixture(root.path(), "card1", "connected", &[0; 127]);
        assert_eq!(read_at(root.path(), "HDMI-A-1"), None);
    }

    #[test]
    fn ambiguous_connector_is_unknown() {
        let root = tempfile::tempdir().unwrap();
        for card in ["card1", "card2"] {
            fixture(
                root.path(),
                card,
                "connected",
                &crate::vcp_ops::tests::ag326uzd_edid(),
            );
        }
        assert_eq!(read_at(root.path(), "HDMI-A-1"), None);
    }
}
