/**
 * Shared component barrel — all views import from here.
 * Add new reusable components as they are extracted.
 */
export { default as StatusChip } from "./StatusChip";
export type { StatusKind } from "./StatusChip";
export { statusLabel, stageKindLabel, phaseChipLabel } from "./status";
export { default as Card } from "./Card";
export { default as HealthChip } from "./HealthChip";
export { default as WearCard } from "./WearCard";
export { default as FailureBanner } from "./FailureBanner";
export type { FailureBannerProps } from "./FailureBanner";
export { default as RollbackBanner } from "./RollbackBanner";
export type { RollbackBannerProps } from "./RollbackBanner";
export { default as EmergencyWakeControl } from "./EmergencyWakeControl";
export { default as ConfirmDialog } from "./ConfirmDialog";
export type { ConfirmOptions, ConfirmDialogProps } from "./ConfirmDialog";
export { useConfirmDialog } from "./useConfirmDialog";
