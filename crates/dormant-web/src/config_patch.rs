//! Config-patch security boundary (spec §5).
//!
//! Pure module — no I/O, no routes, no HTTP types.  A patch is either a
//! [`Set`](Patch::Set) or [`Remove`](Patch::Remove) on a TOML-key path.
//! The canonical pipeline in [`check_patches`] rejects unsafe patches
//! before they ever touch a parsed document; [`apply_patches`] applies
//! a pre-checked batch to a [`toml_edit::DocumentMut`].

use serde::Deserialize;

use dormant_core::config::is_known_config_path;

#[cfg(test)]
use serde_json::json;

// ── Types ──────────────────────────────────────────────────────────────────

/// A single config patch.
///
/// Serde auto-discriminates on the `op` field.  A `Set` carries a JSON
/// value; it replaces the target wholesale (arrays replace whole;
/// arrays-of-tables match the original form).  A `Remove` deletes the
/// leaf key from its parent table.  `CreateEntity`/`DeleteEntity` mint or
/// remove a whole entity table under one of the four CRUD collections
/// (spec §3/§4) — `Set`/`Remove` can never do this (container-Set stays
/// denied, invariant #2).
///
/// `Set`/`Remove` rely on `rename_all = "lowercase"` (`set`/`remove`,
/// pre-existing wire compat).  `CreateEntity`/`DeleteEntity` need an
/// EXPLICIT `#[serde(rename = "...")]` — `rename_all = "lowercase"`
/// would otherwise produce `"createentity"`/`"deleteentity"` (it lowercases
/// the whole variant name, it does not `snake_case` it), not the
/// `"create_entity"`/`"delete_entity"` ops the spec requires (§3). Pinned
/// by `deserialize_create_entity_patch`/`deserialize_delete_entity_patch`.
#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum Patch {
    Set {
        path: Vec<String>,
        value: serde_json::Value,
    },
    Remove {
        path: Vec<String>,
    },
    /// Create a NEW entity table under a collection.  `value` is the
    /// entity's initial contents (an object including its discriminator,
    /// e.g. `type` for sensors).
    #[serde(rename = "create_entity")]
    CreateEntity {
        collection: String,
        id: String,
        value: serde_json::Value,
    },
    /// Delete a whole entity table.  Cross-reference orphaning is caught
    /// by the daemon-identical validate step at apply time, not here.
    #[serde(rename = "delete_entity")]
    DeleteEntity {
        collection: String,
        id: String,
    },
}

/// Reasons a patch is rejected before application.
#[derive(Debug, Clone, PartialEq)]
pub enum PatchError {
    /// Hygiene / unknown-key rejection.
    PathDenied(String),
    /// The path intersects a redacted leaf.
    RedactedPath(String),
    /// An entity id or index does not exist in the current document.
    EntityUnknown(String),
    /// A JSON value could not be converted to TOML (e.g. `null`).
    ValueRejected(String),
    /// `CreateEntity` targeted an id that already exists in the collection.
    EntityExists(String),
}

// ── Canonical pipeline (spec §5, EXACT order) ──────────────────────────────
//
// hygiene → is_known_config_path → editable-subset → redacted-reject
// (prefix-aware BOTH directions) → entity-existence.
//
// Order matters for observability: early gates use the most specific error
// so the caller can distinguish "you have a typo" from "you may not touch
// this value."

/// Run the full check pipeline against every patch in `patches`.
///
/// On first rejection, returns the corresponding [`PatchError`].
pub fn check_patches(
    patches: &[Patch],
    current: &toml_edit::DocumentMut,
    redacted: &[Vec<String>],
) -> Result<(), PatchError> {
    for patch in patches {
        match patch {
            // CreateEntity/DeleteEntity route to their own gates (spec §4,
            // C-M3) BEFORE the Set/Remove 5-stage pipeline — the
            // container-Set rejection below would deny the table shape a
            // create needs, and entity-existence would reject the
            // not-yet-existing id.
            Patch::CreateEntity {
                collection,
                id,
                value,
            } => {
                check_create_entity(collection, id, value, current)?;
            }
            Patch::DeleteEntity { collection, id } => {
                check_delete_entity(collection, id, current)?;
            }
            Patch::Set { path, .. } | Patch::Remove { path } => {
                // 1. Hygiene — reject dangerous segments.
                check_hygiene(path)?;

                // 2. Known-path structural check.
                let path_strs: Vec<&str> = path.iter().map(String::as_str).collect();
                if !is_known_config_path(&path_strs) {
                    return Err(PatchError::PathDenied(format!(
                        "unknown config path: {}",
                        path_strs.join(".")
                    )));
                }

                // 3. Editable-subset — locked leaves, container-Set
                //    rejection, optional-only removal, recursive payload
                //    check.
                check_editable_subset(patch, current)?;

                // 4. Redacted-reject — prefix-aware in both directions.
                check_redacted(&path_strs, redacted)?;

                // 5. Entity-existence — collection ids + array-of-tables
                //    indices.
                check_entity_existence(&path_strs, current)?;
            }
        }
    }
    Ok(())
}

// ── Pipeline step 1: hygiene ───────────────────────────────────────────────

/// Keys whose removal via [`Patch::Remove`] is explicitly permitted.
///
/// These are optional config keys that carry no security-sensitive data.
/// All are leaf keys in the known-config tree.
const REMOVABLE_KEYS: &[&str] = &[
    "blank_mode",
    "degraded_mode",
    "dwell",
    "order",
    "image_duration",
    "scale_mode",
    "transition",
    "transition_duration",
    "hold_time",
    "stale_timeout",
    "ddc_display",
    "output",
    "wol_mac",
    "host",
];

/// v1-locked leaves — may never be written or removed through the patch API.
const LOCKED_LEAVES: &[&str] = &["type", "blank_data", "wake_data"];

const MAX_PATH_DEPTH: usize = 8;

fn check_hygiene(path: &[String]) -> Result<(), PatchError> {
    if path.len() > MAX_PATH_DEPTH {
        return Err(PatchError::PathDenied(format!(
            "path depth {} exceeds maximum {}",
            path.len(),
            MAX_PATH_DEPTH
        )));
    }
    for seg in path {
        if seg.is_empty() {
            return Err(PatchError::PathDenied("path contains empty segment".into()));
        }
        if seg == "." {
            return Err(PatchError::PathDenied(format!(
                "path segment '.' is reserved: '{seg}'"
            )));
        }
        if seg == ".." {
            return Err(PatchError::PathDenied(format!(
                "path segment '..' is reserved: '{seg}'"
            )));
        }
        if seg.contains('\0') {
            return Err(PatchError::PathDenied(format!(
                "path segment contains NUL byte: '{seg}'"
            )));
        }
    }
    Ok(())
}

// ── Pipeline step 3: editable-subset ───────────────────────────────────────

fn check_editable_subset(p: &Patch, current: &toml_edit::DocumentMut) -> Result<(), PatchError> {
    let (path, is_remove) = match p {
        Patch::Set { path, .. } => (path, false),
        Patch::Remove { path } => (path, true),
        Patch::CreateEntity { .. } | Patch::DeleteEntity { .. } => {
            unreachable!("check_patches only calls check_editable_subset from the Set|Remove arm")
        }
    };

    // Locked leaves — never editable via patch API.
    if let Some(leaf) = path.last()
        && LOCKED_LEAVES.contains(&leaf.as_str())
    {
        return Err(PatchError::PathDenied(format!(
            "'{leaf}' is a locked config key — not editable via patch API"
        )));
    }

    // Container-Set rejection — a Set on a table-level path would
    // replace the whole sub-tree, smuggling locked leaves like `type`.
    // A Set on a table-level path (e.g. ["sensors"], ["displays","tv"]) would
    // replace the whole sub-tree, smuggling locked leaves like `type`.
    // Allowed Sets target VALUE leaves or whole ARRAYS only.
    // Tables, collection-level keys, and AOT entry indices are containers.
    if !is_remove {
        let path_strs: Vec<&str> = path.iter().map(String::as_str).collect();
        if is_table_container(current, &path_strs) {
            return Err(PatchError::PathDenied(
                "container paths are not patchable; set leaf values or whole arrays".into(),
            ));
        }
    }

    // Remove is additionally restricted to explicitly optional keys.
    if is_remove
        && let Some(leaf) = path.last()
        && !REMOVABLE_KEYS.contains(&leaf.as_str())
    {
        return Err(PatchError::PathDenied(format!(
            "'{leaf}' is a required key — not removable"
        )));
    }

    // Recursive payload check (defense-in-depth for container-Set bypass).
    // Walk the JSON value tree and reject any object key that is a locked
    // leaf name.  This catches smuggling via whole-array payloads (e.g.
    // setting ladder with an entry that contains `"type"`).
    // source/ladder entries legitimately contain `kind`/`path`/`urls` etc.;
    // only `type`, `blank_data`, and `wake_data` are locked.
    if let Patch::Set { value, .. } = p {
        check_payload_for_locked_leaves(value)?;
    }

    Ok(())
}

/// Returns `true` when the item at `path` in the document is a Table
/// (a structural container, not a leaf value or array).
///
/// AOT keys (`ladder`, `source`) return `false` — they are arrays, not
/// containers.  Paths whose LAST segment is a digit (e.g. `ladder.0`,
/// `source.0`) are AOT entry indices — those are always Tables.
/// Paths with internal digit segments (e.g. `source.0.order`) are leaf
/// edits inside AOT entries — the target is the final non-digit key.
fn is_table_container(doc: &toml_edit::DocumentMut, path: &[&str]) -> bool {
    if path.is_empty() {
        // Root is always a table.
        return true;
    }

    // Paths whose last segment is a digit are AOT entry indices.
    if let Some(last) = path.last()
        && is_digit_segment(last)
    {
        return true;
    }

    // AOT keys (ladder, source) at any depth are arrays, not tables.
    if let Some(last) = path.last()
        && (*last == "ladder" || *last == "source")
    {
        return false;
    }

    // Walk the document to the target and check the item type.
    // Internal digit segments (inside AOT entries) cannot be navigated
    // via &dyn TableLike; when we encounter one we know the path is inside
    // an AOT entry and the eventual target is a leaf value, not a container.
    let mut table_like: &dyn toml_edit::TableLike = doc.as_table();
    for (i, seg) in path.iter().enumerate() {
        let is_last = i == path.len() - 1;

        // If we hit a digit mid-path, the target is deeper inside an AOT
        // entry — it is a leaf, not a container.
        if is_digit_segment(seg) && !is_last {
            return false;
        }

        match table_like.get(seg) {
            Some(item) if is_last => return item.is_table(),
            Some(item) => match item.as_table_like() {
                Some(t) => table_like = t,
                None => return false,
            },
            None => return false,
        }
    }
    false
}

/// Walk a JSON value tree and reject any object key that matches a
/// [`LOCKED_LEAVES`] entry.  Only fires for [`Patch::Set`].
fn check_payload_for_locked_leaves(value: &serde_json::Value) -> Result<(), PatchError> {
    match value {
        serde_json::Value::Object(obj) => {
            for (k, v) in obj {
                if LOCKED_LEAVES.contains(&k.as_str()) {
                    return Err(PatchError::PathDenied(format!(
                        "locked key '{k}' found in replacement payload"
                    )));
                }
                check_payload_for_locked_leaves(v)?;
            }
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                check_payload_for_locked_leaves(item)?;
            }
        }
        _ => {}
    }
    Ok(())
}

// ── Pipeline step 4: redacted-reject ───────────────────────────────────────

/// Reject when a patch path equals, is a descendant of, or is an ancestor of
/// any redacted path.  Both directions use a plain segment-prefix match.
///
/// ## Known deviation: inline-array element leaves
///
/// Inline array element indices (e.g. `source.0.urls.1`) are NOT recognised
/// by [`is_known_config_path`] — T1 scopes numeric-index handling to the
/// `ladder` / `source` `ArrayOfTables` keys only (see
/// `dormant_core::config::validate::is_array_of_tables_parent`).
/// Such paths therefore receive `PathDenied` before reaching this check.
/// Protection is not lost: the ancestor rule rejects patches on the
/// containing array (`...source`), and the descendant rule rejects leaf
/// edits under any path that IS a known descendant of the redacted path.
fn check_redacted(path: &[&str], redacted: &[Vec<String>]) -> Result<(), PatchError> {
    for r in redacted {
        // Defensive: skip empty redacted entries (mirrors TS isLocked guard).
        if r.is_empty() {
            continue;
        }
        let r_strs: Vec<&str> = r.iter().map(String::as_str).collect();

        // Exact match or descendant: patch-path starts with redacted.
        if path.len() >= r_strs.len() && path[..r_strs.len()] == r_strs[..] {
            return Err(PatchError::RedactedPath(format!(
                "path {} is under redacted prefix {}",
                path.join("."),
                r_strs.join(".")
            )));
        }

        // Ancestor: redacted starts with patch-path (patching a parent
        // would replace/remove a redacted descendant).
        if r_strs.len() >= path.len() && r_strs[..path.len()] == path[..] {
            return Err(PatchError::RedactedPath(format!(
                "path {} is an ancestor of redacted path {}",
                path.join("."),
                r_strs.join(".")
            )));
        }
    }
    Ok(())
}

// ── Pipeline step 5: entity-existence ──────────────────────────────────────

fn check_entity_existence(
    path: &[&str],
    current: &toml_edit::DocumentMut,
) -> Result<(), PatchError> {
    // Check collection-level entity ids.
    if let Some(collection) = check_collection_entity(path, current) {
        return Err(PatchError::EntityUnknown(format!(
            "entity '{collection}' not found in config"
        )));
    }

    // Check array-of-tables index validity (source.<idx>, ladder.<idx>).
    check_aot_indices(path, current)?;

    Ok(())
}

/// Returns the unknown entity id if a collection-level entity does not exist.
fn check_collection_entity(path: &[&str], current: &toml_edit::DocumentMut) -> Option<String> {
    const COLLECTIONS: &[&str] = &["sensors", "zones", "displays", "rules"];

    if path.len() < 2 {
        return None;
    }

    let root_key = path[0];
    if !COLLECTIONS.contains(&root_key) {
        return None;
    }

    let entity_id = path[1];

    if let Some(collection_table) = current.get(root_key).and_then(|item| item.as_table()) {
        if !collection_table.contains_key(entity_id) {
            return Some(entity_id.to_string());
        }
    } else {
        return Some(entity_id.to_string());
    }

    None
}

/// Check that numeric indices in source/ladder paths are within bounds.
fn check_aot_indices(path: &[&str], current: &toml_edit::DocumentMut) -> Result<(), PatchError> {
    // Walk the path looking for "source" or "ladder" followed by a digit.
    for i in 0..path.len().saturating_sub(1) {
        if (path[i] == "source" || path[i] == "ladder") && is_digit_segment(path[i + 1]) {
            let idx: usize = path[i + 1]
                .parse()
                .map_err(|_| PatchError::PathDenied("invalid index".into()))?;
            if !aot_index_exists(current, &path[..=i], idx) {
                return Err(PatchError::EntityUnknown(format!(
                    "index {idx} in {} out of range",
                    path[..=i].join(".")
                )));
            }
        }
    }
    Ok(())
}

fn is_digit_segment(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_digit())
}

/// Walk `current` to the array-of-tables at `path_to_aot` and check whether
/// `index` is in bounds.  `path_to_aot` INCLUDES the AOT key as its last
/// segment (e.g. `["displays","tv","screensaver","source"]`).
fn aot_index_exists(current: &toml_edit::DocumentMut, path_to_aot: &[&str], index: usize) -> bool {
    // Walk through the path to reach the AOT key.
    let mut item: Option<&toml_edit::Item> = None;
    let mut table_like: Option<&dyn toml_edit::TableLike> = Some(current.as_table());

    for seg in path_to_aot {
        item = table_like.and_then(|t| t.get(seg));
        table_like = item.and_then(|i| i.as_table_like());
    }

    // `item` should now be the ArrayOfTables item.
    item.and_then(|i| i.as_array_of_tables())
        .is_some_and(|aot| index < aot.len())
}

// ── CreateEntity / DeleteEntity gates (spec §4/§5) ──────────────────────────

/// The four collections that support entity create/delete.  Deliberately
/// closed — `CREATABLE_FIELDS`'s key-space is exactly this set (invariant
/// 2c) so a future `daemon`/`web`/`audio`-style section can never become
/// reachable from `CreateEntity`/`DeleteEntity` by construction.
const CRUD_COLLECTIONS: &[&str] = &["sensors", "zones", "displays", "rules"];

/// Per-collection closed field allowlists for `CreateEntity` payloads
/// (spec §4). The discriminator field (`type` for sensors) is included
/// here — step 4 of `check_create_entity` treats it like any other
/// allowed top-level key — but is exempted from the recursive
/// locked-leaf walk in `check_create_entity`'s own split-contract step
/// (B-M1), never from this membership check.
///
/// `blank_command`/`wake_command` are DELIBERATELY OMITTED from
/// `displays` (cold-gate M3 Must-1) — they are daemon-executed `sh -c`
/// commands (`schema.rs` `DisplayConfig::blank_command`/`wake_command`);
/// a web-created display may not carry an arbitrary shell command in v1.
/// Pinned by `create_display_with_blank_command_denied`.
static CREATABLE_FIELDS: &[(&str, &[&str])] = &[
    (
        "sensors",
        &[
            "type",
            "kind",
            "hold_time",
            "stale_timeout",
            // mqtt
            "broker_url",
            "topic",
            "field",
            "payload_on",
            "payload_off",
            // ha
            "url",
            "entity",
            // usb-ld2410
            "port",
            "baud",
        ],
    ),
    (
        "zones",
        &["mode", "members", "unavailable_policy", "weights"],
    ),
    (
        "displays",
        &[
            "controllers",
            "host",
            "blank_mode",
            "output",
            "ddc_display",
            "wol_mac",
            "samsung_restore_backlight",
            "restore_brightness",
            "treat_unreachable_as_blanked",
            "command_timeout",
        ],
    ),
    (
        "rules",
        &[
            "zone",
            "displays",
            "grace_period",
            "inhibitors",
            "min_blank_time",
            "min_wake_time",
            "activity_idle_threshold",
            "activity_poll_interval",
            "wake_retries",
            "wake_retry_backoff",
            "wake_retry_interval",
        ],
    ),
];

/// Look up the creatable-field allowlist for `collection`, or `None` when
/// `collection` isn't one of [`CRUD_COLLECTIONS`].
fn creatable_fields_for(collection: &str) -> Option<&'static [&'static str]> {
    CREATABLE_FIELDS
        .iter()
        .find(|(c, _)| *c == collection)
        .map(|(_, fields)| *fields)
}

/// Every entity id string that ANY gate in this file — or in
/// `dormant_core::config::validate`'s `is_known_config_path` internals —
/// special-cases by literal name (spec §5, R2-M1/B-M2/B-S2, the
/// load-bearing security fix).  An entity created with one of these ids
/// would defeat a gate that assumes the name never collides with a real
/// entity id (e.g. a `weights`-named sensor makes `is_known_config_path`
/// accept an arbitrary unchecked key beneath it — `weights_named_entity_cannot_smuggle_unknown_key`).
///
/// Superset of (verified at build time by
/// `reserved_ids_superset_of_all_special_cases`):
///   - `dormant_core::config::STRUCTURAL_RESERVED_NAMES` — the REAL,
///     re-exported source of truth (`weights`, `source`, `ladder`,
///     `blank_data`, `wake_data`) driving `is_weights_level`,
///     `is_array_of_tables_parent`, `is_passthrough_data_key`.
///   - this module's [`LOCKED_LEAVES`] (`type`, `blank_data`, `wake_data`
///     — the latter two already members of `STRUCTURAL_RESERVED_NAMES`
///     above; listed again here as a flat literal set, not a second
///     union — see the cross-check test's own comment for why a literal
///     `∪ {"source","ladder"}` union is NOT re-added).
///   - this module's [`REMOVABLE_KEYS`] (14 optional-leaf names).
static RESERVED_ENTITY_IDS: &[&str] = &[
    // STRUCTURAL_RESERVED_NAMES (dormant-core, re-exported)
    "weights",
    "source",
    "ladder",
    "blank_data",
    "wake_data",
    // LOCKED_LEAVES (only "type" is new here — blank_data/wake_data above)
    "type",
    // REMOVABLE_KEYS
    "blank_mode",
    "degraded_mode",
    "dwell",
    "order",
    "image_duration",
    "scale_mode",
    "transition",
    "transition_duration",
    "hold_time",
    "stale_timeout",
    "ddc_display",
    "output",
    "wol_mac",
    "host",
];

/// Entity-id hygiene (spec §5, greenfield — no prior charset validation
/// existed anywhere for a dynamic TOML key).
///
/// Charset `[a-z0-9_-]`, first char `[a-z]`, length 1–64; the reserved-name
/// ban (`RESERVED_ENTITY_IDS`) applies unconditionally.
fn validate_entity_id(id: &str) -> Result<(), PatchError> {
    if id.is_empty() {
        return Err(PatchError::PathDenied("entity id must not be empty".into()));
    }
    if id.chars().count() > 64 {
        return Err(PatchError::PathDenied(format!(
            "entity id '{id}' exceeds the maximum length of 64"
        )));
    }
    let first_ok = id.chars().next().is_some_and(|c| c.is_ascii_lowercase());
    if !first_ok {
        return Err(PatchError::PathDenied(format!(
            "entity id '{id}' must start with a lowercase ASCII letter"
        )));
    }
    let charset_ok = id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if !charset_ok {
        return Err(PatchError::PathDenied(format!(
            "entity id '{id}' contains characters outside [a-z0-9_-]"
        )));
    }
    if RESERVED_ENTITY_IDS.contains(&id) {
        return Err(PatchError::PathDenied(format!(
            "entity id '{id}' is a reserved config key name"
        )));
    }
    Ok(())
}

/// Gate for [`Patch::CreateEntity`] (spec §4). `current` LAST — matches
/// `check_entity_existence`/`check_collection_entity`'s convention and the
/// spec's own §4 dispatch code (cold-gate deepseek M1).
fn check_create_entity(
    collection: &str,
    id: &str,
    value: &serde_json::Value,
    current: &toml_edit::DocumentMut,
) -> Result<(), PatchError> {
    // 1. Collection scope.
    if !CRUD_COLLECTIONS.contains(&collection) {
        return Err(PatchError::PathDenied(format!(
            "'{collection}' is not a CRUD collection"
        )));
    }

    // 2. Id hygiene (charset + reserved-name ban).
    validate_entity_id(id)?;

    // 3. Not already present.
    let already_exists = current
        .get(collection)
        .and_then(|item| item.as_table())
        .is_some_and(|tbl| tbl.contains_key(id));
    if already_exists {
        return Err(PatchError::EntityExists(format!(
            "entity '{id}' already exists in '{collection}'"
        )));
    }

    // 4. Per-collection creatable-field allowlist — closed enumeration,
    //    no "etc." (cold-gate M3 Must-1).
    let allowed = creatable_fields_for(collection).ok_or_else(|| {
        PatchError::PathDenied(format!(
            "no creatable-field allowlist defined for '{collection}'"
        ))
    })?;

    let obj = value
        .as_object()
        .ok_or_else(|| PatchError::PathDenied("create payload must be a JSON object".into()))?;

    for (key, field_value) in obj {
        if !allowed.contains(&key.as_str()) {
            return Err(PatchError::PathDenied(format!(
                "'{key}' is not a creatable field for '{collection}'"
            )));
        }

        // 5. Locked-leaf recursion, split contract (B-M1): the
        //    discriminator top-level key (`type`) is checked ONLY for
        //    allowlist membership above — never recursed through
        //    `check_payload_for_locked_leaves`, which would reject it (it
        //    IS a `LOCKED_LEAVES` member).  Every OTHER top-level field's
        //    VALUE is recursed through the unmodified shared check, so a
        //    locked leaf nested inside e.g. an object value still can't
        //    smuggle through.
        if key != "type" {
            check_payload_for_locked_leaves(field_value)?;
        }
    }

    // 6. Full shape is NOT re-validated here — the daemon-identical
    //    `validate()` at the temp-file step is the authoritative check
    //    (config_apply.rs).
    Ok(())
}

/// Gate for [`Patch::DeleteEntity`] (spec §4, R2-M2). Collection scope is
/// checked FIRST, identically to `check_create_entity` — without it,
/// `DeleteEntity{collection:"daemon", id:"web_allow_nonloopback"}` would
/// delete a security toggle: a valid-charset, existing, non-referential
/// key, routing around `REMOVABLE_KEYS`' deliberate curation.
fn check_delete_entity(
    collection: &str,
    id: &str,
    current: &toml_edit::DocumentMut,
) -> Result<(), PatchError> {
    // 1. Collection scope.
    if !CRUD_COLLECTIONS.contains(&collection) {
        return Err(PatchError::PathDenied(format!(
            "'{collection}' is not a CRUD collection"
        )));
    }

    // 2. Id hygiene.
    validate_entity_id(id)?;

    // 3. Existence.
    let exists = current
        .get(collection)
        .and_then(|item| item.as_table())
        .is_some_and(|tbl| tbl.contains_key(id));
    if !exists {
        return Err(PatchError::EntityUnknown(format!(
            "entity '{id}' not found in '{collection}'"
        )));
    }

    Ok(())
}

// ── Apply patches ──────────────────────────────────────────────────────────

/// Apply a batch of already-checked patches to `doc`.
///
/// # Panics
///
/// Will panic if a patch references a path that does not exist (callers MUST
/// run [`check_patches`] first).
pub fn apply_patches(
    doc: &mut toml_edit::DocumentMut,
    patches: &[Patch],
) -> Result<(), PatchError> {
    for patch in patches {
        match patch {
            Patch::CreateEntity {
                collection,
                id,
                value,
            } => apply_create_entity(doc, collection, id, value),
            Patch::DeleteEntity { collection, id } => apply_delete_entity(doc, collection, id),
            Patch::Set { path, value } => apply_set(doc, path, value)?,
            Patch::Remove { path } => apply_remove(doc, path)?,
        }
    }
    Ok(())
}

/// Apply a pre-checked [`Patch::CreateEntity`].
///
/// Mirrors `upsert_samsung_token`'s pattern (`config/mod.rs`), bypassing
/// `walk_table` entirely (it refuses collection-child creation, C-M2).
///
/// # Ordering (load-bearing, not just style — C-S round 3)
///
/// `doc[collection]` MUST be ensured as an explicit non-inline
/// [`toml_edit::Table`] BEFORE the entity is inserted into it.
/// `toml_edit`'s `Index`/`IndexMut` auto-vivifies a missing intermediate
/// into an `InlineTable`, not a `Table` — so indexing `doc[collection][id]`
/// directly (a one-liner) when `[collection]` doesn't yet exist (e.g. the
/// very first sensor in a config with no `[sensors]` section) would
/// corrupt the section into inline form.  This two-step order avoids it;
/// do not collapse it into a one-liner.
///
/// # Panics
///
/// Contract-panics if `doc[collection]` exists but is not a table — this
/// cannot happen for a caller that ran [`check_patches`] first (which only
/// ever reaches here for `CRUD_COLLECTIONS`, always tables on a valid
/// document).
fn apply_create_entity(
    doc: &mut toml_edit::DocumentMut,
    collection: &str,
    id: &str,
    value: &serde_json::Value,
) {
    // Step 1: ensure the collection table exists as an EXPLICIT Table —
    // strictly before any indexing into it.
    if doc.get(collection).is_none() {
        let mut tbl = toml_edit::Table::new();
        tbl.set_implicit(false);
        doc[collection] = toml_edit::Item::Table(tbl);
    }

    // Step 2: insert the new entity as its own explicit Table (renders as
    // `[collection.id]`, NOT an inline table).
    let entity_table = json_to_toml_table(value);
    let collection_table = doc
        .get_mut(collection)
        .and_then(|item| item.as_table_mut())
        .expect("collection table ensured explicit above");
    collection_table.insert(id, toml_edit::Item::Table(entity_table));
}

/// Apply a pre-checked [`Patch::DeleteEntity`].
///
/// # Panics
///
/// Will panic if `collection`/`id` does not exist — callers MUST run
/// [`check_patches`] first (same contract as [`apply_patches`]).
fn apply_delete_entity(doc: &mut toml_edit::DocumentMut, collection: &str, id: &str) {
    doc.get_mut(collection)
        .and_then(|item| item.as_table_mut())
        .expect("collection exists — check_delete_entity ran first")
        .remove(id);
}

fn apply_set(
    doc: &mut toml_edit::DocumentMut,
    path: &[String],
    value: &serde_json::Value,
) -> Result<(), PatchError> {
    let (parent_path, leaf) = path.split_at(path.len() - 1);
    let leaf_key = leaf[0].as_str();

    // Check whether the current value at this path is an ArrayOfTables.
    let is_aot = is_at_aot_key(doc, path);

    // Navigate to parent and modify in-place while preserving item-level
    // decorations (trailing comments).  toml_edit stores trailing comments
    // on the Item's decor (not the Key), so replacing an Item discards them.
    // We copy the old suffix onto the new Item to keep comments alive.
    let parent_table = parent_table_mut(doc, parent_path, true)?;

    // Snapshot the old item's trailing decoration before we mutate.
    // In toml_edit 0.25, decorations (including trailing comments) live on
    // the `Value`, not on the `Item`.  Get the suffix from the old value's
    // decor before replacing it.
    let old_suffix = parent_table
        .get(leaf_key)
        .and_then(|item| item.as_value())
        .and_then(|value| value.decor().suffix())
        .cloned();

    let new_item = if is_aot && value.is_array() {
        toml_edit::Item::ArrayOfTables(json_to_array_of_tables(value)?)
    } else {
        toml_edit::Item::Value(json_to_toml_value(value)?)
    };

    if let Some(item) = parent_table.get_mut(leaf_key) {
        *item = new_item;
        // Re-attach the old trailing comment to the new value.
        if let Some(suffix) = old_suffix
            && let Some(value) = item.as_value_mut()
        {
            value.decor_mut().set_suffix(suffix);
        }
    } else {
        parent_table[leaf_key] = new_item;
    }

    Ok(())
}

fn apply_remove(doc: &mut toml_edit::DocumentMut, path: &[String]) -> Result<(), PatchError> {
    let (parent_path, leaf) = path.split_at(path.len() - 1);
    let leaf_key = leaf[0].as_str();

    let parent_table = parent_table_mut(doc, parent_path, false)?;
    parent_table.remove(leaf_key);

    Ok(())
}

/// Check whether the item at `path` is an `ArrayOfTables`.
///
/// Only relevant for paths that END at a known AOT key (`source` / `ladder`)
/// without a trailing numeric index.  Paths containing a digit segment
/// are leaf edits inside AOT entries — those always return `false`.
fn is_at_aot_key(doc: &toml_edit::DocumentMut, path: &[String]) -> bool {
    if path.is_empty() {
        return false;
    }
    // Build &str slice and check.
    let path_strs: Vec<&str> = path.iter().map(String::as_str).collect();
    let mut table_like: &dyn toml_edit::TableLike = doc.as_table();

    let mut i = 0;
    while i < path_strs.len() {
        let seg = path_strs[i];

        // If we encounter a digit, this path has index segments —
        // we are NOT at an AOT key, we're inside one.
        if is_digit_segment(seg) {
            return false;
        }

        match table_like.get(seg) {
            Some(item) if i == path_strs.len() - 1 => {
                // Last segment — check if it's an AOT.
                return item.is_array_of_tables();
            }
            Some(item) => match item.as_table_like() {
                Some(t) => table_like = t,
                None => return false, // Hit a value before end of path
            },
            None => return false,
        }
        i += 1;
    }
    false
}

// ── Parent-table navigation (safe — no unsafe blocks) ──────────────────────

/// Navigate to the *parent* table of the leaf operation.
///
/// Handles two cases:
/// 1. `parent_path` has a numeric last segment → index into the preceding
///    `ArrayOfTables` and return the entry table.
/// 2. `parent_path` has a key last segment → walk to the containing table
///    and return the sub-table at that key.
///
/// When `create_missing` is true, missing intermediate non-collection tables
/// are created as implicit tables (no formatting damage on absent tables).
/// Dynamic entity IDs (children of `sensors`/`zones`/`displays`/`rules`) are
/// never created — entity existence is enforced by `check_patches` before
/// apply.
///
/// Uses safe per-segment re-borrow via chained `get_mut → as_table_mut` in a
/// single expression so the borrow checker can see the exclusive chain.
fn parent_table_mut<'a>(
    doc: &'a mut toml_edit::DocumentMut,
    parent_path: &[String],
    create_missing: bool,
) -> Result<&'a mut toml_edit::Table, PatchError> {
    if parent_path.is_empty() {
        return Ok(doc.as_table_mut());
    }

    let last_is_digit = parent_path
        .last()
        .is_some_and(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()));

    if last_is_digit {
        let (aot_parent, digit) = parent_path.split_at(parent_path.len() - 1);
        let idx: usize = digit[0]
            .parse()
            .map_err(|_| PatchError::PathDenied("invalid AOT index".into()))?;

        // aot_parent ends with the AOT key (e.g. ["displays","tv","ladder"]).
        let (table_path, aot_key_seg) = aot_parent.split_at(aot_parent.len() - 1);
        let aot_key = &aot_key_seg[0];

        // Walk to the table containing the AOT key, then index into it.
        let container = walk_table(doc.as_table_mut(), table_path, create_missing)?;
        let aot_item = container
            .get_mut(aot_key.as_str())
            .ok_or_else(|| PatchError::PathDenied(format!("AOT key not found: {aot_key}")))?;
        let aot = aot_item
            .as_array_of_tables_mut()
            .ok_or_else(|| PatchError::PathDenied(format!("{aot_key} is not an ArrayOfTables")))?;
        Ok(aot
            .get_mut(idx)
            .ok_or_else(|| PatchError::PathDenied(format!("AOT index {idx} out of bounds")))?)
    } else {
        // Regular table path: the last segment is the sub-table key.
        let (table_path, key_seg) = parent_path.split_at(parent_path.len() - 1);
        let key = &key_seg[0];

        let container = walk_table(doc.as_table_mut(), table_path, create_missing)?;

        if !container.contains_key(key.as_str()) {
            if create_missing {
                // Create an implicit table for missing structural keys.
                // Dynamic entity IDs never reach here because
                // check_patches rejects unknown entities before apply.
                container.insert(
                    key.as_str(),
                    toml_edit::Item::Table(toml_edit::Table::new()),
                );
            } else {
                return Err(PatchError::PathDenied(format!("key not found: {key}")));
            }
        }

        container
            .get_mut(key.as_str())
            .and_then(|item| item.as_table_mut())
            .ok_or_else(|| PatchError::PathDenied(format!("{key} is not a table")))
    }
}

/// Walk `root` through `path` segments, returning the final table.
///
/// When `create_missing` is true, intermediate segments that are missing and
/// are NOT immediate children of collection keys (`sensors`, `zones`,
/// `displays`, `rules`) are created as implicit tables.  Entity IDs (the
/// first segment under a collection) are never created.
///
/// Each iteration chains `get_mut → as_table_mut` in a single expression.
/// The intermediate `&mut Item` is dropped at the semicolon before `current`
/// is reassigned, so the borrow checker sees a linear exclusive chain.
fn walk_table<'a>(
    root: &'a mut toml_edit::Table,
    path: &[String],
    create_missing: bool,
) -> Result<&'a mut toml_edit::Table, PatchError> {
    if path.is_empty() {
        return Ok(root);
    }
    let mut current: &mut toml_edit::Table = root;
    for (i, seg) in path.iter().enumerate() {
        let is_collection_child = i > 0 && is_collection_key(&path[i - 1]);

        let exists = current.contains_key(seg.as_str());
        if !exists {
            if create_missing && !is_collection_child {
                current.insert(
                    seg.as_str(),
                    toml_edit::Item::Table(toml_edit::Table::new()),
                );
            } else {
                return Err(PatchError::PathDenied(format!("key not found: {seg}")));
            }
        }

        // Second borrow — after potential insert, get the table ref.
        current = current
            .get_mut(seg.as_str())
            .and_then(|item| item.as_table_mut())
            .ok_or_else(|| PatchError::PathDenied(format!("{seg} is not a table")))?;
    }
    Ok(current)
}

/// Returns true when `key` is a collection-level key whose immediate
/// children are dynamic entity IDs.
fn is_collection_key(key: &str) -> bool {
    matches!(key, "sensors" | "zones" | "displays" | "rules")
}

// ── JSON → toml_edit conversion ────────────────────────────────────────────

/// Convert a [`serde_json::Value`] to a [`toml_edit::Value`].
///
/// Strings, integers, floats and booleans are converted directly.
/// `null` is rejected.  Arrays become inline TOML arrays; objects become
/// inline tables.
fn json_to_toml_value(v: &serde_json::Value) -> Result<toml_edit::Value, PatchError> {
    match v {
        serde_json::Value::String(s) => Ok(toml_edit::Value::from(s.as_str())),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(toml_edit::Value::from(i))
            } else if let Some(f) = n.as_f64() {
                Ok(toml_edit::Value::from(f))
            } else {
                Err(PatchError::ValueRejected(format!(
                    "number out of representable range: {n}"
                )))
            }
        }
        serde_json::Value::Bool(b) => Ok(toml_edit::Value::from(*b)),
        serde_json::Value::Null => Err(PatchError::ValueRejected(
            "JSON null is not a valid TOML value".into(),
        )),
        serde_json::Value::Array(arr) => {
            let mut toml_arr = toml_edit::Array::new();
            for item in arr {
                let val = json_to_toml_value(item)?;
                toml_arr.push(val);
            }
            Ok(toml_edit::Value::from(toml_arr))
        }
        serde_json::Value::Object(obj) => {
            let mut table = toml_edit::InlineTable::new();
            for (k, val) in obj {
                let v = json_to_toml_value(val)?;
                table.insert(k, v);
            }
            Ok(toml_edit::Value::from(table))
        }
    }
}

/// Convert a JSON object into a [`toml_edit::Table`] (NOT an
/// [`toml_edit::InlineTable`], distinct from [`json_to_toml_value`]) — the
/// converter [`apply_create_entity`] uses so a new entity renders as its
/// own `[collection.id]` section, matching every existing entity's style.
///
/// A JSON `null` field value is treated as "not set" and OMITTED from the
/// table (mirroring an absent `Option<T>` field) rather than rejected —
/// `CreateEntity` payloads legitimately carry explicit `null` for unset
/// optional fields (e.g. a client-side form submitting `payload_on:
/// null`). A non-object `value` (already rejected by `check_create_entity`
/// for a checked caller) yields an empty table.
fn json_to_toml_table(value: &serde_json::Value) -> toml_edit::Table {
    let mut table = toml_edit::Table::new();
    if let serde_json::Value::Object(obj) = value {
        for (key, field_value) in obj {
            if field_value.is_null() {
                continue;
            }
            if let Ok(toml_value) = json_to_toml_value(field_value) {
                table.insert(key, toml_edit::Item::Value(toml_value));
            }
        }
    }
    table
}

/// Convert a JSON array of objects into a TOML [`ArrayOfTables`](toml_edit::ArrayOfTables).
fn json_to_array_of_tables(
    value: &serde_json::Value,
) -> Result<toml_edit::ArrayOfTables, PatchError> {
    let arr = value
        .as_array()
        .ok_or_else(|| PatchError::ValueRejected("expected JSON array for ArrayOfTables".into()))?;
    let mut aot = toml_edit::ArrayOfTables::new();
    for item in arr {
        let obj = item.as_object().ok_or_else(|| {
            PatchError::ValueRejected("ArrayOfTables entries must be JSON objects".into())
        })?;
        let mut table = toml_edit::Table::new();
        // Mark as implicit so it renders with `[[key]]` syntax, not `[key]`.
        table.set_implicit(true);
        for (k, val) in obj {
            let toml_val = json_to_toml_value(val)?;
            table.insert(k.as_str(), toml_edit::Item::Value(toml_val));
        }
        aot.push(table);
    }
    Ok(aot)
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Helpers ─────────────────────────────────────────────────────────

    /// Parse a TOML string into a `DocumentMut` for tests.
    fn doc(s: &str) -> toml_edit::DocumentMut {
        s.parse::<toml_edit::DocumentMut>().unwrap()
    }

    /// Convert &str slices to Vec<String> for path arguments.
    fn p(segments: &[&str]) -> Vec<String> {
        segments
            .iter()
            .map(std::string::ToString::to_string)
            .collect()
    }

    /// Convenience: build a Set patch.
    fn set(path: &[&str], value: serde_json::Value) -> Patch {
        Patch::Set {
            path: p(path),
            value,
        }
    }

    /// Convenience: build a Remove patch.
    fn remove(path: &[&str]) -> Patch {
        Patch::Remove { path: p(path) }
    }

    /// Convenience: run `check_patches` and unwrap to Ok; panic on Err.
    fn check_ok(patches: &[Patch], current: &toml_edit::DocumentMut, redacted: &[Vec<String>]) {
        check_patches(patches, current, redacted).expect("expected Ok");
    }

    /// Minimal valid config with several entity collections populated.
    fn minimal_config() -> toml_edit::DocumentMut {
        doc(r#"
config_version = 1

[daemon]
log_level = "info"
web_port = 9123

[sensors]
[sensors.desk]
type = "mqtt"
broker_url = "mqtt://localhost"
topic = "presence/desk"
field = "occupancy"

[zones]
[zones.office]
mode = "any"
members = ["desk"]

[displays]
[displays.tv]
controllers = ["samsung_tizen"]
blank_mode = "picture_off"

[displays.tv.screensaver]
trigger = "on_blank"

[[displays.tv.screensaver.source]]
path = "/usr/share/wallpapers"
order = "sequential"

[[displays.tv.ladder]]
kind = "power_off"

[rules]
[rules.r]
zone = "office"
displays = ["tv"]
grace_period = "30s"
"#)
    }

    // ==================================================================
    // 1. Hygiene tests
    // ==================================================================

    #[test]
    fn hygiene_rejects_double_dot() {
        let cur = minimal_config();
        let err = check_patches(
            &[set(&["daemon", "..", "log_level"], json!("debug"))],
            &cur,
            &[],
        )
        .unwrap_err();
        assert!(matches!(err, PatchError::PathDenied(m) if m.contains("..")));
    }

    #[test]
    fn hygiene_rejects_single_dot() {
        let cur = minimal_config();
        let err = check_patches(
            &[set(&["daemon", ".", "log_level"], json!("debug"))],
            &cur,
            &[],
        )
        .unwrap_err();
        assert!(matches!(err, PatchError::PathDenied(m) if m.contains('.')));
    }

    #[test]
    fn hygiene_rejects_empty_segment() {
        let cur = minimal_config();
        let err = check_patches(
            &[set(&["daemon", "", "log_level"], json!("debug"))],
            &cur,
            &[],
        )
        .unwrap_err();
        assert!(matches!(err, PatchError::PathDenied(m) if m.contains("empty")));
    }

    #[test]
    fn hygiene_rejects_depth_nine() {
        let cur = minimal_config();
        let deep: Vec<String> = (0..9).map(|i| format!("level{i}")).collect();
        let err = check_patches(
            &[Patch::Set {
                path: deep,
                value: json!("x"),
            }],
            &cur,
            &[],
        )
        .unwrap_err();
        assert!(matches!(err, PatchError::PathDenied(_)));
    }

    #[test]
    fn hygiene_rejects_interior_nul() {
        let cur = minimal_config();
        let err =
            check_patches(&[set(&["daemon", "bad\0key"], json!("debug"))], &cur, &[]).unwrap_err();
        assert!(
            matches!(err, PatchError::PathDenied(m) if m.contains("NUL") || m.contains("null"))
        );
    }

    #[test]
    fn hygiene_allows_valid_path() {
        let cur = minimal_config();
        check_ok(&[set(&["daemon", "log_level"], json!("debug"))], &cur, &[]);
    }

    // ==================================================================
    // 2. Known-path tests
    // ==================================================================

    #[test]
    fn unknown_root_key_rejected() {
        let cur = minimal_config();
        let err = check_patches(&[set(&["nope"], json!(42))], &cur, &[]).unwrap_err();
        assert!(matches!(err, PatchError::PathDenied(m) if m.contains("nope")));
    }

    #[test]
    fn unknown_leaf_rejected() {
        let cur = minimal_config();
        let err = check_patches(&[set(&["daemon", "nope"], json!(42))], &cur, &[]).unwrap_err();
        assert!(matches!(err, PatchError::PathDenied(m) if m.contains("nope")));
    }

    #[test]
    fn known_path_accepted() {
        let cur = minimal_config();
        check_ok(&[set(&["daemon", "web_port"], json!(8080))], &cur, &[]);
    }

    #[test]
    fn daemon_new_enum_and_duration_fields_accepted() {
        let cur = minimal_config();
        // idle_time_unit — enum (auto/ms/s), added to known-keys tree at daemon level.
        check_ok(
            &[set(&["daemon", "idle_time_unit"], json!("ms"))],
            &cur,
            &[],
        );
        // idle_source — enum (auto/wayland/dbus), also at daemon level.
        check_ok(
            &[set(&["daemon", "idle_source"], json!("wayland"))],
            &cur,
            &[],
        );
        // stale_sensor_timeout — duration, already in known-keys tree.
        check_ok(
            &[set(&["daemon", "stale_sensor_timeout"], json!("600s"))],
            &cur,
            &[],
        );
    }

    // ==================================================================
    // 3. Editable-subset tests
    // ==================================================================

    #[test]
    fn type_discriminator_is_locked() {
        let cur = minimal_config();
        let err = check_patches(&[set(&["sensors", "desk", "type"], json!("ha"))], &cur, &[])
            .unwrap_err();
        assert!(matches!(err, PatchError::PathDenied(m) if m.contains("type")));
    }

    #[test]
    fn blank_data_is_locked() {
        let cur = minimal_config();
        let err = check_patches(
            &[set(&["displays", "tv", "blank_data"], json!({"x":1}))],
            &cur,
            &[],
        )
        .unwrap_err();
        assert!(matches!(err, PatchError::PathDenied(m) if m.contains("blank_data")));
    }

    #[test]
    fn wake_data_is_locked() {
        let cur = minimal_config();
        let err = check_patches(
            &[set(&["displays", "tv", "wake_data"], json!({"x":1}))],
            &cur,
            &[],
        )
        .unwrap_err();
        assert!(matches!(err, PatchError::PathDenied(m) if m.contains("wake_data")));
    }

    #[test]
    fn remove_controllers_is_denied() {
        let cur = minimal_config();
        let err =
            check_patches(&[remove(&["displays", "tv", "controllers"])], &cur, &[]).unwrap_err();
        assert!(matches!(err, PatchError::PathDenied(m) if m.contains("controllers")));
    }

    #[test]
    fn remove_blank_mode_is_allowed() {
        let cur = minimal_config();
        check_ok(&[remove(&["displays", "tv", "blank_mode"])], &cur, &[]);
    }

    #[test]
    fn remove_degraded_mode_is_allowed() {
        let cur = minimal_config();
        check_ok(&[remove(&["displays", "tv", "degraded_mode"])], &cur, &[]);
    }

    #[test]
    fn remove_dwell_is_allowed() {
        let cur = minimal_config();
        check_ok(
            &[remove(&["displays", "tv", "ladder", "0", "dwell"])],
            &cur,
            &[],
        );
    }

    #[test]
    fn remove_order_is_allowed() {
        let cur = minimal_config();
        check_ok(
            &[remove(&[
                "displays",
                "tv",
                "screensaver",
                "source",
                "0",
                "order",
            ])],
            &cur,
            &[],
        );
    }

    #[test]
    fn set_order_sequential_accepted() {
        let cur = minimal_config();
        // Only "sequential" is valid per validate.rs — the form must not offer
        // "random" or "shuffle_playlist".  This test asserts the offered option
        // passes the structural patch pipeline (value validation happens at
        // daemon reload, not here).
        check_ok(
            &[set(
                &["displays", "tv", "screensaver", "source", "0", "order"],
                json!("sequential"),
            )],
            &cur,
            &[],
        );
    }

    #[test]
    fn remove_image_duration_is_allowed() {
        let cur = minimal_config();
        check_ok(
            &[remove(&[
                "displays",
                "tv",
                "screensaver",
                "source",
                "0",
                "image_duration",
            ])],
            &cur,
            &[],
        );
    }

    #[test]
    fn remove_scale_mode_is_allowed() {
        let cur = minimal_config();
        check_ok(
            &[remove(&["displays", "tv", "screensaver", "scale_mode"])],
            &cur,
            &[],
        );
    }

    #[test]
    fn remove_transition_is_allowed() {
        let cur = minimal_config();
        check_ok(
            &[remove(&["displays", "tv", "screensaver", "transition"])],
            &cur,
            &[],
        );
    }

    #[test]
    fn remove_transition_duration_is_allowed() {
        let cur = minimal_config();
        check_ok(
            &[remove(&[
                "displays",
                "tv",
                "screensaver",
                "transition_duration",
            ])],
            &cur,
            &[],
        );
    }

    #[test]
    fn remove_hold_time_is_allowed() {
        let cur = minimal_config();
        check_ok(&[remove(&["sensors", "desk", "hold_time"])], &cur, &[]);
    }

    #[test]
    fn remove_stale_timeout_is_allowed() {
        let cur = minimal_config();
        check_ok(&[remove(&["sensors", "desk", "stale_timeout"])], &cur, &[]);
    }

    #[test]
    fn remove_ddc_display_is_allowed() {
        let cur = minimal_config();
        check_ok(&[remove(&["displays", "tv", "ddc_display"])], &cur, &[]);
    }

    #[test]
    fn remove_output_is_allowed() {
        let cur = minimal_config();
        check_ok(&[remove(&["displays", "tv", "output"])], &cur, &[]);
    }

    #[test]
    fn remove_wol_mac_is_allowed() {
        let cur = minimal_config();
        check_ok(&[remove(&["displays", "tv", "wol_mac"])], &cur, &[]);
    }

    #[test]
    fn remove_host_is_allowed() {
        let cur = minimal_config();
        check_ok(&[remove(&["displays", "tv", "host"])], &cur, &[]);
    }

    // ==================================================================
    // 4. Entity-existence tests
    // ==================================================================

    #[test]
    fn unknown_display_id_rejected() {
        let cur = minimal_config();
        let err = check_patches(
            &[set(
                &["displays", "nonexistent", "blank_mode"],
                json!("screen_off"),
            )],
            &cur,
            &[],
        )
        .unwrap_err();
        assert!(matches!(err, PatchError::EntityUnknown(m) if m.contains("nonexistent")));
    }

    #[test]
    fn unknown_sensor_id_rejected() {
        let cur = minimal_config();
        let err = check_patches(
            &[set(&["sensors", "nonexistent", "hold_time"], json!("5s"))],
            &cur,
            &[],
        )
        .unwrap_err();
        assert!(matches!(err, PatchError::EntityUnknown(m) if m.contains("nonexistent")));
    }

    #[test]
    fn unknown_zone_id_rejected() {
        let cur = minimal_config();
        let err = check_patches(
            &[set(&["zones", "nonexistent", "mode"], json!("all"))],
            &cur,
            &[],
        )
        .unwrap_err();
        assert!(matches!(err, PatchError::EntityUnknown(m) if m.contains("nonexistent")));
    }

    #[test]
    fn unknown_rule_id_rejected() {
        let cur = minimal_config();
        let err = check_patches(
            &[set(&["rules", "nonexistent", "grace_period"], json!("10s"))],
            &cur,
            &[],
        )
        .unwrap_err();
        assert!(matches!(err, PatchError::EntityUnknown(m) if m.contains("nonexistent")));
    }

    #[test]
    fn source_index_out_of_range_rejected() {
        let cur = minimal_config();
        // There's only one source entry (index 0), so index 5 is out of range.
        let err = check_patches(
            &[set(
                &["displays", "tv", "screensaver", "source", "5", "order"],
                json!("random"),
            )],
            &cur,
            &[],
        )
        .unwrap_err();
        assert!(matches!(err, PatchError::EntityUnknown(_)));
    }

    #[test]
    fn existing_entity_accepted() {
        let cur = minimal_config();
        check_ok(
            &[set(&["displays", "tv", "blank_mode"], json!("screen_off"))],
            &cur,
            &[],
        );
    }

    // ==================================================================
    // 5. Redacted prefix tests
    // ==================================================================

    #[test]
    fn redacted_exact_match_rejected() {
        let cur = minimal_config();
        // Inline-array element indices (urls.1) are NOT in the T1 known-key
        // tree — only source/ladder indices are indexed.  Use a known leaf
        // (order) that can be redacted.
        let redacted = vec![p(&[
            "displays",
            "tv",
            "screensaver",
            "source",
            "0",
            "order",
        ])];
        let err = check_patches(
            &[set(
                &["displays", "tv", "screensaver", "source", "0", "order"],
                json!("random"),
            )],
            &cur,
            &redacted,
        )
        .unwrap_err();
        assert!(matches!(err, PatchError::RedactedPath(_)));
    }

    #[test]
    fn redacted_descendant_rejected() {
        // Patching a child of a redacted path → rejected.
        let cur = minimal_config();
        let redacted = vec![p(&["displays", "tv", "screensaver", "source", "0"])];
        let err = check_patches(
            &[set(
                &["displays", "tv", "screensaver", "source", "0", "order"],
                json!("random"),
            )],
            &cur,
            &redacted,
        )
        .unwrap_err();
        assert!(matches!(err, PatchError::RedactedPath(_)));
    }

    #[test]
    fn redacted_ancestor_rejected() {
        // Patching an ancestor that CONTAINS a redacted leaf → rejected.
        let cur = minimal_config();
        let redacted = vec![p(&["displays", "tv", "screensaver", "source", "0", "path"])];
        let err = check_patches(
            &[set(
                &["displays", "tv", "screensaver", "source"],
                json!([{"path": "/new"}]),
            )],
            &cur,
            &redacted,
        )
        .unwrap_err();
        assert!(matches!(err, PatchError::RedactedPath(_)));
    }

    #[test]
    fn unrelated_path_not_redacted() {
        let cur = minimal_config();
        let redacted = vec![p(&["displays", "tv", "screensaver", "source", "0", "path"])];
        check_ok(
            &[set(&["daemon", "web_port"], json!(8080))],
            &cur,
            &redacted,
        );
    }

    #[test]
    fn empty_redacted_entry_skipped() {
        let cur = minimal_config();
        // An empty redacted entry would match every path if the check were
        // not skipped (both prefix slices would be empty).  Mirror the TS
        // isLocked defensive skip.
        let redacted: Vec<Vec<String>> = vec![vec![], p(&["sensors", "desk", "broker_url"])];
        // Not under the non-empty redacted path → should pass.
        check_ok(
            &[set(&["daemon", "web_port"], json!(8080))],
            &cur,
            &redacted,
        );
    }

    // ==================================================================
    // 6. Pipeline order tests
    // ==================================================================

    #[test]
    fn unknown_and_redacted_returns_path_denied_not_redacted() {
        let cur = minimal_config();
        let redacted = vec![p(&["daemon", "nope"])];
        let err =
            check_patches(&[set(&["daemon", "nope"], json!(42))], &cur, &redacted).unwrap_err();
        assert!(
            matches!(err, PatchError::PathDenied(_)),
            "expected PathDenied (pipeline order), got {err:?}"
        );
    }

    #[test]
    fn locked_and_redacted_returns_path_denied_not_redacted() {
        let cur = minimal_config();
        let redacted = vec![p(&["sensors", "desk", "type"])];
        let err = check_patches(
            &[set(&["sensors", "desk", "type"], json!("ha"))],
            &cur,
            &redacted,
        )
        .unwrap_err();
        assert!(
            matches!(err, PatchError::PathDenied(_)),
            "expected PathDenied (pipeline order), got {err:?}"
        );
    }

    // ==================================================================
    // 7. Golden preservation tests
    // ==================================================================

    /// Fixture with comments in every possible position.
    const GOLDEN_FIXTURE: &str = r#"# Top-level header comment
config_version = 1

# daemon section doc comment
[daemon]
log_level = "info"    # trailing comment on log_level
web_port = 9123

# sensors section doc comment
[sensors]

[sensors.desk]
type = "mqtt"
broker_url = "mqtt://localhost" # trailing broker comment
topic = "presence/desk"
field = "occupancy"

[zones]

[zones.office]
mode = "any"
members = ["desk"] # inline array comment

[displays]

[displays.tv]
# per-table comment above controllers
controllers = ["samsung_tizen"]
blank_mode = "picture_off"

# per-table comment above screensaver
[displays.tv.screensaver]
trigger = "on_blank"

[[displays.tv.screensaver.source]]
path = "/usr/share/wallpapers"
order = "sequential"
# a comment inside ladder array entry
[[displays.tv.ladder]]
kind = "power_off"

[rules]

[rules.r]
zone = "office"
displays = ["tv"]
grace_period = "30s" # trailing grace_period comment
"#;

    #[test]
    fn set_preserves_comments_everywhere() {
        let mut doc = doc(GOLDEN_FIXTURE);

        let patches = [set(&["rules", "r", "grace_period"], json!("20s"))];
        check_ok(&patches, &doc, &[]);
        apply_patches(&mut doc, &patches).unwrap();

        let after = doc.to_string();

        // Full-string equality: the only change is 30s → 20s.
        let expected = GOLDEN_FIXTURE.replace("\"30s\"", "\"20s\"");
        assert_eq!(after, expected, "only grace_period value should change");
    }

    #[test]
    fn set_ladder_array_preserves_array_of_tables_form() {
        let mut doc = doc(GOLDEN_FIXTURE);

        let patches = [set(
            &["displays", "tv", "ladder"],
            json!([{"kind": "screen_off"}, {"kind": "power_off"}]),
        )];
        check_ok(&patches, &doc, &[]);
        apply_patches(&mut doc, &patches).unwrap();

        let after = doc.to_string();
        // ArrayOfTables form preserved ([[double-bracket]])
        assert!(after.contains("[[displays.tv.ladder]]"));
        assert!(after.contains("kind = \"screen_off\""));
        assert!(after.contains("kind = \"power_off\""));
        // Internal comment inside the replaced array entries is lost —
        // the spec only requires comments OUTSIDE the array to survive.
    }

    #[test]
    fn remove_optional_key_preserves_neighbors() {
        let mut doc = doc(GOLDEN_FIXTURE);

        let patches = [remove(&["displays", "tv", "blank_mode"])];
        check_ok(&patches, &doc, &[]);
        apply_patches(&mut doc, &patches).unwrap();

        let after = doc.to_string();

        // Full-string equality: blank_mode line removed, everything else intact.
        let expected = GOLDEN_FIXTURE.replace("blank_mode = \"picture_off\"\n", "");
        assert_eq!(after, expected, "only blank_mode line should be removed");
    }

    #[test]
    fn humantime_values_stay_quoted_strings() {
        let mut doc = doc(GOLDEN_FIXTURE);

        let patches = [set(&["rules", "r", "grace_period"], json!("1m30s"))];
        check_ok(&patches, &doc, &[]);
        apply_patches(&mut doc, &patches).unwrap();

        let after = doc.to_string();
        assert!(after.contains("grace_period = \"1m30s\""));
    }

    #[test]
    fn json_integer_typed_as_toml_integer() {
        let mut doc = doc(GOLDEN_FIXTURE);

        let patches = [set(&["daemon", "web_port"], json!(8080))];
        check_ok(&patches, &doc, &[]);
        apply_patches(&mut doc, &patches).unwrap();

        let after = doc.to_string();
        // Integer, not "8080" string
        assert!(after.contains("web_port = 8080"));
        assert!(!after.contains("web_port = \"8080\""));
    }

    #[test]
    fn json_bool_typed_as_toml_bool() {
        let mut doc = doc(GOLDEN_FIXTURE);
        let patches = [set(&["daemon", "web_allow_nonloopback"], json!(true))];
        check_ok(&patches, &doc, &[]);
        apply_patches(&mut doc, &patches).unwrap();

        let after = doc.to_string();
        assert!(after.contains("web_allow_nonloopback = true"));
    }

    #[test]
    fn json_float_typed_as_toml_float() {
        let mut doc = doc(GOLDEN_FIXTURE);

        let patches = [set(&["zones", "office", "quorum"], json!(0.75))];
        check_ok(&patches, &doc, &[]);
        apply_patches(&mut doc, &patches).unwrap();

        let after = doc.to_string();
        assert!(after.contains("quorum = 0.75"));
    }

    #[test]
    fn json_null_value_rejected() {
        let mut doc = doc(GOLDEN_FIXTURE);
        let err = apply_patches(
            &mut doc,
            &[set(&["daemon", "web_port"], serde_json::Value::Null)],
        )
        .expect_err("null should be rejected by apply_patches");
        assert!(matches!(err, PatchError::ValueRejected(_)));
    }

    #[test]
    fn set_array_value_on_source_preserves_array_of_tables() {
        let mut doc = doc(GOLDEN_FIXTURE);

        let patches = [set(
            &["displays", "tv", "screensaver", "source"],
            json!([
                {"path": "/new/path", "order": "random"},
                {"urls": ["https://a.com"], "shuffle": true}
            ]),
        )];
        check_ok(&patches, &doc, &[]);
        apply_patches(&mut doc, &patches).unwrap();

        let after = doc.to_string();
        // ArrayOfTables preserved
        assert!(after.contains("[[displays.tv.screensaver.source]]"));
        assert!(after.contains("path = \"/new/path\""));
        assert!(after.contains("order = \"random\""));
    }

    #[test]
    fn set_inline_array_stays_inline() {
        // controllers is an inline array, not ArrayOfTables.
        let mut doc = doc(GOLDEN_FIXTURE);

        let patches = [set(
            &["displays", "tv", "controllers"],
            json!(["kwin_dpms", "ddcci"]),
        )];
        check_ok(&patches, &doc, &[]);
        apply_patches(&mut doc, &patches).unwrap();

        let after = doc.to_string();
        // Inline array, not [[...]]
        assert!(after.contains("controllers = [\"kwin_dpms\", \"ddcci\"]"));
        assert!(!after.contains("[[displays.tv.controllers]]"));
    }

    // ==================================================================
    // 8. Container-Set bypass regression tests — verify the
    //    container-level rejection catches smuggling of locked leaves
    //    through whole-table Set operations.
    // ==================================================================

    #[test]
    fn set_sensors_table_is_denied() {
        // Setting the whole sensors table smuggles locked 'type'.
        let cur = minimal_config();
        let err = check_patches(
            &[set(
                &["sensors"],
                json!({"desk": {"type": "ha", "broker_url": "mqtt://x", "topic": "t", "field": "f"}}),
            )],
            &cur,
            &[],
        )
        .unwrap_err();
        assert!(
            matches!(err, PatchError::PathDenied(_)),
            "setting container table should be denied, got {err:?}"
        );
    }

    #[test]
    fn set_display_table_is_denied() {
        // Setting displays.tv table smuggles locked leaves.
        let cur = minimal_config();
        let err = check_patches(
            &[set(
                &["displays", "tv"],
                json!({"controllers": ["kwin_dpms"], "blank_data": {"x": 1}, "wake_data": {"y": 2}}),
            )],
            &cur,
            &[],
        )
        .unwrap_err();
        assert!(
            matches!(err, PatchError::PathDenied(_)),
            "setting container table should be denied, got {err:?}"
        );
    }

    #[test]
    fn set_displays_root_table_is_denied() {
        // Setting the displays collection smuggles locked leaves.
        let cur = minimal_config();
        let err = check_patches(
            &[set(
                &["displays"],
                json!({"tv": {"controllers": ["kwin_dpms"], "blank_data": {"x": 1}}}),
            )],
            &cur,
            &[],
        )
        .unwrap_err();
        assert!(
            matches!(err, PatchError::PathDenied(_)),
            "setting container table should be denied, got {err:?}"
        );
    }

    #[test]
    fn nested_smuggle_in_source_payload_is_denied() {
        // Nested-smuggle: setting a whole source AOT with a locked leaf in payload.
        let cur = minimal_config();
        let err = check_patches(
            &[set(
                &["displays", "tv", "screensaver", "source"],
                json!([{"path": "/x", "type": "evil"}]),
            )],
            &cur,
            &[],
        )
        .unwrap_err();
        assert!(
            matches!(err, PatchError::PathDenied(_)),
            "locked leaf in array payload should be denied, got {err:?}"
        );
    }

    #[test]
    fn clean_source_array_set_is_allowed() {
        // A clean whole-array Set on source (no locked leaves) is allowed.
        let cur = minimal_config();
        check_ok(
            &[set(
                &["displays", "tv", "screensaver", "source"],
                json!([{"path": "/pics"}]),
            )],
            &cur,
            &[],
        );
    }

    // ==================================================================
    // 9. Wire-shape serde tests
    // ==================================================================

    #[test]
    fn deserialize_set_patch() {
        let json = r#"{"op":"set","path":["daemon","log_level"],"value":"debug"}"#;
        let patch: Patch = serde_json::from_str(json).unwrap();
        assert_eq!(
            patch,
            Patch::Set {
                path: p(&["daemon", "log_level"]),
                value: json!("debug"),
            }
        );
    }

    #[test]
    fn deserialize_remove_patch() {
        let json = r#"{"op":"remove","path":["displays","tv","blank_mode"]}"#;
        let patch: Patch = serde_json::from_str(json).unwrap();
        assert_eq!(
            patch,
            Patch::Remove {
                path: p(&["displays", "tv", "blank_mode"]),
            }
        );
    }

    #[test]
    fn deserialize_create_entity_patch() {
        // Wire-string pin: rename_all="lowercase" alone would produce
        // "createentity" (it lowercases the whole variant name, it does
        // NOT snake_case it) — the explicit #[serde(rename="create_entity")]
        // is load-bearing.
        let json = r#"{"op":"create_entity","collection":"sensors","id":"desk2","value":{"type":"mqtt","broker_url":"mqtt://x","topic":"t"}}"#;
        let patch: Patch = serde_json::from_str(json).unwrap();
        assert_eq!(
            patch,
            Patch::CreateEntity {
                collection: "sensors".into(),
                id: "desk2".into(),
                value: json!({"type":"mqtt","broker_url":"mqtt://x","topic":"t"}),
            }
        );
    }

    #[test]
    fn deserialize_delete_entity_patch() {
        let json = r#"{"op":"delete_entity","collection":"sensors","id":"desk2"}"#;
        let patch: Patch = serde_json::from_str(json).unwrap();
        assert_eq!(
            patch,
            Patch::DeleteEntity {
                collection: "sensors".into(),
                id: "desk2".into(),
            }
        );
    }

    #[test]
    fn create_entity_op_string_is_not_lowercased_variant_name() {
        // Documents the exact failure mode the rename guards against: the
        // naive rename_all=lowercase output would be "createentity", which
        // must NOT deserialize successfully as a distinct correct op.
        let naive = r#"{"op":"createentity","collection":"sensors","id":"x","value":{}}"#;
        assert!(serde_json::from_str::<Patch>(naive).is_err());
    }

    // ==================================================================
    // 10. THE SECURITY MATRIX (Task 2 — the load-bearing tests, plan
    //     lines 100-157, copied faithfully incl. comments).
    // ==================================================================

    // --- reserved-name cross-check (R2-M1, the load-bearing one) ---
    #[test]
    fn reserved_ids_superset_of_all_special_cases() {
        // P1: references the REAL symbol, not a copy
        use dormant_core::config::STRUCTURAL_RESERVED_NAMES;
        for n in STRUCTURAL_RESERVED_NAMES {
            assert!(RESERVED_ENTITY_IDS.contains(n));
        }
        for n in LOCKED_LEAVES {
            assert!(RESERVED_ENTITY_IDS.contains(n));
        }
        for n in REMOVABLE_KEYS {
            assert!(RESERVED_ENTITY_IDS.contains(n));
        }
        // source/ladder are in STRUCTURAL_RESERVED_NAMES; nothing hand-copied. A new
        // dormant-core predicate that extends the const but not RESERVED_ENTITY_IDS →
        // RED here.
        // (cold-gate deepseek S1: no separate `∪ {"source","ladder"}` union here —
        // those two names are already members of STRUCTURAL_RESERVED_NAMES, checked
        // in the first loop above; a literal extra union would be a no-op.
        // Intentionally not added — don't "restore" it as belt-and-suspenders, it
        // would just be dead code.)
    }

    #[test]
    fn create_reserved_id_rejected() {
        let cur = minimal_config();
        for id in [
            "type",
            "source",
            "ladder",
            "host",
            "output",
            "weights",
            "blank_data",
        ] {
            assert!(
                matches!(
                    check_create_entity("sensors", id, &json!({"type": "mqtt"}), &cur),
                    Err(PatchError::PathDenied(_))
                ),
                "expected id '{id}' to be rejected as reserved"
            );
        }
    }

    #[test]
    fn weights_named_entity_cannot_smuggle_unknown_key() {
        // R2-M1 functional pin. cold-gate M3 Should-3: the original body was
        // comments-only (asserted nothing, passed vacuously) — now makes two
        // real assertions per M3's prescribed fix.
        let cur = minimal_config();
        assert!(matches!(
            check_create_entity("sensors", "weights", &json!({"type": "mqtt"}), &cur),
            Err(PatchError::PathDenied(_))
        )); // create id="weights" is REJECTED (reserved) — the Set(["sensors","weights","evil"]) path never opens.
        assert!(dormant_core::config::is_known_config_path(&[
            "sensors", "weights", "evil"
        ]));
        // The documenting belt: is_known_config_path's suffix rule WOULD accept an
        // arbitrary "evil" key under a "weights"-named entity absent the create-time
        // ban above — proving the ban is load-bearing, not decorative.
    }

    // --- DeleteEntity collection scope (R2-M2) ---
    #[test]
    fn delete_non_crud_collection_rejected() {
        let cur = minimal_config();
        assert!(matches!(
            check_delete_entity("daemon", "web_allow_nonloopback", &cur),
            Err(PatchError::PathDenied(_))
        ));
        // RED if the collection-enum check is omitted → a security toggle gets deleted.
    }

    // --- create-gate ---
    #[test]
    fn create_sensor_happy_path() {
        let cur = minimal_config();
        assert!(
            check_create_entity(
                "sensors",
                "newsensor",
                &json!({"type": "mqtt", "broker_url": "mqtt://localhost", "topic": "presence/new"}),
                &cur,
            )
            .is_ok()
        );
    }

    #[test]
    fn create_with_blank_data_top_level_denied() {
        // Independent exclusion (B-M1) — blank_data is not in
        // CREATABLE_FIELDS[sensors], so it's denied at the allowlist step
        // regardless of the locked-leaf recursion.
        let cur = minimal_config();
        let err = check_create_entity(
            "sensors",
            "newsensor",
            &json!({
                "type": "mqtt",
                "broker_url": "mqtt://localhost",
                "topic": "t",
                "blank_data": {"x": 1},
            }),
            &cur,
        )
        .unwrap_err();
        assert!(matches!(err, PatchError::PathDenied(_)));
    }

    #[test]
    fn create_display_with_type_denied() {
        // Displays have no `type` discriminator (A-M) — it's simply absent
        // from CREATABLE_FIELDS[displays]'s closed enumeration.
        let cur = minimal_config();
        let err = check_create_entity(
            "displays",
            "newdisplay",
            &json!({"controllers": ["kwin-dpms"], "type": "foo"}),
            &cur,
        )
        .unwrap_err();
        assert!(matches!(err, PatchError::PathDenied(_)));
    }

    #[test]
    fn create_unknown_top_field_denied() {
        let cur = minimal_config();
        let err = check_create_entity(
            "sensors",
            "newsensor",
            &json!({"type": "mqtt", "broker_url": "x", "topic": "t", "bogus_field": "z"}),
            &cur,
        )
        .unwrap_err();
        assert!(matches!(err, PatchError::PathDenied(_)));
    }

    #[test]
    fn create_existing_id_is_entity_exists() {
        let cur = minimal_config();
        let err = check_create_entity(
            "sensors",
            "desk", // already present in minimal_config()
            &json!({"type": "mqtt", "broker_url": "x", "topic": "t"}),
            &cur,
        )
        .unwrap_err();
        assert!(matches!(err, PatchError::EntityExists(_)));
    }

    #[test]
    fn container_set_still_denied() {
        // The old regression tests stay green — assert explicitly (they
        // also live under "8. Container-Set bypass regression tests"
        // above; this re-asserts the property survives the CreateEntity
        // dispatch restructure).
        let cur = minimal_config();
        let err = check_patches(
            &[set(
                &["sensors"],
                json!({"desk": {"type": "ha", "broker_url": "x", "topic": "t", "field": "f"}}),
            )],
            &cur,
            &[],
        )
        .unwrap_err();
        assert!(matches!(err, PatchError::PathDenied(_)));
    }

    // --- command-execution field exclusion (cold-gate M3 Must-1, spec §4/§11 invariant 2c) ---
    #[test]
    fn create_display_with_blank_command_denied() {
        // blank_command/wake_command are NOT in CREATABLE_FIELDS[displays]'s closed
        // enumeration — a web-created display cannot carry a daemon-executed shell
        // command in v1.
        let cur = minimal_config();
        let v = json!({"controllers": ["command"], "blank_command": "echo hi"});
        assert!(matches!(
            check_create_entity("displays", "x", &v, &cur),
            Err(PatchError::PathDenied(_))
        ));
    }

    #[test]
    fn create_display_with_wake_command_denied() {
        let cur = minimal_config();
        let v = json!({"controllers": ["command"], "wake_command": "echo hi"});
        assert!(matches!(
            check_create_entity("displays", "x", &v, &cur),
            Err(PatchError::PathDenied(_))
        ));
    }

    #[test]
    fn creatable_fields_key_space_is_closed_to_crud_collections() {
        // Structural pin, not name-based: CREATABLE_FIELDS covers EXACTLY the 4 CRUD
        // collections and nothing else — so a future `audio.pw_dump_command` (feature
        // 03, not yet in-tree) is unreachable from CreateEntity by construction, no
        // code review needed to remember to exclude it. See spec §4/§11 invariant 2c.
        let collections: Vec<&str> = CREATABLE_FIELDS.iter().map(|(c, _)| *c).collect();
        assert_eq!(collections, vec!["sensors", "zones", "displays", "rules"]);
    }

    // ==================================================================
    // 11. Entity-id hygiene (spec §5 charset + reserved-name ban).
    // ==================================================================

    #[test]
    fn validate_entity_id_accepts_valid_ids() {
        for id in [
            "desk", "office", "tv", "r", "sensor-2", "zone_a", "a1b2c3", "x",
        ] {
            assert!(
                validate_entity_id(id).is_ok(),
                "expected '{id}' to be valid"
            );
        }
    }

    #[test]
    fn validate_entity_id_rejects_empty() {
        assert!(matches!(
            validate_entity_id(""),
            Err(PatchError::PathDenied(_))
        ));
    }

    #[test]
    fn validate_entity_id_accepts_max_length_64() {
        let id = "a".repeat(64);
        assert!(validate_entity_id(&id).is_ok());
    }

    #[test]
    fn validate_entity_id_rejects_too_long_65() {
        let id = "a".repeat(65);
        assert!(matches!(
            validate_entity_id(&id),
            Err(PatchError::PathDenied(_))
        ));
    }

    #[test]
    fn validate_entity_id_rejects_leading_digit() {
        // Subsumed by "first char [a-z]" — a digit id is structurally
        // indistinguishable from an AOT index (is_digit_segment /
        // aot_index_exists) — the ambiguity class is removed entirely.
        assert!(matches!(
            validate_entity_id("1sensor"),
            Err(PatchError::PathDenied(_))
        ));
        assert!(matches!(
            validate_entity_id("123"),
            Err(PatchError::PathDenied(_))
        ));
    }

    #[test]
    fn validate_entity_id_rejects_uppercase() {
        assert!(matches!(
            validate_entity_id("Desk"),
            Err(PatchError::PathDenied(_))
        ));
    }

    #[test]
    fn validate_entity_id_rejects_space() {
        assert!(matches!(
            validate_entity_id("my sensor"),
            Err(PatchError::PathDenied(_))
        ));
    }

    #[test]
    fn validate_entity_id_rejects_dot() {
        assert!(matches!(
            validate_entity_id("my.sensor"),
            Err(PatchError::PathDenied(_))
        ));
    }

    #[test]
    fn validate_entity_id_rejects_unicode() {
        assert!(matches!(
            validate_entity_id("s\u{e9}nsor"),
            Err(PatchError::PathDenied(_))
        ));
    }

    #[test]
    fn validate_entity_id_rejects_all_reserved_names() {
        for id in RESERVED_ENTITY_IDS {
            assert!(
                matches!(validate_entity_id(id), Err(PatchError::PathDenied(_))),
                "expected reserved id '{id}' to be rejected"
            );
        }
    }

    // ==================================================================
    // 12. Delete-gate tests.
    // ==================================================================

    #[test]
    fn delete_sensor_happy_path() {
        let cur = minimal_config();
        assert!(check_delete_entity("sensors", "desk", &cur).is_ok());
    }

    #[test]
    fn delete_missing_entity_is_entity_unknown() {
        let cur = minimal_config();
        assert!(matches!(
            check_delete_entity("sensors", "nonexistent", &cur),
            Err(PatchError::EntityUnknown(_))
        ));
    }

    #[test]
    fn delete_reserved_id_rejected_even_if_hygiene_only() {
        // A hand-authored entity legitimately named e.g. "host" pre-dates
        // this feature's charset validation (spec §5 "forward-only
        // residual") — it is NOT deletable via the new API (validate_entity_id
        // rejects the reserved name on delete too, same as create).
        let cur = minimal_config();
        assert!(matches!(
            check_delete_entity("sensors", "host", &cur),
            Err(PatchError::PathDenied(_))
        ));
    }

    // ==================================================================
    // 13. apply_create_entity / apply_delete_entity + json_to_toml_table.
    // ==================================================================

    #[test]
    fn apply_create_entity_renders_explicit_table_not_inline() {
        let mut d = doc("config_version = 1\n");
        apply_create_entity(
            &mut d,
            "sensors",
            "newsensor",
            &json!({"type": "mqtt", "broker_url": "mqtt://x", "topic": "t"}),
        );
        let out = d.to_string();
        assert!(
            out.contains("[sensors.newsensor]"),
            "expected an explicit [sensors.newsensor] section, got:\n{out}"
        );
        assert!(
            !out.contains("newsensor = {"),
            "must not render as an inline table, got:\n{out}"
        );
        assert!(out.contains(r#"type = "mqtt""#));
    }

    #[test]
    fn apply_create_entity_first_in_empty_collection_still_explicit_table() {
        // The very-first-entity case: [collection] doesn't exist at all yet.
        // Ordering-critical (C-S round 3) — a one-liner index would
        // auto-vivify [collection] as an InlineTable, corrupting the section.
        let mut d = doc("config_version = 1\n");
        assert!(d.get("sensors").is_none());
        apply_create_entity(
            &mut d,
            "sensors",
            "first",
            &json!({"type": "mqtt", "broker_url": "mqtt://x", "topic": "t"}),
        );
        let out = d.to_string();
        assert!(out.contains("[sensors.first]"), "got:\n{out}");
        assert!(!out.contains("sensors = {"), "got:\n{out}");
    }

    #[test]
    fn apply_create_entity_omits_null_fields() {
        let mut d = doc("config_version = 1\n");
        apply_create_entity(
            &mut d,
            "sensors",
            "s",
            &json!({"type": "mqtt", "broker_url": "x", "topic": "t", "payload_on": null}),
        );
        let out = d.to_string();
        assert!(!out.contains("payload_on"), "got:\n{out}");
    }

    #[test]
    fn apply_delete_entity_removes_only_target() {
        let mut cur = minimal_config();
        apply_delete_entity(&mut cur, "sensors", "desk");
        assert!(
            cur.get("sensors")
                .and_then(|i| i.as_table())
                .is_some_and(|t| !t.contains_key("desk"))
        );
    }

    #[test]
    fn json_to_toml_table_converts_scalar_and_array_fields() {
        let t = json_to_toml_table(&json!({
            "mode": "any",
            "members": ["a", "b"],
        }));
        assert_eq!(t.get("mode").and_then(|i| i.as_str()), Some("any"));
        assert!(t.get("members").and_then(|i| i.as_array()).is_some());
    }

    // ==================================================================
    // 14. check_patches dispatch — CreateEntity/DeleteEntity end to end.
    // ==================================================================

    #[test]
    fn check_patches_create_entity_end_to_end_ok() {
        let cur = minimal_config();
        let patches = [Patch::CreateEntity {
            collection: "zones".into(),
            id: "newzone".into(),
            value: json!({"mode": "any", "members": ["desk"]}),
        }];
        assert!(check_patches(&patches, &cur, &[]).is_ok());
    }

    #[test]
    fn check_patches_delete_entity_end_to_end_ok() {
        let cur = minimal_config();
        let patches = [Patch::DeleteEntity {
            collection: "rules".into(),
            id: "r".into(),
        }];
        assert!(check_patches(&patches, &cur, &[]).is_ok());
    }

    #[test]
    fn check_patches_create_entity_reserved_collection_rejected() {
        let cur = minimal_config();
        let patches = [Patch::CreateEntity {
            collection: "daemon".into(),
            id: "evil".into(),
            value: json!({"web_port": 9999}),
        }];
        assert!(matches!(
            check_patches(&patches, &cur, &[]),
            Err(PatchError::PathDenied(_))
        ));
    }
}
