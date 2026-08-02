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
 *
 * #186 onboarding nudge: when attribution is `uniform`, active sampling
 * has no consent, and the daemon's platform capability is available, the
 * nudge invites the operator to start the portal flow with a single
 * Enable click and a persistent Dismiss.  Platform capability comes
 * from `GET /api/daemon`'s `wear_sampling_supported` field — NOT from
 * the config `enabled` flag. The two are independent: `enabled` is the
 * user's *intent*, `supported` is the system's *capability*. A macOS
 * user must never see the portal affordance, so the badge is gated on
 * `supported` even when `enabled` is true.
 */
import { useEffect, useMemo, useState } from "react";
import { useNavigate } from "../nav";
import { useLiveState } from "../hooks/useLiveState";
import {
  getConfig,
  getDaemon,
  getWearSamplingStatus,
  postWearSamplingEnable,
  postWearSamplingNudgeDismiss,
} from "../../api/client";
import type { WearSamplingStatus, WearSummary } from "../../api/types";
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
        onClick={() => onOpenDetail(summary.config_display_id ?? summary.display_name)}
      >
        <strong className="wear-row__name">{summary.display_name}</strong>
        <span className="wear-row__stat">{summary.total_on_hours.toFixed(1)}h total on-time</span>
        <span className="wear-row__stat">{summary.sample_count.toLocaleString()} samples</span>
        {summary.content_weighted_since !== undefined && summary.content_weighted_since !== null && (
          <span className="wear-row__stat">
            content-weighted since {new Date(summary.content_weighted_since * 1000).toLocaleDateString()}
          </span>
        )}
      </button>
      <div className={`wear-row__tone wear-row__tone--${tone}`}>{toneText}</div>
    </div>
  );
}

export default function WearCard() {
  const { wear, wearError, selectDisplay } = useLiveState();
  const navigate = useNavigate();
  const [sampling, setSampling] = useState<WearSamplingStatus | null>(null);
  const [samplingEnabled, setSamplingEnabled] = useState(false);
  const [sampledDisplayId, setSampledDisplayId] = useState<string | null>(null);
  const [samplingError, setSamplingError] = useState<string | null>(null);
  // #186 — platform-capability gate. `true` only when the daemon's
  // active-sampling pipeline is actually present on this host
  // (Linux-only: portal + PipeWire). The config `enabled` flag and
  // this are independent.
  const [wearSamplingSupported, setWearSamplingSupported] = useState(false);
  // #186 — persisted dismissal flag (mirrors `star_nudge_dismissed`).
  const [nudgeDismissed, setNudgeDismissed] = useState(false);
  const [dismissing, setDismissing] = useState(false);

  const handleOpenDetail = (displayName: string) => {
    selectDisplay(displayName);
    navigate("displays");
  };

  const displays = wear?.displays ?? null;
  const sampledDisplay = useMemo(() => {
    const configured = wear?.displays.find((display) =>
      display.config_display_id === sampledDisplayId || display.display_name === sampledDisplayId,
    );
    return configured ?? wear?.displays[0] ?? null;
  }, [wear, sampledDisplayId]);

  useEffect(() => {
    let cancelled = false;
    void Promise.all([getConfig(), getWearSamplingStatus(), getDaemon()]).then(
      ([config, status, daemon]) => {
        if (cancelled) return;
        setSamplingEnabled(config.inventory.wear?.active_sampling?.enabled === true);
        setSampledDisplayId(config.inventory.wear?.active_sampling?.sampled_display ?? null);
        setSampling(status);
        setWearSamplingSupported(daemon.wear_sampling_supported === true);
        setNudgeDismissed(daemon.wear_sampling_nudge_dismissed === true);
      },
    ).catch(() => {});
    return () => { cancelled = true; };
  }, []);

  useEffect(() => {
    if (sampling?.status !== "awaiting_consent") return undefined;
    let cancelled = false;
    const timer = window.setTimeout(() => {
      void getWearSamplingStatus().then((status) => {
        if (!cancelled) setSampling(status);
      }).catch(() => {});
    }, 1000);
    return () => { cancelled = true; window.clearTimeout(timer); };
  }, [sampling]);

  const needsConsent = sampling?.status === "error" && sampling.reason === "wear_sampling_needs_consent";
  // Portal/pipewire unreachable — recovered only by operator intervention,
  // so the card replaces the Enable affordance with a link to the Doctor
  // view (#186: degraded states link somewhere useful instead of dead-ending).
  const portalUnreachable =
    sampling?.status === "error" &&
    sampling.reason === "wear_sampling_portal_unreachable";
  const samplingLabel = needsConsent ? "Needs consent"
    : sampling?.status === "awaiting_consent" ? "Awaiting consent"
      : sampling?.status === "granted" ? "Granted"
        : sampling?.status === "denied" ? "Consent denied"
          : sampling?.status === "timed_out" ? "Consent timed out"
            : sampling?.status === "error" ? "Sampling degraded" : "Sampling unavailable";
  const age = sampledDisplay?.last_sample_at_epoch_s;
  const ageText = age === undefined || age === null ? "Last sample: unavailable"
    : `Last sample: ${Math.max(0, Math.floor((Date.now() / 1000 - age) / 60))}m ago`;

  // #186 — nudge visibility gate. Treated as uniform when the field is
  // absent (pre-BG-8 ledgers) — matches the Rust `#[default] Uniform` on
  // `WearAttributionMode`. `sampled` is the only mode that shuts the
  // nudge off.
  const isUniformAttribution = (displays ?? []).some(
    (display) => display.wear_attribution_mode !== "sampled",
  );
  const nudgeVisible =
    wearSamplingSupported &&
    samplingEnabled &&
    isUniformAttribution &&
    needsConsent &&
    !nudgeDismissed;

  const enableSampling = async () => {
    try {
      setSamplingError(null);
      setSampling(await postWearSamplingEnable());
    } catch (error) {
      setSamplingError(error instanceof Error ? error.message : "Unable to start active sampling");
    }
  };

  const dismissNudge = async () => {
    if (dismissing) return;
    setDismissing(true);
    try {
      await postWearSamplingNudgeDismiss();
      setNudgeDismissed(true);
    } catch {
      // keep the flag false on failure so the operator can retry
    } finally {
      setDismissing(false);
    }
  };

  const openDoctor = () => {
    navigate("doctor");
  };

  return (
    <div className="wear-card">
      <div className="wear-card__header">Panel exposure</div>
      <div className="wear-card__caption">on-time, sampling, and compensation status</div>
      <div className="wear-card__sampling">
        <span className="wear-card__sampling-state">{samplingLabel}</span>
        <span>{ageText}</span>
        {sampling?.status === "error" && !needsConsent && sampling.reason && (
          <span className="wear-card__sampling-reason">{sampling.reason}</span>
        )}
        {samplingError && <span className="wear-card__sampling-reason">{samplingError}</span>}
      </div>

      {nudgeVisible && (
        <div
          className="wear-card__nudge"
          data-testid="wear-sampling-nudge"
          role="region"
          aria-label="Active sampling onboarding"
        >
          <div className="wear-card__nudge-text">
            Active sampling is off, so wear estimates assume a uniform image. Grant consent to
            measure content-weighted attribution per display.
          </div>
          <div className="wear-card__nudge-actions">
            <button
              type="button"
              onClick={() => { void enableSampling(); }}
            >
              Enable active sampling
            </button>
            <button
              type="button"
              onClick={() => { void dismissNudge(); }}
              disabled={dismissing}
            >
              Dismiss
            </button>
          </div>
        </div>
      )}

      {portalUnreachable && (
        <div className="wear-card__nudge wear-card__nudge--degraded">
          <div className="wear-card__nudge-text">
            The portal consent service is unreachable. Active sampling cannot start until the
            session is restored.
          </div>
          <div className="wear-card__nudge-actions">
            <a href="#/doctor" role="link" onClick={(e) => { e.preventDefault(); openDoctor(); }}>
              Open Doctor
            </a>
          </div>
        </div>
      )}

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
