/** Log levels matching Rust's tracing crate + dormant's custom levels. */
export const LOG_LEVELS = ["trace", "debug", "info", "warn", "error"] as const;

/** Zone fusion modes. */
export const FUSION_MODES = ["any", "all", "quorum", "weighted"] as const;

/** Unavailable-policy options. */
export const UNAVAILABLE_POLICIES = ["present", "absent"] as const;

/** daemon.idle_time_unit options. */
export const IDLE_TIME_UNITS = ["auto", "ms", "s"] as const;

/** daemon.idle_source options. */
export const IDLE_SOURCES = ["auto", "wayland", "dbus", "macos"] as const;

/** displays.*.panel_type options. rust: wear.rs PanelType, kebab-case. */
export const PANEL_TYPES = ["woled", "qd-oled", "unknown"] as const;
