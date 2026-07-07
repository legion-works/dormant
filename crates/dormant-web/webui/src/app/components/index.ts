/**
 * Shared component barrel — all views import from here.
 * Add new reusable components as they are extracted.
 */
export { default as StatusChip } from "./StatusChip";
export type { StatusKind } from "./StatusChip";
export { statusLabel, stageKindLabel, phaseChipLabel } from "./status";
export { default as Card } from "./Card";
export { default as HealthChip } from "./HealthChip";
