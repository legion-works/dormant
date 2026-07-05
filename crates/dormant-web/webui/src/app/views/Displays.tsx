/**
 * Displays view — full per-display detail cards with operator controls.
 *
 * Data: /api/state (phase, inhibited, paused, cmd_gen, controllers[])
 * + /api/config (blank_mode, zone/rule via display_rules reverse lookup).
 *
 * Visual authority: design/web-ui/Dormant Dashboard.dc.html lines 190-248.
 */
import { useDashboardData } from "../hooks/useDashboardData";
import { Card, StatusChip, HealthChip } from "../components";
import { postBlank, postWake, postPause, postResume } from "../../api/client";
import { useCallback, useState } from "react";
import type { DisplaySnapshot } from "../../api/types";
import "./Displays.css";

/* ── Action button ── */

type ActionState = { loading: boolean; error?: string };

/* ── Display detail card ── */

interface DisplayCardProps {
  id: string;
  snap: DisplaySnapshot;
  blankMode: string;
  zone: string;
  rule: string;
}

function DisplayCard({ id, snap, blankMode, zone, rule }: DisplayCardProps) {
  const [blankState, setBlankState] = useState<ActionState>({ loading: false });
  const [wakeState, setWakeState] = useState<ActionState>({ loading: false });
  const [pauseState, setPauseState] = useState<ActionState>({ loading: false });
  const [resumeState, setResumeState] = useState<ActionState>({ loading: false });

  const handleBlank = useCallback(async () => {
    setBlankState({ loading: true });
    try {
      await postBlank(id);
      setBlankState({ loading: false });
    } catch (err: unknown) {
      setBlankState({ loading: false, error: err instanceof Error ? err.message : "Failed" });
    }
  }, [id]);

  const handleWake = useCallback(async () => {
    setWakeState({ loading: true });
    try {
      await postWake(id);
      setWakeState({ loading: false });
    } catch (err: unknown) {
      setWakeState({ loading: false, error: err instanceof Error ? err.message : "Failed" });
    }
  }, [id]);

  const handlePause = useCallback(async () => {
    setPauseState({ loading: true });
    try {
      await postPause({ rule });
      setPauseState({ loading: false });
    } catch (err: unknown) {
      setPauseState({ loading: false, error: err instanceof Error ? err.message : "Failed" });
    }
  }, [rule]);

  const handleResume = useCallback(async () => {
    setResumeState({ loading: true });
    try {
      await postResume({ rule });
      setResumeState({ loading: false });
    } catch (err: unknown) {
      setResumeState({ loading: false, error: err instanceof Error ? err.message : "Failed" });
    }
  }, [rule]);

  // Screen preview glyph by phase
  const previewGlyph = (() => {
    switch (snap.phase) {
      case "active": return "● ON";
      case "grace": return "◐ grace";
      case "blanking": return "◑ …";
      case "blanked": return "○ OFF";
      case "waking": return "◔ wake";
      default: return snap.phase;
    }
  })();

  const phaseIsAlive = snap.phase === "active" || snap.phase === "waking";
  const isPaused = snap.paused;

  const blankModeLabel = blankMode.split("_").map((w) => w[0].toUpperCase() + w.slice(1)).join(" ");

  return (
    <Card className="display-card">
      <div className="display-card__body">
        {/* Screen preview */}
        <div className={`display-preview${phaseIsAlive ? " display-preview--alive" : ""}`}>
          <span className="display-preview__glyph">{previewGlyph}</span>
        </div>

        {/* Details */}
        <div className="display-card__details">
          <div className="display-card__title-row">
            <span className="display-card__id">{id}</span>
            <StatusChip kind={snap.phase} />
            {isPaused && <StatusChip kind="paused" />}
            {snap.inhibited && <StatusChip kind="inhibited" />}
          </div>

          {/* Metric grid */}
          <div className="display-card__metrics">
            <div className="display-metric">
              <div className="display-metric__label">Blank mode</div>
              <div className="display-metric__value">{blankModeLabel}</div>
            </div>
            <div className="display-metric">
              <div className="display-metric__label">Driven by zone</div>
              <div className="display-metric__value">{zone || "—"}</div>
            </div>
            <div className="display-metric">
              <div className="display-metric__label">Rule</div>
              <div className="display-metric__value">{rule || "—"}</div>
            </div>
            <div className="display-metric">
              <div className="display-metric__label">Cmd gen</div>
              <div className="display-metric__value">{snap.cmd_gen}</div>
            </div>
          </div>

          {/* Controller chain */}
          {snap.controllers.length > 0 && (
            <div className="display-card__controllers">
              <div className="display-card__controllers-label">Controller chain (fallback order)</div>
              <div className="display-card__controllers-row">
                {snap.controllers.map((c) => (
                  <HealthChip key={c.name} health={c} />
                ))}
              </div>
            </div>
          )}

          {/* Action error */}
          {(blankState.error || wakeState.error || pauseState.error || resumeState.error) && (
            <div className="display-card__action-error">
              {blankState.error || wakeState.error || pauseState.error || resumeState.error}
            </div>
          )}
        </div>

        {/* Actions column */}
        <div className="display-card__actions">
          <button
            className="display-action display-action--blank"
            onClick={handleBlank}
            disabled={blankState.loading}
          >
            {blankState.loading ? "…" : "Force blank"}
          </button>
          <button
            className="display-action display-action--wake"
            onClick={handleWake}
            disabled={wakeState.loading}
          >
            {wakeState.loading ? "…" : "Force wake"}
          </button>
          {isPaused ? (
            <button
              className="display-action display-action--resume"
              onClick={handleResume}
              disabled={resumeState.loading}
            >
              {resumeState.loading ? "…" : "Resume rule"}
            </button>
          ) : (
            <button
              className="display-action display-action--pause"
              onClick={handlePause}
              disabled={pauseState.loading}
            >
              {pauseState.loading ? "…" : "Pause rule"}
            </button>
          )}
        </div>
      </div>
    </Card>
  );
}

/* ── Main Displays component ── */

export default function Displays() {
  const { loading, error, snapshot, config, displayConfigs, displayRules } = useDashboardData();

  if (loading) {
    return <div className="displays-loading">Loading daemon state…</div>;
  }

  if (error) {
    return <div className="displays-error">Daemon unreachable: {error}</div>;
  }

  if (!snapshot || !config) {
    return <div className="displays-error">No data received from daemon.</div>;
  }

  const { displays } = snapshot;

  return (
    <div className="displays">
      {displays.map(([id, snap]) => {
        const dc = displayConfigs[id];
        const dr = displayRules[id];
        return (
          <DisplayCard
            key={id}
            id={id}
            snap={snap}
            blankMode={dc?.blank_mode ?? "—"}
            zone={dr?.zone ?? "—"}
            rule={dr?.rule ?? "—"}
          />
        );
      })}
    </div>
  );
}
