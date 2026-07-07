/** Log levels matching Rust's tracing crate + dormant's custom levels. */
export const LOG_LEVELS = ["trace", "debug", "info", "warn", "error"] as const;

/** Zone fusion modes. */
export const FUSION_MODES = ["any", "all", "quorum", "weighted"] as const;

/** Unavailable-policy options. */
export const UNAVAILABLE_POLICIES = ["present", "absent"] as const;
