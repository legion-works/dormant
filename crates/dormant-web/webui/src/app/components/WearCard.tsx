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
 *
 * Per-display polling (issue #185 cycle B): each display row independently
 * polls its own `GET /api/wear/sampling?display=<id>` and evaluates the
 * nudge gate — a two-display setup can show the nudge on one row and not
 * the other, matching per-display consent state.
 */
import { useEffect, useMemo, useState } from "react";
import { useNavigate } from "../nav";
import { useLiveState } from "../hooks/useLiveState";
import {
  getConfig,
  getDaemon,
  getWearSamplingStatusFor,
  postWearSamplingEnableFor,
  postWearSamplingDisableFor,
  postWearSamplingNudgeDismiss,
} from "../../api/client";
import type { WearSamplingStatus, WearSummary } from "../../api/types";
import "./WearCard.css";

type Tone = "success" | "warning" | "error";

interface WearRowProps {
  summary: WearSummary;
  tone: Tone;
  onOpenDetail: (displayName: string) => void;
  // Per-display sampling state (issue #185 cycle B)
  samplingStatus: WearSamplingStatus | null;
  samplingError: string | null;
  samplingEnabled: boolean; // config-level enabled
  onEnable: () => void;
  onDisable: () => void;
  enablePending: boolean;
  // Per-row nudge evaluation
  nudgeVisible: boolean;
  onDismissNudge: () => void;
  dismissingNudge: boolean;
}

function WearRow({
  summary,
  tone,
  onOpenDetail,
  samplingStatus,
  samplingError,
  samplingEnabled,
  onEnable,
  onDisable,
  enablePending,
  nudgeVisible,
  onDismissNudge,
  dismissingNudge,
}: WearRowProps) {
  const days = Math.floor(summary.hours_since_long_dwell / 24);
  const toneText =
    tone === "error"
      ? "wear data unavailable"
      : tone === "warning"
        ? `no long standby window in ${days} days`
        : "compensation window healthy";

  // Sampling action logic per row
  const needsConsent =
    samplingStatus?.status === "error" &&
    samplingStatus.reason === "wear_sampling_needs_consent";
  const portalUnreachable =
    samplingStatus?.status === "error" &&
    samplingStatus.reason === "wear_sampling_portal_unreachable";
  const isGranted = samplingStatus?.status === "granted";
  const isAwaiting = samplingStatus?.status === "awaiting_consent";
  const isDenied = samplingStatus?.status === "denied";
  const isTimedOut = samplingStatus?.status === "timed_out";

  const samplingLabel = needsConsent
    ? "Needs consent"
    : isAwaiting
      ? "Awaiting consent"
      : isGranted
        ? "Granted"
        : isDenied
          ? "Consent denied"
          : isTimedOut
            ? "Consent timed out"
            : samplingStatus?.status === "error"
              ? "Sampling degraded"
              : "Sampling unavailable";

  const age = summary.last_sample_at_epoch_s;
  const ageText =
    age === undefined || age === null
      ? "Last sample: unavailable"
      : `Last sample: ${Math.max(0, Math.floor((Date.now() / 1000 - age) / 60))}m ago`;

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

      {/* Per-display sampling controls (only when config-level enabled) */}
      {samplingEnabled && (
        <div className="wear-row__sampling">
          <span className="wear-row__sampling-state">{samplingLabel}</span>
          <span>{ageText}</span>
          {samplingStatus?.status === "error" && !needsConsent && samplingStatus.reason && (
            <span className="wear-row__sampling-reason">{samplingStatus.reason}</span>
          )}
          {samplingError && (
            <span className="wear-row__sampling-reason">{samplingError}</span>
          )}

          {nudgeVisible && (
            <div
              className="wear-row__nudge"
              data-testid="wear-sampling-nudge"
              role="region"
              aria-label="Active sampling onboarding"
            >
              <div className="wear-row__nudge-text">
                Active sampling is off, so wear estimates assume a uniform image. Grant consent to
                measure content-weighted attribution per display.
              </div>
              <div className="wear-row__nudge-actions">
                <button
                  type="button"
                  onClick={onEnable}
                  disabled={enablePending}
                >
                  Enable active sampling
                </button>
                <button
                  type="button"
                  onClick={onDismissNudge}
                  disabled={dismissingNudge}
                >
                  Dismiss
                </button>
              </div>
            </div>
          )}

          {portalUnreachable && (
            <div className="wear-row__nudge wear-row__nudge--degraded">
              <div className="wear-row__nudge-text">
                The portal consent service is unreachable. Active sampling cannot start until the
                session is restored.
              </div>
              <div className="wear-row__nudge-actions">
                <a href="#/doctor" role="link">Open Doctor</a>
              </div>
            </div>
          )}

          {!nudgeVisible && !portalUnreachable && isGranted && (
            <button
              type="button"
              className="wear-row__disable-btn"
              onClick={onDisable}
            >
              Disable sampling
            </button>
          )}
        </div>
      )}
    </div>
  );
}

export default function WearCard() {
  const { wear, wearError, selectDisplay } = useLiveState();
  const navigate = useNavigate();

  // Config-level state
  const [samplingEnabled, setSamplingEnabled] = useState(false);
  // #186 — platform-capability gate
  const [wearSamplingSupported, setWearSamplingSupported] = useState(false);
  // #186 — persisted dismissal flag (global — dismiss once, applies to all rows)
  const [nudgeDismissed, setNudgeDismissed] = useState(false);
  const [dismissingNudge, setDismissingNudge] = useState(false);

  // Per-display sampling status map (issue #185 cycle B)
  const [perDisplayStatus, setPerDisplayStatus] = useState<
    Record<string, WearSamplingStatus | null>
  >({});
  const [perDisplayErrors, setPerDisplayErrors] = useState<Record<string, string | null>>({});
  const [perDisplayPending, setPerDisplayPending] = useState<Record<string, boolean>>({});

  const displays = wear?.displays ?? null;

  // Load config + daemon capability on mount
  useEffect(() => {
    let cancelled = false;
    void Promise.all([getConfig(), getDaemon()]).then(([config]) => {
      if (cancelled) return;
      setSamplingEnabled(config.inventory.wear?.active_sampling?.enabled === true);
    }).catch(() => {});
    return () => {
      cancelled = true;
    };
  }, []);

  // Fetch per-display daemon flags once
  useEffect(() => {
    let cancelled = false;
    void getDaemon()
      .then((daemon) => {
        if (cancelled) return;
        setWearSamplingSupported(daemon.wear_sampling_supported === true);
        setNudgeDismissed(daemon.wear_sampling_nudge_dismissed === true);
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, []);

  // Poll per-display sampling status for each display independently
  useEffect(() => {
    if (!samplingEnabled || !displays) return;

    const displayIds = displays.map((d) => d.display_name);

    // Initialize status map for new displays
    setPerDisplayStatus((prev) => {
      const next = { ...prev };
      let changed = false;
      for (const id of displayIds) {
        if (!(id in next)) {
          next[id] = null;
          changed = true;
        }
      }
      return changed ? next : prev;
    });

    // Fetch each display's status in parallel
    void Promise.all(
      displayIds.map((displayName) =>
        getWearSamplingStatusFor(displayName)
          .then((status) => {
            setPerDisplayStatus((prev) => ({ ...prev, [displayName]: status }));
            setPerDisplayErrors((prev) => ({ ...prev, [displayName]: null }));
          })
          .catch((err: unknown) => {
            setPerDisplayErrors((prev) => ({
              ...prev,
              [displayName]: err instanceof Error ? err.message : "Unable to fetch status",
            }));
          })
      ),
    ).catch(() => {});
  }, [displays, samplingEnabled]);

  // Per-row polling for "awaiting_consent" state
  useEffect(() => {
    if (!samplingEnabled) return;

    const awaitingDisplays = Object.entries(perDisplayStatus)
      .filter(([, s]) => s?.status === "awaiting_consent")
      .map(([displayName]) => displayName);

    if (awaitingDisplays.length === 0) return;

    const timer = window.setTimeout(() => {
      void Promise.all(
        awaitingDisplays.map((displayName) =>
          getWearSamplingStatusFor(displayName)
            .then((status) => {
              setPerDisplayStatus((prev) => ({ ...prev, [displayName]: status }));
            })
            .catch(() => {}),
        ),
      ).catch(() => {});
    }, 1000);

    return () => {
      window.clearTimeout(timer);
    };
  }, [perDisplayStatus, samplingEnabled]);

  // Per-row nudge evaluation: each display with uniform attribution and
  // needs-consent shows its own nudge independently.
  const nudgeVisibleByDisplay = useMemo(() => {
    if (!samplingEnabled || !wearSamplingSupported || nudgeDismissed || !displays) {
      return {} as Record<string, boolean>;
    }
    const result: Record<string, boolean> = {};
    for (const display of displays) {
      const status = perDisplayStatus[display.display_name];
      const needsConsent =
        status?.status === "error" && status.reason === "wear_sampling_needs_consent";
      const isUniform = display.wear_attribution_mode !== "sampled";
      result[display.display_name] = needsConsent && isUniform;
    }
    return result;
  }, [samplingEnabled, wearSamplingSupported, nudgeDismissed, displays, perDisplayStatus]);

  const handleOpenDetail = (displayName: string) => {
    selectDisplay(displayName);
    navigate("displays");
  };

  const enableForDisplay = (displayName: string) => {
    setPerDisplayPending((prev) => ({ ...prev, [displayName]: true }));
    setPerDisplayErrors((prev) => ({ ...prev, [displayName]: null }));
    void postWearSamplingEnableFor(displayName)
      .then((status) => {
        setPerDisplayStatus((prev) => ({ ...prev, [displayName]: status }));
        setPerDisplayPending((prev) => ({ ...prev, [displayName]: false }));
      })
      .catch((err: unknown) => {
        setPerDisplayErrors((prev) => ({
          ...prev,
          [displayName]: err instanceof Error ? err.message : "Unable to start active sampling",
        }));
        setPerDisplayPending((prev) => ({ ...prev, [displayName]: false }));
      });
  };

  const disableForDisplay = (displayName: string) => {
    setPerDisplayErrors((prev) => ({ ...prev, [displayName]: null }));
    void postWearSamplingDisableFor(displayName, false)
      .then((status) => {
        setPerDisplayStatus((prev) => ({ ...prev, [displayName]: status }));
      })
      .catch((err: unknown) => {
        setPerDisplayErrors((prev) => ({
          ...prev,
          [displayName]: err instanceof Error ? err.message : "Unable to disable active sampling",
        }));
      });
  };

  const dismissNudgeGlobal = async () => {
    if (dismissingNudge) return;
    setDismissingNudge(true);
    try {
      await postWearSamplingNudgeDismiss();
      setNudgeDismissed(true);
    } catch {
      // keep false on failure so the operator can retry
    } finally {
      setDismissingNudge(false);
    }
  };

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
            samplingStatus={perDisplayStatus[d.display_name] ?? null}
            samplingError={perDisplayErrors[d.display_name] ?? null}
            samplingEnabled={samplingEnabled}
            onEnable={() => enableForDisplay(d.display_name)}
            onDisable={() => disableForDisplay(d.display_name)}
            enablePending={perDisplayPending[d.display_name] ?? false}
            nudgeVisible={nudgeVisibleByDisplay[d.display_name] ?? false}
            onDismissNudge={dismissNudgeGlobal}
            dismissingNudge={dismissingNudge}
          />
        ))}
    </div>
  );
}
