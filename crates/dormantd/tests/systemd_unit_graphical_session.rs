//! The shipped systemd user unit starts the daemon only inside a graphical
//! session.
//!
//! A daemon that starts before the desktop session has no `WAYLAND_DISPLAY`
//! and no `DISPLAY` for its whole life: render stages fail, the
//! user-activity inhibitor cannot read idle time, and command hooks that need
//! the compositor abort. These tests pin the unit directives that prevent
//! such a start and that tie the unit to the session.

const UNIT: &str = include_str!("../systemd/app-dormant.service");

/// Directive lines (comments and blank lines removed) of one `[section]`.
fn section(name: &str) -> Vec<&'static str> {
    let header = format!("[{name}]");
    UNIT.lines()
        .map(str::trim)
        .skip_while(|line| *line != header)
        .skip(1)
        .take_while(|line| !line.starts_with('['))
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect()
}

#[test]
fn unit_start_is_skipped_without_a_graphical_session() {
    let unit = section("Unit");
    for directive in [
        "ConditionEnvironment=|WAYLAND_DISPLAY",
        "ConditionEnvironment=|DISPLAY",
    ] {
        assert!(
            unit.contains(&directive),
            "[Unit] must contain `{directive}` so that a start outside a graphical session is skipped; found {unit:?}"
        );
    }
}

#[test]
fn unit_is_bound_to_the_graphical_session() {
    let unit = section("Unit");
    for directive in [
        "After=graphical-session.target",
        "PartOf=graphical-session.target",
    ] {
        assert!(
            unit.contains(&directive),
            "[Unit] must contain `{directive}`; found {unit:?}"
        );
    }
    let install = section("Install");
    assert!(
        install.contains(&"WantedBy=graphical-session.target"),
        "[Install] must contain `WantedBy=graphical-session.target`; found {install:?}"
    );
}
