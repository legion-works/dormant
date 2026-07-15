/**
 * Failure banner — global chrome (mounted once in `Shell`, not per
 * view; the second global alert, directly after `RollbackBanner`).
 * Surfaces displays that are currently failing to wake or whose last
 * blank command exhausted its controller chain.
 *
 * Snapshot-only derivation (mirrors dormant-tray's `derive_icon_state`
 * Failure predicate in crates/dormant-tray/src/state.rs): a display is
 * failing when `(wake_attempts ?? 0) > 0 || (last_blank_failed ?? false)`.
 * Both fields are `#[serde(default)]` on the wire — a legacy snapshot
 * that omits them renders nothing here (never crashes, never false-alarms).
 *
 * `onInspect` routes an operator from this global banner to the failing
 * display's detail (Shell selects the display then navigates to
 * Displays) — it does not own any navigation itself.
 */
import { useLiveState } from "../hooks/useLiveState";
import type { ControllerHealth, DisplaySnapshot } from "../../api/types";
import "./FailureBanner.css";

export interface FailureBannerProps {
  onInspect: (display: string) => void;
}

interface FailingDisplay {
  id: string;
  wakeAttempts: number;
  blankFailed: boolean;
  unhealthyController?: ControllerHealth;
}

function isFailing(d: DisplaySnapshot): boolean {
  return (d.wake_attempts ?? 0) > 0 || (d.last_blank_failed ?? false);
}

export default function FailureBanner({ onInspect }: FailureBannerProps) {
  const { snapshot } = useLiveState();
  if (!snapshot) return null;

  const failing: FailingDisplay[] = snapshot.displays
    .filter(([, d]) => isFailing(d))
    .map(([id, d]) => ({
      id,
      wakeAttempts: d.wake_attempts ?? 0,
      blankFailed: d.last_blank_failed ?? false,
      unhealthyController: d.controllers.find((c) => !c.healthy),
    }));

  if (failing.length === 0) return null;

  return (
    <div
      className="failure-banner global-banner global-banner--failure"
      data-testid="failure-banner"
      role="alert"
    >
      <div className="failure-banner__summary">
        <strong className="global-banner__title">
          {failing.length} display{failing.length === 1 ? "" : "s"} blank chain exhausted
        </strong>
        <span className="global-banner__detail">panel may be stuck lit</span>
      </div>
      {failing.map((f) => (
        <div
          key={f.id}
          className="failure-banner__row"
          data-testid={`failure-row-${f.id}`}
        >
          <span className="failure-banner__id">{f.id}</span>
          {f.wakeAttempts > 0 && (
            <span className="failure-banner__detail">
              wake failing ×{f.wakeAttempts}
            </span>
          )}
          {f.blankFailed && (
            <span className="failure-banner__detail">last blank failed</span>
          )}
          {f.unhealthyController && (
            <span className="failure-banner__detail">
              {f.unhealthyController.name}
              {f.unhealthyController.detail ? `: ${f.unhealthyController.detail}` : ""}
            </span>
          )}
          <button
            type="button"
            className="failure-banner__inspect"
            onClick={() => onInspect(f.id)}
          >
            Inspect {f.id}
          </button>
        </div>
      ))}
    </div>
  );
}
