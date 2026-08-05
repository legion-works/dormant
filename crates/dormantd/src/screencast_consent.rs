//! Persistent portal consent and its display binding.

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};
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
            stream_position: None,
            compositor_output: None,
        }
    }

    #[test]
    fn round_trip_preserves_grant_dimensions_and_binding() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("consent.json");
        let expected = record();
        store_atomic(&path, &expected).unwrap();
        let loaded = load(&path, "oled-main", None).unwrap();
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
        let error = load(&path, "other-display", None).err().unwrap();
        assert!(matches!(error, ConsentError::DisplayChanged));
        assert_eq!(error.to_string(), "wear_sampling_display_changed");
    }

    #[test]
    fn corrupt_and_absent_records_are_distinct_errors() {
        let dir = tempdir().unwrap();
        let absent = load(&dir.path().join("missing.json"), "oled-main", None)
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
        let corrupt = load(&path, "oled-main", None).err().unwrap();
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
            load(&path, "oled-main", None).unwrap().record().token,
            rotated.token
        );
        forget(&path).unwrap();
        assert!(matches!(
            load(&path, "oled-main", None),
            Err(ConsentError::NotFound)
        ));
    }

    #[test]
    fn bound_consent_is_send_sync_static() {
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
            load(&path, "oled-main", None),
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

    #[test]
    fn consent_path_derives_per_display_filename() {
        // Per-display records are named after the sanitized display id
        // so two configured displays get two separate on-disk files.
        // Unsanitized input (mixed case, punctuation) is exercised here
        // so the test only holds when the sanitizer is wired.
        assert_eq!(
            consent_path(Path::new("/var/lib/state"), "Desk"),
            PathBuf::from("/var/lib/state/screencast-consent-desk.json"),
        );
        assert_eq!(
            consent_path(Path::new("/var/lib/state"), "tv/1"),
            PathBuf::from("/var/lib/state/screencast-consent-tv-1.json"),
        );
    }

    #[test]
    fn consent_path_sanitizes_unsafe_characters() {
        // Any character outside [a-z0-9._-] must collapse to '-', and the
        // id is bounded to 64 chars to keep filesystem paths bounded.
        let long = "x".repeat(120);
        let sanitized = consent_path(Path::new("/state"), &long);
        let name = sanitized.file_name().unwrap().to_str().unwrap();
        assert!(
            name.len() <= "screencast-consent-".len() + 64 + ".json".len(),
            "filename must stay bounded, got: {name}"
        );
        let ugly = consent_path(Path::new("/state"), "Desk/Layout!");
        assert_eq!(
            ugly,
            PathBuf::from("/state/screencast-consent-desk-layout-.json"),
        );
    }

    #[test]
    fn two_consent_records_store_and_load_without_overwrite_or_cross_binding() {
        // Two consent records at per-display paths must remain
        // independent: storing one does not overwrite the other, and
        // loading each against its own display id binds cleanly while
        // loading each against the OTHER id fails DisplayChanged. Use
        // distinct display ids that don't sanitize to the same key
        // (collision is rejected at config parse; here we exercise the
        // store/load path with names that differ after sanitization).
        let dir = tempdir().unwrap();
        let path_desk = consent_path(dir.path(), "desk");
        let path_tv = consent_path(dir.path(), "tv");

        let mut desk_record = record();
        desk_record.sampled_display = "desk".into();
        desk_record.token = "desk-token".into();
        store_atomic(&path_desk, &desk_record).unwrap();

        let mut tv_record = record();
        tv_record.sampled_display = "tv".into();
        tv_record.token = "tv-token".into();
        store_atomic(&path_tv, &tv_record).unwrap();

        let loaded_desk = load(&path_desk, "desk", None).unwrap();
        assert_eq!(loaded_desk.record().token, "desk-token");
        let loaded_tv = load(&path_tv, "tv", None).unwrap();
        assert_eq!(loaded_tv.record().token, "tv-token");

        // Cross-binding fails: loading the desk record bound to "tv" must
        // surface DisplayChanged, never silently succeed.
        assert!(matches!(
            load(&path_desk, "tv", None),
            Err(ConsentError::DisplayChanged)
        ));
        assert!(matches!(
            load(&path_tv, "desk", None),
            Err(ConsentError::DisplayChanged)
        ));
        // The two on-disk files have distinct names after sanitization,
        // so a store-then-store sequence leaves both readable.
        assert!(path_desk.exists());
        assert!(path_tv.exists());
        assert_ne!(path_desk, path_tv);
    }

    #[test]
    fn legacy_consent_path_is_a_one_time_source_for_legacy_display() {
        // The pre-multi-display record at screencast-consent.json stays
        // readable for the legacy-selected display; it must never
        // re-bind to a different display id.
        let dir = tempdir().unwrap();
        let legacy_path = dir.path().join("screencast-consent.json");
        store_atomic(&legacy_path, &record()).unwrap();
        assert_eq!(
            legacy_consent_path(dir.path()),
            legacy_path,
            "legacy path must be the un-suffixed screencast-consent.json",
        );
        let loaded = load(&legacy_path, "oled-main", None).unwrap();
        assert_eq!(loaded.record().sampled_display, "oled-main");
        assert!(matches!(
            load(&legacy_path, "other-display", None),
            Err(ConsentError::DisplayChanged)
        ));
    }

    #[test]
    fn legacy_json_without_position_or_output_deserializes() {
        // Records written before the position/output fields landed must
        // continue to load: both new fields default to None so the
        // legacy on-disk JSON does not need a migration.
        let dir = tempdir().unwrap();
        let path = dir.path().join("consent.json");
        let legacy_json = r#"{
            "token": "saved-token",
            "sampled_display": "oled-main",
            "granted_at": 1754000000,
            "portal_persistent_ids": ["persistent-portal-id"],
            "granted_width": 3840,
            "granted_height": 2160
        }"#;
        fs::write(&path, legacy_json).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let loaded = load(&path, "oled-main", None).unwrap();
        assert_eq!(loaded.record().stream_position, None);
        assert_eq!(loaded.record().compositor_output, None);
    }

    #[test]
    fn round_trip_preserves_position_and_compositor_output() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("consent.json");
        let mut expected = record();
        expected.stream_position = Some((0, 0));
        expected.compositor_output = Some("HDMI-A-1".to_owned());
        store_atomic(&path, &expected).unwrap();
        let loaded = load(&path, "oled-main", Some("HDMI-A-1")).unwrap();
        assert_eq!(loaded.record().stream_position, Some((0, 0)));
        assert_eq!(
            loaded.record().compositor_output.as_deref(),
            Some("HDMI-A-1")
        );
        let binding = loaded.as_binding();
        assert_eq!(binding.stream_position, Some((0, 0)));
    }

    #[test]
    fn compositor_output_drift_invalidates_with_existing_literal() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("consent.json");
        let mut stored = record();
        stored.compositor_output = Some("HDMI-A-1".to_owned());
        store_atomic(&path, &stored).unwrap();
        let error = load(&path, "oled-main", Some("HDMI-A-2")).err().unwrap();
        assert!(matches!(error, ConsentError::DisplayChanged));
        assert_eq!(error.to_string(), "wear_sampling_display_changed");
    }

    #[test]
    fn configured_compositor_output_none_accepts_legacy_record() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("consent.json");
        store_atomic(&path, &record()).unwrap();
        let loaded = load(&path, "oled-main", None).unwrap();
        assert_eq!(loaded.record().sampled_display, "oled-main");
    }
}
use std::fmt;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Filename prefix for per-display consent records.
const CONSENT_FILENAME_PREFIX: &str = "screencast-consent-";
/// Filename suffix for all consent records.
const CONSENT_FILENAME_SUFFIX: &str = ".json";
/// Legacy un-suffixed filename retained as the one-time source for the
/// legacy-selected display after a multi-display migration.
const LEGACY_CONSENT_FILENAME: &str = "screencast-consent.json";

/// Build the on-disk path for one display's consent record.
///
/// The id is sanitized to a stable filesystem-safe key (see
/// [`dormant_core::wear::sanitize_identity_key`]) so two display ids
/// that differ only in punctuation collapse to the same path — that
/// collision is rejected at config parse rather than discovered at
/// sampler spawn. The legacy `screencast-consent.json` is preserved
/// by [`legacy_consent_path`] for the legacy-selected display only.
#[must_use]
pub fn consent_path(state_dir: &Path, display_id: &str) -> PathBuf {
    let sanitized = dormant_core::wear::sanitize_identity_key(display_id);
    state_dir.join(format!(
        "{CONSENT_FILENAME_PREFIX}{sanitized}{CONSENT_FILENAME_SUFFIX}"
    ))
}

/// The legacy un-suffixed consent path, retained as a one-time source
/// for the legacy-selected display when migrating to the per-display
/// layout.
#[must_use]
pub fn legacy_consent_path(state_dir: &Path) -> PathBuf {
    state_dir.join(LEGACY_CONSENT_FILENAME)
}

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
    /// Logical `(x, y)` of the portal stream at grant time. The compositor
    /// reports this as the top-left of the output in its own coordinate
    /// space; two same-resolution 4K monitors stay distinguishable only
    /// while this signal is present (older compositors omit it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_position: Option<(i32, i32)>,
    /// Compositor output name the operator bound this grant to. A
    /// subsequent reconfigure that moves the sampler to a different
    /// output invalidates the record so the daemon does not silently
    /// relabel a mirror seat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compositor_output: Option<String>,
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
            stream_position: self.record.stream_position,
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
/// `configured_compositor_output` is the compositor output the
/// sampler is currently keyed to. A stored record that pinned a different
/// output is rejected with [`ConsentError::DisplayChanged`] so a reconfigure
/// that moves the sampler to a different monitor does not silently re-bind
/// the old grant. `None` here means the sampler has no configured output
/// (legacy config), in which case the field is not consulted — the record
/// itself may still carry one for future runs.
///
/// # Errors
///
/// Returns [`ConsentError::NotFound`] when no record exists, or another
/// [`ConsentError`] variant when the record is malformed, insecure, or bound to
/// a different display.
pub fn load(
    path: &Path,
    configured_display: &str,
    configured_compositor_output: Option<&str>,
) -> Result<BoundConsent, ConsentError> {
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
    if let Some(configured) = configured_compositor_output
        && record.compositor_output.as_deref() != Some(configured)
    {
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
        ".{}.tmp.{}.{}",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("consent"),
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed),
    ));

    let write_result = (|| -> Result<(), ConsentError> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        // Portable builds retain the JSON schema and test surface. Runtime
        // portal capture is Linux-only; non-Unix targets lack O_NOFOLLOW.
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
