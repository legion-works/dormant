/**
 * Shared component barrel — all views and Batch B (Events/Config/Doctor)
 * import from here.  Add new reusable components to this file as they
 * are extracted.
 */
export { default as StatusChip } from "./StatusChip";
export type { StatusKind } from "./StatusChip";
export { statusLabel } from "./status";
export { default as Card } from "./Card";
export { default as HealthChip } from "./HealthChip";
