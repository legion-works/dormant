/**
 * Display detail view — single-display scannable detail with four
 * anchored sections: Control, Sharing (shared only), Wear, Health.
 *
 * Reached from the Displays list ("Detail →") or from the Overview's
 * panel-exposure card.  Data is passed in by the caller (Displays.tsx
 * reads `useLiveState()` and resolves `config`/`rule`/`wear` for the
 * selected id) — this component has no private fetch.
 *
 * Sections are not tabs — the whole page is scannable and deep-links
 * land visibly.
 */
import { useNavigate } from "../nav";
import { useCallback, useState, useEffect, useMemo } from "react";
import {
  Card,
  StatusChip,
  phaseChipLabel,
  useConfirmDialog,
  normalizeWearGrid,
  WearHeatMap,
  HEAT_RAMP_STOPS,
  ExerciseRunner,
} from "../components";
import { postBlank, postWake, postPause, postResume } from "../../api/client";
import type { DisplayConfig, DisplayRuleInfo, DisplaySnapshot, PanelType, WearDetail } from "../../api/types";
import "./DisplayDetail.css";

export interface DisplayDetailProps {
  id: string;
  snapshot: DisplaySnapshot;
  config: DisplayConfig | undefined;
  rule: DisplayRuleInfo | undefined;
  wear: WearDetail | undefined;
  wearError: string | null;
  onBack: () => void;
}

function blankModeLabel(mode: string | undefined): string {
  if (!mode) return "—";
  return mode.split("_").map((w) => w[0].toUpperCase() + w.slice(1)).join(" ");
}

const PANEL_TYPE_LABELS: Record<PanelType, string> = {
  woled: "WOLED",
  "qd-oled": "QD-OLED",
  unknown: "Unknown",
};

function panelTypeLabel(panelType: PanelType): string {
  return PANEL_TYPE_LABELS[panelType] ?? panelType;
}

/** Scroll to and briefly highlight a named anchor section. */
function useScrollToAnchor(anchor: string | null) {
  useEffect(() => {
    if (!anchor) return;
    const timer = setTimeout(() => {
      const el = document.getElementById(anchor);
      if (!el) return;
      el.scrollIntoView({ behavior: "smooth", block: "start" });
      el.style.transition = "background-color 0.15s ease";
      el.style.backgroundColor = "var(--accent-warm-muted)";
      setTimeout(() => {
        el.style.backgroundColor = "";
      }, 1200);
    }, 150);
    return () => clearTimeout(timer);
  }, [anchor]);
}

/** Read the secondary fragment from the URL hash — e.g. #/displays/studio#wear → "wear". */
function getAnchorFromHash(): string | null {
  const hash = window.location.hash;
  const secondHash = hash.indexOf("#", 1);
  if (secondHash === -1) return null;
  return hash.slice(secondHash + 1) || null;
}

/** Generate a CSS gradient from HEAT_RAMP_STOPS — the same source heatColor()
 *  uses, so the legend cannot drift from the heat-map ramp. */
function heatRampGradient(): string {
  const stops = HEAT_RAMP_STOPS.map(([v, r, g, b]) => {
    const pct = Math.round(v * 100);
    return `rgba(${r}, ${g}, ${b}, 1) ${pct}%`;
  });
  return `linear-gradient(90deg, ${stops.join(", ")})`;
}

/** Seconds-ago → human-readable relative duration (max precision: hours). */
function relativeTime(epochS: number | null | undefined, nowS: number): string {
  if (epochS == null) return "—";
  const delta = Math.max(0, nowS - epochS);
  if (delta < 60) return `${delta}s ago`;
  if (delta < 3600) return `${Math.floor(delta / 60)}m ago`;
  if (delta < 86400) return `${Math.floor(delta / 3600)}h ago`;
  return `${Math.floor(delta / 86400)}d ago`;
}

/** Ladder stage display label. */
function ladderLabel(ladder: DisplayConfig["ladder"]): string {
  if (!ladder || ladder.length === 0) return "—";
  return ladder
    .map((s) => {
      const parts = [s.kind.replace(/_/g, " ")];
      if (s.dwell) parts.push(s.dwell);
      return parts.join(" · ");
    })
    .join(" → ");
}

export default function DisplayDetail({ id, snapshot, config, rule, wear, wearError, onBack }: DisplayDetailProps) {
  const { confirm, dialog } = useConfirmDialog();
  const [actionError, setActionError] = useState<string | null>(null);
  const navigate = useNavigate();

  const grid = normalizeWearGrid(wear);
  const ruleName = rule?.rule;
  const isShared = snapshot.scope === "shared";

  // Scroll to anchor on mount (e.g. #/displays/{id}#wear).
  useScrollToAnchor(getAnchorFromHash());

  const handleBlank = useCallback(async () => {
    const accepted = await confirm({
      title: `Force blank ${id}?`,
      description: `Immediately blanks ${id}, bypassing the normal presence rules.`,
      confirmLabel: "Force blank",
      tone: "danger",
    });
    if (!accepted) return;
    setActionError(null);
    try {
      await postBlank(id);
    } catch (err: unknown) {
      setActionError(err instanceof Error ? err.message : "Force blank failed");
    }
  }, [confirm, id]);

  const handleWake = useCallback(async () => {
    setActionError(null);
    try {
      await postWake(id);
    } catch (err: unknown) {
      setActionError(err instanceof Error ? err.message : "Force wake failed");
    }
  }, [id]);

  const handlePause = useCallback(async () => {
    if (!ruleName) return;
    const accepted = await confirm({
      title: `Pause ${ruleName}?`,
      description: `Pauses rule "${ruleName}" until manually resumed.`,
      confirmLabel: "Pause rule",
    });
    if (!accepted) return;
    setActionError(null);
    try {
      await postPause({ rule: ruleName });
    } catch (err: unknown) {
      setActionError(err instanceof Error ? err.message : "Pause rule failed");
    }
  }, [confirm, ruleName]);

  const handleResume = useCallback(async () => {
    if (!ruleName) return;
    setActionError(null);
    try {
      await postResume({ rule: ruleName });
    } catch (err: unknown) {
      setActionError(err instanceof Error ? err.message : "Resume rule failed");
    }
  }, [ruleName]);

  // Compute current time for staleness + seeded/measured split.
  const nowS = useMemo(() => Math.floor(Date.now() / 1000), []);
  const legendGradient = useMemo(() => heatRampGradient(), []);
  const seededHours = wear?.seeded_usage_hours ?? null;
  const measuredHours = seededHours != null
    ? Math.max(0, (wear?.total_on_hours ?? 0) - seededHours)
    : (wear?.total_on_hours ?? 0);

  return (
    <div className="display-detail">
      <button type="button" className="display-detail__back" onClick={onBack}>
        ← Displays
      </button>

      <div className="display-detail__header">
        <span className="display-detail__id">{id}</span>
        <StatusChip kind={snapshot.phase} label={phaseChipLabel(snapshot.phase, snapshot.stage)} />
        {snapshot.paused && <StatusChip kind="paused" />}
        {snapshot.inhibited && <StatusChip kind="inhibited" />}
        {snapshot.scope === "shared" && (
          <span className="display-detail__shared-badge" title="shared display">⇄ shared</span>
        )}
      </div>

      {/* Two-column: CONTROL (left) + HEALTH (right) — screens/04 */}
      <div className="display-detail__top-row">
        {/* CONTROL */}
        <Card>
          <div className="display-detail__eyebrow">Control</div>
          <StatusChip kind={snapshot.phase} label={phaseChipLabel(snapshot.phase, snapshot.stage)} />
          <span className="display-detail__cmd-gen">cmd gen {snapshot.cmd_gen}</span>

          {/* State box */}
          <div className={`display-detail__state-box${snapshot.phase === "active" ? " display-detail__state-box--active" : ""}`}>
            {snapshot.phase === "active" ? "● ON" : "○ OFF"}
          </div>

          {/* Fact rows */}
          <div className="display-detail__fact-rows">
            <div className="display-detail__fact-row">
              <span className="display-detail__fact-label">Blank mode</span>
              <span className="display-detail__fact-value">{blankModeLabel(config?.blank_mode)}</span>
            </div>
            {config?.degraded_mode && (
              <div className="display-detail__fact-row">
                <span className="display-detail__fact-label">Degraded</span>
                <span className="display-detail__fact-value">{blankModeLabel(config.degraded_mode)}</span>
              </div>
            )}
            <div className="display-detail__fact-row">
              <span className="display-detail__fact-label">Driven by</span>
              <span className="display-detail__fact-value">
                {rule ? `${rule.zone} → ${rule.rule}` : "—"}
              </span>
            </div>
            {config?.ladder && config.ladder.length > 0 && (
              <div className="display-detail__fact-row">
                <span className="display-detail__fact-label">Ladder</span>
                <span className="display-detail__fact-value">{ladderLabel(config.ladder)}</span>
              </div>
            )}
          </div>

          {(snapshot.wake_attempts ?? 0) > 0 && (
            <div className="display-detail__wake-warning">
              wake retry {snapshot.wake_attempts} in progress
            </div>
          )}

          {actionError && (
            <div className="display-detail__action-error">{actionError}</div>
          )}

          {!dialog && (
            <div className="display-detail__controls">
              <button type="button" className="display-detail__action" onClick={() => void handleBlank()}>
                Force blank
              </button>
              <button type="button" className="display-detail__action display-detail__action--wake" onClick={() => void handleWake()}>
                Force wake
              </button>
              {snapshot.paused ? (
                <button type="button" className="display-detail__action display-detail__action--resume" onClick={() => void handleResume()} disabled={!ruleName}>
                  Resume rule
                </button>
              ) : (
                <button type="button" className="display-detail__action display-detail__action--pause" onClick={() => void handlePause()} disabled={!ruleName}>
                  Pause rule
                </button>
              )}
            </div>
          )}

          {/* Sharing link (shared displays only) */}
          {isShared && (
            <div className="display-detail__sharing-link">
              <button type="button" className="display-detail__link-btn" onClick={() => navigate("switching")}>
                open Switching →
              </button>
            </div>
          )}
        </Card>

        {/* HEALTH */}
        <Card>
          <div className="display-detail__eyebrow">Health</div>
          <span className="display-detail__health-meta">controller chain · fallback order</span>

          {/* Controller chain rows */}
          <div className="display-detail__controllers-list">
            {snapshot.controllers.length > 0 ? (
              snapshot.controllers.map((c) => (
                <div key={c.name} className={`display-detail__controller-row${c.healthy ? "" : " display-detail__controller-row--failing"}`}>
                  <span className={`display-detail__controller-dot${c.healthy ? " display-detail__controller-dot--healthy" : " display-detail__controller-dot--failing"}`} />
                  <div className="display-detail__controller-info">
                    <span className="display-detail__controller-name">{c.name}</span>
                    {!c.healthy && c.detail && (
                      <span className="display-detail__controller-detail">{c.detail}</span>
                    )}
                    {c.healthy && (
                      <span className="display-detail__controller-detail">primary · last attempt succeeded</span>
                    )}
                  </div>
                  <span className={`display-detail__controller-status${c.healthy ? " display-detail__controller-status--ok" : " display-detail__controller-status--fail"}`}>
                    {c.healthy ? "healthy" : "failing"}
                  </span>
                </div>
              ))
            ) : (
              <div className="display-detail__metric display-detail__metric--muted">
                no blank/wake attempts recorded yet
              </div>
            )}
          </div>

          {/* Exercise runner */}
          <div className="display-detail__exercise">
            <ExerciseRunner display={id} compact />
          </div>
        </Card>
      </div>

      {/* PANEL EXPOSURE — full width below */}
      {wear ? (
        <Card className="display-detail__exposure-card">
          <div className="display-detail__exposure-layout">
            <div className="display-detail__exposure-heat">
              <div className="display-detail__heat-header">
                <div>
                  <div className="display-detail__eyebrow">Panel exposure</div>
                  <div className="display-detail__heat-caption">
                    {grid.cols}×{grid.rows} grid · brightness-weighted on-hours
                  </div>
                </div>
              </div>

              <div className="display-detail__heat-map-wrap">
                <WearHeatMap display={id} grid={grid} />
              </div>

              {grid.hasGridSamples || grid.hasHeatSamples ? (
                <div className="display-detail__legend">
                  <span className="display-detail__legend-label">cool</span>
                  <div className="display-detail__legend-bar" style={{ background: legendGradient }} />
                  <span className="display-detail__legend-label">hot</span>
                </div>
              ) : null}
            </div>

            <div className="display-detail__exposure-stats">
              <span className="display-detail__panel-chip">{panelTypeLabel(wear.panel_type)}</span>
              <StatusChip kind={wear.advisory ? "wear_advisory" : "ok"} label={wear.advisory ? "advisory" : "no advisory"} />

              <div className="display-detail__exposure-rows">
                <div className="display-detail__exposure-row">
                  <span className="display-detail__exposure-label">Total on-hours</span>
                  <span className="display-detail__exposure-value">{wear.total_on_hours.toFixed(0)}</span>
                </div>
                {seededHours != null && (
                  <>
                    <div className="display-detail__exposure-row">
                      <span className="display-detail__exposure-label">Seeded from panel</span>
                      <span className="display-detail__exposure-value">{seededHours}</span>
                      <span className="display-detail__exposure-hint">VCP 0xC0</span>
                    </div>
                    <div className="display-detail__exposure-row">
                      <span className="display-detail__exposure-label">dormant-measured</span>
                      <span className="display-detail__exposure-value">{measuredHours.toFixed(0)}</span>
                    </div>
                  </>
                )}
                <div className="display-detail__exposure-row">
                  <span className="display-detail__exposure-label">Samples</span>
                  <span className="display-detail__exposure-value">{wear.sample_count.toLocaleString()}</span>
                </div>
                <div className="display-detail__exposure-row">
                  <span className="display-detail__exposure-label">Last sample</span>
                  <span className="display-detail__exposure-value">
                    {relativeTime(wear.last_sample_at_epoch_s, nowS)}
                  </span>
                </div>
                <div className="display-detail__exposure-row">
                  <span className="display-detail__exposure-label">Since long dwell</span>
                  <span className="display-detail__exposure-value">{Math.floor(wear.hours_since_long_dwell / 24)}d</span>
                  <span className="display-detail__exposure-hint">advisory at 48h</span>
                </div>
              </div>
            </div>
          </div>
        </Card>
      ) : (
        <Card>
          {wearError ? (
            <div className="display-detail__metric display-detail__metric--danger">
              panel exposure unavailable — {wearError}
            </div>
          ) : (
            <div className="display-detail__heat-map-wrap">
              <WearHeatMap display={id} grid={grid} />
            </div>
          )}
        </Card>
      )}

      {dialog}
    </div>
  );
}
