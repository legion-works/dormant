/**
 * Panel exposure card — Dashboard summary of per-display panel wear.
 *
 * `GET /api/wear` is the source of truth for both the displayed numbers
 * and the `advisory` flag (server-derived, spec §7.3) — WS frames are
 * best-effort nudges only:
 *   - `wear_snapshot` patches the displayed on-hours/sample-count in
 *     place (via `useLiveState().wearSnapshots`).
 *   - `compensation_advisory` nudges the advisory line immediately (via
 *     `useLiveState().wearAdvisories`) and triggers a re-fetch so the
 *     fetch path catches up.
 *
 * Honesty rule (spec §7.3): no spatial attribution surfaces here — see
 * the grid caption. This card only shows panel-wide totals + advisory.
 */
import { useEffect, useState } from "react";
import { getWear } from "../../api/client";
import { useLiveState } from "../hooks/useLiveState";
import type { WearSummary } from "../../api/types";
import "./WearCard.css";

/** Minutes elapsed since an epoch-seconds timestamp, or "—" if unknown. */
function minutesAgoLabel(epochS: number | null | undefined): string {
  if (epochS == null) return "—";
  const mins = Math.max(0, Math.round((Date.now() / 1000 - epochS) / 60));
  return `${mins} min ago`;
}

interface WearRowProps {
  summary: WearSummary;
  /** `compensation_advisory` WS nudge: hours since long-dwell, if any arrived. */
  advisoryHoursNudge?: number;
}

function WearRow({ summary, advisoryHoursNudge }: WearRowProps) {
  const advisory = summary.advisory || advisoryHoursNudge !== undefined;
  // `hours_since_long_dwell` is server-derived from
  // `max(last_long_dwell_epoch_s, advisory_baseline_epoch_s)` (T8 review
  // Should-fix) — always a real number, even for a display that has never
  // had an observed long dwell yet (baseline-only, the common first-load
  // case), so this never falls back to a "?" day count.
  const days = Math.floor((advisoryHoursNudge ?? summary.hours_since_long_dwell) / 24);

  return (
    <div className="wear-row" data-testid={`wear-row-${summary.display}`}>
      <div className="wear-row__top">
        <span className="wear-row__name">{summary.display_name}</span>
        <span className="wear-row__panel">{summary.panel_type}</span>
      </div>
      <div className="wear-row__stats">
        <span>{summary.total_on_hours.toFixed(1)}h total on-time</span>
        {summary.seeded_usage_hours != null && (
          <span>+{summary.seeded_usage_hours}h seeded</span>
        )}
        <span>last long-dwell {minutesAgoLabel(summary.last_long_dwell_epoch_s)}</span>
      </div>
      {advisory && (
        <div className="wear-row__advisory">no long standby window in {days} days</div>
      )}
    </div>
  );
}

export default function WearCard() {
  const [displays, setDisplays] = useState<WearSummary[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const { wearSnapshots, wearAdvisories } = useLiveState();

  useEffect(() => {
    let mounted = true;
    getWear()
      .then((res) => {
        if (mounted) setDisplays(res.displays);
      })
      .catch((err: unknown) => {
        if (mounted) setError(err instanceof Error ? err.message : "Unknown error");
      });
    return () => {
      mounted = false;
    };
    // Re-fetch whenever a compensation_advisory nudge arrives so the
    // fetch path (source of truth) catches up promptly.
  }, [wearAdvisories]);

  return (
    <div className="wear-card">
      <div className="wear-card__header">Panel exposure</div>
      <div className="wear-card__caption">
        no spatial attribution yet — arrives with content-aware tracking (v2)
      </div>

      {error && <div className="wear-card__error">Wear data unavailable: {error}</div>}

      {!error && displays === null && (
        <div className="wear-card__loading">Loading…</div>
      )}

      {!error && displays !== null && displays.length === 0 && (
        <div className="wear-card__empty">No tracked displays yet.</div>
      )}

      {!error &&
        displays !== null &&
        displays.map((d) => {
          const patch = wearSnapshots[d.display];
          const merged: WearSummary = patch
            ? { ...d, total_on_hours: patch.total_on_hours, sample_count: patch.sample_count }
            : d;
          return (
            <WearRow
              key={d.display}
              summary={merged}
              advisoryHoursNudge={wearAdvisories[d.display]}
            />
          );
        })}
    </div>
  );
}
