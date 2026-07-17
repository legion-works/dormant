//! Issue-draft rendering — turns the offline doctor's own findings into a
//! ready-to-paste GitHub issue body.
//!
//! `dormantctl doctor --report-issue` / `--draft-feature` already have
//! everything the `.github/ISSUE_TEMPLATE/{bug,feature}.yml` templates ask
//! for on hand at the moment something fails: the probe table, the loaded
//! display inventory, and the environment. This module only *renders* that
//! data — no file I/O, no process spawning. The CLI (`dormantctl`)
//! collects [`EnvInfo`] and writes the file; `cmd_doctor.rs` owns the
//! collision-suffix path logic.
//!
//! Redaction is allowlist-style by construction: [`build_display_inventory`]
//! copies out only `id`, `panel_type`, `controllers`, and the primary
//! `blank_mode` from [`DisplayConfig`] — a new config field (host, token,
//! MAC address, ...) added later does not leak into a draft unless someone
//! deliberately adds it to the allowlist here.

use std::fmt::Write as _;
use std::time::SystemTime;

use dormant_core::config::Config;

use crate::types::ProbeResult;

// ── EnvInfo ─────────────────────────────────────────────────────────────────────

/// Machine-collectable environment facts for a draft's Environment section.
///
/// Every field is `"unknown"` when the underlying source is unavailable —
/// never fabricated.
#[derive(Debug, Clone, PartialEq)]
pub struct EnvInfo {
    /// `/etc/os-release` `PRETTY_NAME`, or `"unknown"`.
    pub os_pretty_name: String,
    /// Kernel release (`uname -r` equivalent), or `"unknown"`.
    pub kernel_release: String,
    /// `XDG_SESSION_TYPE` (`"wayland"` / `"x11"`), or `"unknown"`.
    pub session_type: String,
    /// `XDG_CURRENT_DESKTOP`, or `"unknown"`.
    pub desktop: String,
}

const UNKNOWN: &str = "unknown";

/// Collect [`EnvInfo`] from the real environment: `/etc/os-release`, the
/// `uname -r` equivalent, and the `XDG_SESSION_TYPE` / `XDG_CURRENT_DESKTOP`
/// environment variables.
///
/// The only impure entry point in this module — kept to a single, obvious
/// caller (`cmd_doctor.rs`) so [`render_bug_draft`] / [`render_feature_draft`]
/// stay pure and unit-testable against a hand-built [`EnvInfo`].
#[must_use]
pub fn collect_env() -> EnvInfo {
    let os_release = std::fs::read_to_string("/etc/os-release").ok();
    let kernel_release = std::process::Command::new("uname")
        .arg("-r")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    collect_env_from(os_release.as_deref(), kernel_release, |key| {
        std::env::var(key).ok()
    })
}

/// Pure core of [`collect_env`] — takes already-read inputs so the parsing
/// logic is testable without touching the filesystem, a subprocess, or real
/// environment variables.
fn collect_env_from(
    os_release_content: Option<&str>,
    kernel_release: Option<String>,
    env_lookup: impl Fn(&str) -> Option<String>,
) -> EnvInfo {
    EnvInfo {
        os_pretty_name: parse_os_pretty_name(os_release_content).unwrap_or_else(|| UNKNOWN.into()),
        kernel_release: kernel_release
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| UNKNOWN.into()),
        session_type: env_lookup("XDG_SESSION_TYPE").unwrap_or_else(|| UNKNOWN.into()),
        desktop: env_lookup("XDG_CURRENT_DESKTOP").unwrap_or_else(|| UNKNOWN.into()),
    }
}

/// Parse `PRETTY_NAME="..."` out of `/etc/os-release` content.
fn parse_os_pretty_name(content: Option<&str>) -> Option<String> {
    let content = content?;
    content.lines().find_map(|line| {
        let value = line.strip_prefix("PRETTY_NAME=")?;
        Some(value.trim().trim_matches('"').to_string())
    })
}

// ── Display inventory (allowlist redaction) ─────────────────────────────────────

/// One display's allowlisted, publishable fields.
///
/// Deliberately does NOT hold `host`, `wol_mac`, `ha_url`, or any other
/// network-address/credential-shaped field — see the module docs.
#[derive(Debug, Clone, PartialEq)]
pub struct DisplayInventoryEntry {
    /// The display's config id.
    pub id: String,
    /// Panel technology classification (`"Woled"` / `"QdOled"` / `"Unknown"`).
    pub panel_type: String,
    /// Ordered controller chain (controller names only — no per-controller
    /// connection details).
    pub controllers: Vec<String>,
    /// Primary blank mode (`normalized_ladder`'s first controller stage).
    pub blank_mode: String,
}

/// Build the display inventory for a draft, allowlist-style: only the four
/// fields above are copied out of each [`dormant_core::config::schema::DisplayConfig`].
#[must_use]
pub fn build_display_inventory(cfg: &Config) -> Vec<DisplayInventoryEntry> {
    cfg.displays
        .iter()
        .map(|(id, display)| DisplayInventoryEntry {
            id: id.clone(),
            panel_type: format!("{:?}", display.panel_type),
            controllers: display.controllers.clone(),
            blank_mode: format!("{:?}", display.primary_blank_mode()),
        })
        .collect()
}

// ── DraftContext ─────────────────────────────────────────────────────────────────

/// Everything a draft needs to render: version, environment, config status,
/// the redacted display inventory, and the probe results from the just-run
/// offline doctor pass.
#[derive(Debug, Clone, PartialEq)]
pub struct DraftContext {
    /// `dormant` crate version (`env!("CARGO_PKG_VERSION")`).
    pub version: String,
    /// Collected environment facts.
    pub env: EnvInfo,
    /// The config path that was loaded (as displayed, not canonicalized).
    pub config_path: String,
    /// Whether the config loaded and validated without a `Fail` probe.
    pub config_ok: bool,
    /// Allowlisted per-display inventory.
    pub displays: Vec<DisplayInventoryEntry>,
    /// The full offline probe result set, in run order.
    pub probes: Vec<ProbeResult>,
}

// ── Date formatting (no new date/time dependency) ───────────────────────────────

/// Format `now` as `YYYY-MM-DD` (UTC), for the default draft filename.
///
/// Implements Howard Hinnant's `civil_from_days` algorithm directly on
/// epoch seconds rather than pulling in a date/time crate — the workspace
/// already has no `chrono`/`time` dependency and this needs nothing more
/// than a calendar date.
#[must_use]
pub fn format_date_ymd(now: SystemTime) -> String {
    let epoch_s = now
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let days = i64::try_from(epoch_s / 86400).unwrap_or(0);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Days-since-epoch → (year, month, day). See Howard Hinnant's
/// "chrono-Compatible Low-Level Date Algorithms" for the derivation.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1); // [1, 31]
    let m = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1); // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

// ── Shared rendering helpers ─────────────────────────────────────────────────────

const FILL_IN: &str = "<!-- fill in -->";

fn render_environment_section(ctx: &DraftContext) -> String {
    format!(
        "## Environment\n\n\
         - OS: {os}\n\
         - Kernel: {kernel}\n\
         - Session type: {session}\n\
         - Desktop/compositor: {desktop}\n\
         - dormant version: {version}\n",
        os = ctx.env.os_pretty_name,
        kernel = ctx.env.kernel_release,
        session = ctx.env.session_type,
        desktop = ctx.env.desktop,
        version = ctx.version,
    )
}

fn render_config_section(ctx: &DraftContext) -> String {
    let status = if ctx.config_ok {
        "loaded and validated OK"
    } else {
        "FAILED to load or validate — see the doctor output below"
    };
    format!(
        "## Configuration\n\n\
         Config path: `{path}` ({status})\n\n\
         <!-- Paste your sanitized TOML config here if it helps. Do not paste \
         tokens, passwords, or credentials.toml contents. -->\n",
        path = ctx.config_path,
    )
}

fn render_displays_section(ctx: &DraftContext) -> String {
    let mut out = String::from("## Displays involved\n\n");
    if ctx.displays.is_empty() {
        out.push_str("_No displays configured._\n\n");
    } else {
        out.push_str("| Display | Type | Controllers | Blank mode |\n");
        out.push_str("|---|---|---|---|\n");
        for d in &ctx.displays {
            let _ = writeln!(
                out,
                "| {id} | {panel_type} | {controllers} | {blank_mode} |",
                id = d.id,
                panel_type = d.panel_type,
                controllers = d.controllers.join(", "),
                blank_mode = d.blank_mode,
            );
        }
        out.push('\n');
    }
    out.push_str(
        "<!-- fill in: exact make + model, panel technology, and connection \
         (DisplayPort / HDMI / network-only) for each display above -->\n",
    );
    out
}

fn render_doctor_section(ctx: &DraftContext) -> String {
    let mut out = String::from("## `dormantctl doctor` output\n\n");
    out.push_str("| Probe | Status | Detail |\n");
    out.push_str("|---|---|---|\n");
    for r in &ctx.probes {
        let status = match r.status {
            crate::types::ProbeStatus::Pass => "PASS",
            crate::types::ProbeStatus::Fail => "FAIL",
            crate::types::ProbeStatus::Skip => "SKIP",
            crate::types::ProbeStatus::NotSupported => "N/A",
        };
        // Markdown table cells can't contain literal newlines or bare pipes.
        let detail = r.detail.replace('|', "\\|").replace('\n', "<br>");
        let _ = writeln!(out, "| {} | {status} | {detail} |", r.name);
    }
    out
}

// ── Bug draft ─────────────────────────────────────────────────────────────────

/// Render the bug-report draft — mirrors `.github/ISSUE_TEMPLATE/bug.yml`'s
/// field order and headings so pasting the result into a new GitHub issue
/// lines up with the template.
#[must_use]
pub fn render_bug_draft(ctx: &DraftContext) -> String {
    let mut out = String::new();
    out.push_str("# Bug report (dormant doctor draft)\n\n");
    out.push_str(
        "<!-- Generated by `dormantctl doctor --report-issue`. Fill in every \
         section marked <!-- fill in --> before filing, then paste this body \
         into a new issue using the Bug Report template. -->\n\n",
    );

    out.push_str("## Summary\n\n");
    out.push_str(FILL_IN);
    out.push_str("\nWhat happened, and what did you expect instead?\n\n");

    out.push_str("## Steps to reproduce\n\n");
    out.push_str(FILL_IN);
    out.push_str("\nThe exact sequence that triggers it.\n\n");

    out.push_str(&render_config_section(ctx));
    out.push('\n');
    out.push_str(&render_displays_section(ctx));
    out.push('\n');
    out.push_str(&render_environment_section(ctx));
    out.push('\n');

    out.push_str("## `dormantctl status` output\n\n");
    out.push_str(FILL_IN);
    out.push_str("\nPaste `dormantctl status` while the bug is present.\n\n");

    out.push_str(&render_doctor_section(ctx));
    out.push('\n');

    out.push_str("## Relevant daemon logs\n\n");
    out.push_str(FILL_IN);
    out.push_str("\nSet `log_level = \"debug\"` and reproduce for the useful detail.\n\n");

    out.push_str("## Area\n\n");
    out.push_str(FILL_IN);
    out.push_str("\nCore / Sensors / Displays / Render / CLI / Web UI / Tray / Reload\n");

    out
}

// ── Feature draft ─────────────────────────────────────────────────────────────

/// Render the feature-request draft — mirrors
/// `.github/ISSUE_TEMPLATE/feature.yml`'s field order and headings. No
/// probe-failure framing: this is the environment-capture shape without a
/// bug narrative.
#[must_use]
pub fn render_feature_draft(ctx: &DraftContext) -> String {
    let mut out = String::new();
    out.push_str("# Feature request (dormant doctor draft)\n\n");
    out.push_str(
        "<!-- Generated by `dormantctl doctor --draft-feature`. Fill in every \
         section marked <!-- fill in --> before filing, then paste this body \
         into a new issue using the Feature Request template. -->\n\n",
    );

    out.push_str("## Use case\n\n");
    out.push_str(FILL_IN);
    out.push_str(
        "\nWhat problem does this solve? Describe the room, who's in it, \
         and what dormant should do.\n\n",
    );

    out.push_str("## Proposed behavior\n\n");
    out.push_str(FILL_IN);
    out.push_str("\nWhat should dormant do? A config sketch helps.\n\n");

    out.push_str(&render_displays_section(ctx));
    out.push('\n');

    out.push_str("## Sensors involved\n\n");
    out.push_str(FILL_IN);
    out.push_str("\nName + how it reaches dormant, or \"none\".\n\n");

    out.push_str(&render_environment_section(ctx));
    out.push('\n');

    out.push_str("## Control path and prior art\n\n");
    out.push_str(FILL_IN);
    out.push_str(
        "\nProtocol/API docs, other projects that already control this \
         device, captured tool output.\n\n",
    );

    out.push_str("## Can you test on this hardware?\n\n");
    out.push_str(FILL_IN);
    out.push('\n');

    out.push_str("## Alternatives considered\n\n");
    out.push_str(FILL_IN);
    out.push('\n');

    out.push_str("## Area\n\n");
    out.push_str(FILL_IN);
    out.push_str("\nCore / Sensors / Displays / Render / CLI / Web UI / Tray / Documentation\n");

    out
}

// ── Tests ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use dormant_core::config::defaults;
    use dormant_core::config::schema::{
        AudioConfig, Credentials, DaemonConfig, DisplayConfig, NotificationsConfig, WatchdogConfig,
        WearConfig,
    };
    use dormant_core::types::BlankMode;
    use dormant_core::wear::PanelType;
    use indexmap::IndexMap;

    // ── collect_env_from ────────────────────────────────────────────────────

    #[test]
    fn parse_os_pretty_name_extracts_quoted_value() {
        let content = "NAME=\"Arch Linux\"\nPRETTY_NAME=\"Arch Linux\"\nID=arch\n";
        assert_eq!(
            parse_os_pretty_name(Some(content)),
            Some("Arch Linux".to_string())
        );
    }

    #[test]
    fn collect_env_from_unknown_when_all_absent() {
        let env = collect_env_from(None, None, |_| None);
        assert_eq!(env.os_pretty_name, "unknown");
        assert_eq!(env.kernel_release, "unknown");
        assert_eq!(env.session_type, "unknown");
        assert_eq!(env.desktop, "unknown");
    }

    #[test]
    fn collect_env_from_populates_present_fields() {
        let content = "PRETTY_NAME=\"Ubuntu 24.04\"\n";
        let env = collect_env_from(Some(content), Some("6.9.0".into()), |key| match key {
            "XDG_SESSION_TYPE" => Some("wayland".into()),
            "XDG_CURRENT_DESKTOP" => Some("KDE".into()),
            _ => None,
        });
        assert_eq!(env.os_pretty_name, "Ubuntu 24.04");
        assert_eq!(env.kernel_release, "6.9.0");
        assert_eq!(env.session_type, "wayland");
        assert_eq!(env.desktop, "KDE");
    }

    // ── build_display_inventory ─────────────────────────────────────────────

    fn sample_display(controllers: Vec<&str>, host: Option<&str>) -> DisplayConfig {
        DisplayConfig {
            controllers: controllers.into_iter().map(String::from).collect(),
            blank_mode: Some(BlankMode::PowerOff),
            degraded_mode: None,
            ladder: vec![],
            screensaver: None,
            output: None,
            ddc_display: None,
            host: host.map(String::from),
            wol_mac: None,
            blank_command: None,
            wake_command: None,
            modes: None,
            ha_url: None,
            blank_service: None,
            blank_data: None,
            wake_service: None,
            wake_data: None,
            command_timeout: defaults::COMMAND_TIMEOUT,
            restore_brightness: 80,
            samsung_restore_backlight: defaults::SAMSUNG_RESTORE_BACKLIGHT,
            treat_unreachable_as_blanked: true,
            panel_type: PanelType::default(),
        }
    }

    fn config_with_displays(displays: IndexMap<String, DisplayConfig>) -> Config {
        Config {
            config_version: 1,
            daemon: DaemonConfig::default(),
            wear: WearConfig::default(),
            notifications: NotificationsConfig::default(),
            watchdog: WatchdogConfig::default(),
            audio: AudioConfig::default(),
            sensors: IndexMap::default(),
            zones: IndexMap::default(),
            displays,
            rules: IndexMap::default(),
        }
    }

    #[test]
    fn build_display_inventory_maps_allowlisted_fields_only() {
        let mut displays = IndexMap::new();
        displays.insert(
            "tv".to_string(),
            sample_display(vec!["samsung-tizen"], Some("192.168.1.99")),
        );
        let cfg = config_with_displays(displays);

        let inventory = build_display_inventory(&cfg);
        assert_eq!(inventory.len(), 1);
        assert_eq!(inventory[0].id, "tv");
        assert_eq!(inventory[0].controllers, vec!["samsung-tizen"]);
        assert_eq!(inventory[0].blank_mode, "PowerOff");
    }

    // ── date formatting ──────────────────────────────────────────────────────

    #[test]
    fn format_date_ymd_epoch_is_1970_01_01() {
        assert_eq!(format_date_ymd(std::time::UNIX_EPOCH), "1970-01-01");
    }

    #[test]
    fn format_date_ymd_known_date() {
        // 2026-07-18T00:00:00Z = 1784332800
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_784_332_800);
        assert_eq!(format_date_ymd(t), "2026-07-18");
    }

    // ── render_bug_draft / render_feature_draft ─────────────────────────────

    fn sample_ctx() -> DraftContext {
        let mut displays = IndexMap::new();
        displays.insert(
            "main_monitor".to_string(),
            sample_display(vec!["ddcci", "command"], None),
        );
        DraftContext {
            version: "0.3.1".into(),
            env: EnvInfo {
                os_pretty_name: "Arch Linux".into(),
                kernel_release: "6.9.0".into(),
                session_type: "wayland".into(),
                desktop: "KDE".into(),
            },
            config_path: "/home/user/.config/dormant/config.toml".into(),
            config_ok: true,
            displays: build_display_inventory(&config_with_displays(displays)),
            probes: vec![
                ProbeResult::pass("config", "configuration OK"),
                ProbeResult::fail("ddcci", "no DDC/CI displays detected"),
            ],
        }
    }

    #[test]
    fn bug_draft_contains_template_headings_and_version() {
        let out = render_bug_draft(&sample_ctx());
        for heading in [
            "## Summary",
            "## Steps to reproduce",
            "## Configuration",
            "## Displays involved",
            "## Environment",
            "## `dormantctl status` output",
            "## `dormantctl doctor` output",
            "## Relevant daemon logs",
            "## Area",
        ] {
            assert!(out.contains(heading), "missing heading: {heading}");
        }
        assert!(out.contains("0.3.1"), "should contain dormant version");
        assert!(out.contains("main_monitor"), "should list the display");
        assert!(out.contains("ddcci"), "should list the probe row");
        assert!(
            out.contains("no DDC/CI displays detected"),
            "should list the probe detail"
        );
    }

    #[test]
    fn feature_draft_shape_has_no_bug_specific_sections() {
        let out = render_feature_draft(&sample_ctx());
        for heading in [
            "## Use case",
            "## Proposed behavior",
            "## Displays involved",
            "## Sensors involved",
            "## Environment",
            "## Control path and prior art",
            "## Can you test on this hardware?",
            "## Alternatives considered",
            "## Area",
        ] {
            assert!(out.contains(heading), "missing heading: {heading}");
        }
        assert!(
            !out.contains("## Steps to reproduce"),
            "feature draft must not carry bug-framing sections"
        );
        assert!(
            !out.contains("## Relevant daemon logs"),
            "feature draft must not carry bug-framing sections"
        );
    }

    #[test]
    fn unknown_env_renders_literal_unknown() {
        let mut ctx = sample_ctx();
        ctx.env = EnvInfo {
            os_pretty_name: "unknown".into(),
            kernel_release: "unknown".into(),
            session_type: "unknown".into(),
            desktop: "unknown".into(),
        };
        let out = render_bug_draft(&ctx);
        assert!(out.contains("- OS: unknown"));
        assert!(out.contains("- Kernel: unknown"));
        assert!(out.contains("- Session type: unknown"));
        assert!(out.contains("- Desktop/compositor: unknown"));
    }

    /// Decisive redaction test: a config with a Samsung host and a
    /// credentials token must not leak either string into either draft —
    /// [`build_display_inventory`] only copies the allowlisted fields, so
    /// this fails loudly if that allowlist is ever widened by mistake.
    #[test]
    fn drafts_never_leak_host_or_credentials() {
        let secret_host = "192.168.77.42";
        let secret_token = "supersecret-samsung-token-do-not-leak";

        let mut displays = IndexMap::new();
        displays.insert(
            "tv".to_string(),
            sample_display(vec!["samsung-tizen"], Some(secret_host)),
        );
        let cfg = config_with_displays(displays);
        let mut creds = Credentials::default();
        creds
            .samsung
            .insert(secret_host.to_string(), secret_token.to_string());

        let ctx = DraftContext {
            displays: build_display_inventory(&cfg),
            ..sample_ctx()
        };

        let bug = render_bug_draft(&ctx);
        let feature = render_feature_draft(&ctx);

        assert!(
            !bug.contains(secret_host) && !bug.contains(secret_token),
            "bug draft leaked host or token"
        );
        assert!(
            !feature.contains(secret_host) && !feature.contains(secret_token),
            "feature draft leaked host or token"
        );
        // Sanity: the token really was set (would be a false-negative test
        // otherwise).
        assert_eq!(
            creds.samsung.get(secret_host).map(String::as_str),
            Some(secret_token)
        );
    }
}
