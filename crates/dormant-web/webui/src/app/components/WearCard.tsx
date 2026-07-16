/**
 * Panel exposure card — Dashboard summary of per-display panel wear.
 *
 * Reads provider state only (`useLiveState().wear`/`wearError`) — no
 * private fetch. `GET /api/wear` (and its per-display detail retries)
 * live in `LiveStateProvider.refreshWear`, called on mount and
 * re-triggered by `wear_snapshot`/`compensation_advisory` WS nudges
 * (spec §7.3); this component just renders whatever the provider
 * currently holds.
 *
 * `display` (the wear tracker's storage key, used only for
 * `GET /api/wear/:display`) and `display_name` (the configured display
 * id used by snapshot/config/UI joins) are distinct — this card always
 * renders and keys on `display_name`.
 *
 * Honesty rule (spec §7.3): no spatial attribution surfaces here — this
 * card only shows panel-wide totals + advisory/compensation status.
 *
 * Tone rules (do not invent panel-health thresholds):
 *   - `wearError` → error border (every row — the fetch itself failed).
 *   - `summary.advisory === true` → warning border + "no long standby
 *     window in N days".
 *   - otherwise → success border + "compensation window healthy".
 */
import { useNavigate } from "../nav";
import { useLiveState } from "../hooks/useLiveState";
import type { WearSummary } from "../../api/types";
import "./WearCard.css";

type Tone = "success" | "warning" | "error";

interface WearRowProps {
  summary: WearSummary;
  tone: Tone;
  onOpenDetail: (displayName: string) => void;
}

function WearRow({ summary, tone, onOpenDetail }: WearRowProps) {
  const days = Math.floor(summary.hours_since_long_dwell / 24);
  const toneText =
    tone === "error"
      ? "wear data unavailable"
      : tone === "warning"
        ? `no long standby window in ${days} days`
        : "compensation window healthy";

  return (
    <div className={`wear-row wear-row--${tone}`} data-testid={`wear-row-${summary.display_name}`}>
      <button
        type="button"
        className="wear-row__summary"
        aria-label={`Open ${summary.display_name} panel detail`}
        onClick={() => onOpenDetail(summary.display_name)}
      >
        <strong className="wear-row__name">{summary.display_name}</strong>
        <span className="wear-row__stat">{summary.total_on_hours.toFixed(1)}h total on-time</span>
        <span className="wear-row__stat">{summary.sample_count.toLocaleString()} samples</span>
      </button>
      <div className={`wear-row__tone wear-row__tone--${tone}`}>{toneText}</div>
    </div>
  );
}

export default function WearCard() {
  const { wear, wearError, selectDisplay } = useLiveState();
  const navigate = useNavigate();

  const handleOpenDetail = (displayName: string) => {
    selectDisplay(displayName);
    navigate("displays");
  };

  const displays = wear?.displays ?? null;

  return (
    <div className="wear-card">
      <div className="wear-card__header">Panel exposure</div>
      <div className="wear-card__caption">on-time, sampling, and compensation status</div>

      {wearError && <div className="wear-card__error">Wear data unavailable: {wearError}</div>}

      {!wearError && displays === null && (
        <div className="wear-card__loading">Loading…</div>
      )}

      {!wearError && displays !== null && displays.length === 0 && (
        <div className="wear-card__empty">No tracked displays yet.</div>
      )}

      {displays !== null &&
        displays.map((d) => (
          <WearRow
            key={d.display_name}
            summary={d}
            tone={wearError ? "error" : d.advisory ? "warning" : "success"}
            onOpenDetail={handleOpenDetail}
          />
        ))}
    </div>
  );
}
