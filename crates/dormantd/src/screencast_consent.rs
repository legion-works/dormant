//! Persistent portal consent and its display binding.

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;
    use time::OffsetDateTime;

    fn record() -> ConsentRecord {
        ConsentRecord {
            token: "token-rotated-secret".into(),
            sampled_display: "oled-main".into(),
            granted_at: OffsetDateTime::from_unix_timestamp(1_754_000_000).unwrap(),
            portal_persistent_ids: vec!["persistent-portal-id".into()],
            granted_width: 3840,
            granted_height: 2160,
        }
    }

    #[test]
    fn round_trip_preserves_grant_dimensions_and_binding() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("consent.json");
        let expected = record();
        store_atomic(&path, &expected).unwrap();
        let loaded = load(&path, "oled-main").unwrap();
        assert!(loaded.record() == &expected);
        let binding = loaded.as_binding();
        assert_eq!(binding.granted_width, 3840);
        assert_eq!(binding.granted_height, 2160);
    }

    #[test]
    fn changed_display_invalidates_before_binding() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("consent.json");
        store_atomic(&path, &record()).unwrap();
        let error = load(&path, "other-display").err().unwrap();
        assert!(matches!(error, ConsentError::DisplayChanged));
        assert_eq!(error.to_string(), "wear_sampling_display_changed");
    }

    #[test]
    fn corrupt_and_absent_records_are_distinct_errors() {
        let dir = tempdir().unwrap();
        let absent = load(&dir.path().join("missing.json"), "oled-main")
            .err()
            .unwrap();
        assert!(matches!(absent, ConsentError::NotFound));
        let path = dir.path().join("corrupt.json");
        fs::write(&path, b"not json").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let corrupt = load(&path, "oled-main").err().unwrap();
        assert!(matches!(corrupt, ConsentError::InvalidJson));
    }

    #[test]
    fn rotated_token_overwrites_record_and_forget_removes_it() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("consent.json");
        store_atomic(&path, &record()).unwrap();
        let mut rotated = record();
        rotated.token = "new-rotated-secret".into();
        store_atomic(&path, &rotated).unwrap();
        assert_eq!(
            load(&path, "oled-main").unwrap().record().token,
            rotated.token
        );
        forget(&path).unwrap();
        assert!(matches!(
            load(&path, "oled-main"),
            Err(ConsentError::NotFound)
        ));
    }

    #[test]
    fn bound_consent_is_send_sync_static_and_not_debug() {
        fn assert_send_sync_static<T: Send + Sync + 'static>() {}
        assert_send_sync_static::<BoundConsent>();
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_sets_private_parent_and_file_modes() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let path = dir.path().join("state").join("consent.json");
        store_atomic(&path, &record()).unwrap();
        assert_eq!(
            fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn load_rejects_insecure_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let path = dir.path().join("consent.json");
        store_atomic(&path, &record()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            load(&path, "oled-main"),
            Err(ConsentError::InsecurePermissions)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn rename_failure_does_not_partially_write_target() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("consent.json");
        fs::create_dir(&path).unwrap();
        let error = store_atomic(&path, &record()).err().unwrap();
        assert!(matches!(error, ConsentError::Io(_)));
        assert!(path.is_dir());
        assert!(!path.join("token-rotated-secret").exists());
    }
}
use std::fmt;
use std::io::Write as _;
use std::path::Path;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// The persisted grant needed to reattach a portal capture stream.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct ConsentRecord {
    pub token: String,
    pub sampled_display: String,
    #[serde(with = "timestamp_serde")]
    pub granted_at: OffsetDateTime,
    pub portal_persistent_ids: Vec<String>,
    pub granted_width: u32,
    pub granted_height: u32,
}

impl dormant_doctor::ConsentSecrets for ConsentRecord {
    fn token(&self) -> &str {
        &self.token
    }

    fn persistent_ids(&self) -> &[String] {
        &self.portal_persistent_ids
    }
}

/// An owned record safe to pass between the sampler's async tasks.
#[derive(Clone)]
pub struct BoundConsent {
    record: ConsentRecord,
}

impl BoundConsent {
    /// Borrow the record for the capture boundary.
    #[must_use]
    pub fn as_binding(&self) -> crate::active_sampler::ConsentBinding<'_> {
        crate::active_sampler::ConsentBinding {
            token: &self.record.token,
            sampled_display: &self.record.sampled_display,
            portal_persistent_ids: &self.record.portal_persistent_ids,
            granted_width: self.record.granted_width,
            granted_height: self.record.granted_height,
        }
    }

    /// Access the validated record without exposing it through formatting.
    #[must_use]
    pub fn record(&self) -> &ConsentRecord {
        &self.record
    }
}

/// Failure while loading or changing a consent record.
pub enum ConsentError {
    NotFound,
    InvalidJson,
    DisplayChanged,
    InsecurePermissions,
    Io(std::io::Error),
}

impl fmt::Debug for ConsentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => f.write_str("NotFound"),
            Self::InvalidJson => f.write_str("InvalidJson"),
            Self::DisplayChanged => f.write_str("DisplayChanged"),
            Self::InsecurePermissions => f.write_str("InsecurePermissions"),
            Self::Io(error) => f.debug_tuple("Io").field(error).finish(),
        }
    }
}

impl fmt::Display for ConsentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => f.write_str("consent record not found"),
            Self::InvalidJson => f.write_str("invalid consent record JSON"),
            Self::DisplayChanged => {
                f.write_str(crate::active_sampler::WEAR_SAMPLING_DISPLAY_CHANGED)
            }
            Self::InsecurePermissions => f.write_str("insecure consent record permissions"),
            Self::Io(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for ConsentError {}

/// Read, parse, permission-check, and bind a saved grant to its configured display.
///
/// # Errors
///
/// Returns [`ConsentError::NotFound`] when no record exists, or another
/// [`ConsentError`] variant when the record is malformed, insecure, or bound to
/// a different display.
pub fn load(path: &Path, configured_display: &str) -> Result<BoundConsent, ConsentError> {
    let metadata = std::fs::metadata(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            ConsentError::NotFound
        } else {
            ConsentError::Io(error)
        }
    })?;
    check_permissions(path, &metadata)?;
    let raw = std::fs::read_to_string(path).map_err(ConsentError::Io)?;
    let record =
        serde_json::from_str::<ConsentRecord>(&raw).map_err(|_| ConsentError::InvalidJson)?;
    if record.sampled_display != configured_display {
        return Err(ConsentError::DisplayChanged);
    }
    Ok(BoundConsent { record })
}

/// Persist a grant without exposing secret bytes through a partially written target.
///
/// # Errors
///
/// Returns [`ConsentError::InvalidJson`] if serialization fails, or
/// [`ConsentError::Io`] when the private atomic write cannot complete.
pub fn store_atomic(path: &Path, record: &ConsentRecord) -> Result<(), ConsentError> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir).map_err(ConsentError::Io)?;
    set_mode(dir, 0o700)?;
    let serialized = serde_json::to_vec_pretty(record).map_err(|_| ConsentError::InvalidJson)?;
    let tmp_path = dir.join(format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("consent")
    ));

    let write_result = (|| -> Result<(), ConsentError> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp_path).map_err(ConsentError::Io)?;
        set_mode(&tmp_path, 0o600)?;
        file.write_all(&serialized).map_err(ConsentError::Io)?;
        file.flush().map_err(ConsentError::Io)?;
        file.sync_all().map_err(ConsentError::Io)?;
        Ok(())
    })();

    match write_result {
        Ok(()) => std::fs::rename(&tmp_path, path).map_err(ConsentError::Io),
        Err(error) => {
            let _ = std::fs::remove_file(&tmp_path);
            Err(error)
        }
    }
}

/// Remove a saved grant; absence is already the desired state.
///
/// # Errors
///
/// Returns [`ConsentError::Io`] when the record cannot be removed.
pub fn forget(path: &Path) -> Result<(), ConsentError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(ConsentError::Io(error)),
    }
}

fn check_permissions(path: &Path, metadata: &std::fs::Metadata) -> Result<(), ConsentError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let parent_mode = std::fs::metadata(path.parent().unwrap_or_else(|| Path::new(".")))
            .map_err(ConsentError::Io)?
            .permissions()
            .mode()
            & 0o777;
        if parent_mode != 0o700 || metadata.permissions().mode() & 0o777 != 0o600 {
            return Err(ConsentError::InsecurePermissions);
        }
    }
    Ok(())
}

fn set_mode(path: &Path, mode: u32) -> Result<(), ConsentError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .map_err(ConsentError::Io)?;
    }
    Ok(())
}

mod timestamp_serde {
    use super::OffsetDateTime;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(value: &OffsetDateTime, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_i64(value.unix_timestamp())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<OffsetDateTime, D::Error>
    where
        D: Deserializer<'de>,
    {
        OffsetDateTime::from_unix_timestamp(i64::deserialize(deserializer)?)
            .map_err(serde::de::Error::custom)
    }
}
