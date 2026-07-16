/**
 * State → human-readable label mapping.
 *
 * Used by StatusChip and anywhere that needs to display a
 * state string (Dashboard sensor/zone rows, Events badges).
 */
import type { StatusKind } from "./StatusChip";

/**
 * StageKind → human-readable label.
 *
 * Used to render the stage detail when a display is in the `staged` phase.
 */
const STAGE_KIND_LABELS: Record<string, string> = {
  power_off: "power off",
  screen_off_audio_on: "screen off + audio",
  brightness_zero: "brightness 0",
  render_black: "render black",
  render_screensaver: "render screensaver",
};

/** Return a human-readable label for a StageKind wire value. */
export function stageKindLabel(kind: string): string {
  return STAGE_KIND_LABELS[kind] ?? kind;
}

const STATE_LABELS: Record<string, string> = {
  present: "present",
  absent: "absent",
  unavailable: "unavailable",
  active: "active",
  grace: "grace",
  blanking: "blanking…",
  blanked: "blanked",
  waking: "waking",
  staged: "staged",
  render_pending: "starting…",
  paused: "paused",
  inhibited: "inhibited",
  ok: "ok",
  fail: "fail",
  wake_retry: "retry",
  skip: "skip",
  not_supported: "n/a",
  blank_failed: "blank failed",
  wear_advisory: "wear advisory",
};

export function statusLabel(kind: StatusKind): string {
  return STATE_LABELS[kind] ?? kind;
}

/**
 * Phase → display chip label, optionally enriched with stage detail.
 *
 * For `staged` displays with known stage, returns "staged · <stage label>".
 * For `render_pending`, returns "starting…".  Otherwise falls through to
 * the base statusLabel.
 */
export function phaseChipLabel(
  phase: string,
  stage?: { kind: string } | null,
): string | undefined {
  if (phase === "staged" && stage) {
    return `staged · ${stageKindLabel(stage.kind)}`;
  }
  if (phase === "render_pending") {
    return "starting…";
  }
  return undefined;
}
