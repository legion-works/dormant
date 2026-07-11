/**
 * Failure banner — Dashboard-level surfacing of displays that are
 * currently failing to wake or whose last blank command exhausted its
 * controller chain.
 *
 * Snapshot-only derivation (mirrors dormant-tray's `derive_icon_state`
 * Failure predicate in crates/dormant-tray/src/state.rs): a display is
 * failing when `(wake_attempts ?? 0) > 0 || (last_blank_failed ?? false)`.
 * Both fields are `#[serde(default)]` on the wire — a legacy snapshot
 * that omits them renders nothing here (never crashes, never false-alarms).
 */
import { useLiveState } from "../hooks/useLiveState";
import type { DisplaySnapshot } from "../../api/types";
import "./FailureBanner.css";

interface FailingDisplay {
  id: string;
  wakeAttempts: number;
  blankFailed: boolean;
}

function isFailing(d: DisplaySnapshot): boolean {
  return (d.wake_attempts ?? 0) > 0 || (d.last_blank_failed ?? false);
}

export default function FailureBanner() {
  const { snapshot } = useLiveState();
  if (!snapshot) return null;

  const failing: FailingDisplay[] = snapshot.displays
    .filter(([, d]) => isFailing(d))
    .map(([id, d]) => ({
      id,
      wakeAttempts: d.wake_attempts ?? 0,
      blankFailed: d.last_blank_failed ?? false,
    }));

  if (failing.length === 0) return null;

  return (
    <div className="failure-banner" data-testid="failure-banner">
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
        </div>
      ))}
    </div>
  );
}
