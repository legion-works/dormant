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
import OwnershipPair from "./OwnershipPair";
import { useCallback, useState, useEffect, useMemo } from "react";
import {
  Card,
  HealthChip,
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

  const averagePercent = grid.averageHeat !== null ? Math.round(grid.averageHeat * 100) : null;
  const uniformityPercent = grid.uniformity !== null ? Math.round(grid.uniformity * 100) : null;

  // Compute current time for staleness + seeded/measured split.
  const nowS = useMemo(() => Math.floor(Date.now() / 1000), []);
  const legendGradient = useMemo(() => heatRampGradient(), []);
  const seededHours = wear?.seeded_usage_hours ?? null;
  const measuredHours = seededHours != null
    ? Math.max(0, (wear?.total_on_hours ?? 0) - seededHours)
    : (wear?.total_on_hours ?? 0);
  const isStale = wear?.last_sample_at_epoch_s != null
    && (nowS - wear.last_sample_at_epoch_s) > 10 * 3600; // 10h default heuristic

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

      {/* ── 1. Control ── */}
      <section id="control" className="display-detail__section">
        <h2 className="display-detail__section-title">Control</h2>
        <Card>
          <div className="display-detail__facts">
            <div className="display-detail__fact">
              <div className="display-detail__fact-label">Phase</div>
              <div className="display-detail__fact-value">{snapshot.phase}</div>
            </div>
            {snapshot.stage && (
              <div className="display-detail__fact">
                <div className="display-detail__fact-label">Stage</div>
                <div className="display-detail__fact-value">
                  {snapshot.stage.idx + 1}/{snapshot.stage.kind.replace(/_/g, " ")}
                </div>
              </div>
            )}
            <div className="display-detail__fact">
              <div className="display-detail__fact-label">Cmd gen</div>
              <div className="display-detail__fact-value">{snapshot.cmd_gen}</div>
            </div>
            <div className="display-detail__fact">
              <div className="display-detail__fact-label">Blank mode</div>
              <div className="display-detail__fact-value">{blankModeLabel(config?.blank_mode)}</div>
            </div>
            {config?.degraded_mode && (
              <div className="display-detail__fact">
                <div className="display-detail__fact-label">Degraded mode</div>
                <div className="display-detail__fact-value">{blankModeLabel(config.degraded_mode)}</div>
              </div>
            )}
            <div className="display-detail__fact">
              <div className="display-detail__fact-label">Driven by</div>
              <div className="display-detail__fact-value">
                {rule ? `${rule.zone} → ${rule.rule}` : "—"}
              </div>
            </div>
            {config?.ladder && config.ladder.length > 0 && (
              <div className="display-detail__fact">
                <div className="display-detail__fact-label">Ladder</div>
                <div className="display-detail__fact-value">{ladderLabel(config.ladder)}</div>
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
              <button
                type="button"
                className="display-detail__action display-detail__action--blank"
                onClick={() => void handleBlank()}
              >
                Force blank
              </button>
              <button
                type="button"
                className="display-detail__action display-detail__action--wake"
                onClick={() => void handleWake()}
              >
                Force wake
              </button>
              {snapshot.paused ? (
                <button
                  type="button"
                  className="display-detail__action display-detail__action--resume"
                  onClick={() => void handleResume()}
                  disabled={!ruleName}
                >
                  Resume rule
                </button>
              ) : (
                <button
                  type="button"
                  className="display-detail__action display-detail__action--pause"
                  onClick={() => void handlePause()}
                  disabled={!ruleName}
                >
                  Pause rule
                </button>
              )}
            </div>
          )}
        </Card>
      </section>

      {/* ── 2. Sharing (shared displays only) ── */}
      {isShared && (
        <section id="sharing" className="display-detail__section">
          <h2 className="display-detail__section-title">Sharing</h2>
          <Card>
            <OwnershipPair
              displayId={id}
              snap={snapshot}
              config={config}
              size="compact"
            />
            <div className="display-detail__sharing-link">
              <button
                type="button"
                className="display-detail__link-btn"
                onClick={() => navigate("switching")}
              >
                open Switching →
              </button>
            </div>
          </Card>
        </section>
      )}

      {/* ── 3. Wear ── */}
      <section id="wear" className="display-detail__section">
        <h2 className="display-detail__section-title">Wear</h2>
        {wear ? (
          <div className="display-detail__wear-grid">
            <Card className="display-detail__heat-card">
              <div className="display-detail__heat-header">
                <div>
                  <div className="display-detail__eyebrow">Panel wear heat map</div>
                  <div className="display-detail__heat-caption">
                    {grid.cols}×{grid.rows} grid · per-cell brightness-weighted on-hours
                  </div>
                </div>
                <span className="display-detail__panel-chip">{panelTypeLabel(wear.panel_type)}</span>
              </div>

              <div className="display-detail__heat-map-wrap">
                <WearHeatMap display={id} grid={grid} />
              </div>

              {grid.hasGridSamples || grid.hasHeatSamples ? (
                <div className="display-detail__legend">
                  <span className="display-detail__legend-label">low</span>
                  <div
                    className="display-detail__legend-bar"
                    style={{ background: legendGradient }}
                  />
                  <span className="display-detail__legend-label">high</span>
                </div>
              ) : null}

              <div className="display-detail__honesty-note">
                v1 attribution is panel-wide and advisory — spatial variation appears only once
                per-region sampling ships.
              </div>
            </Card>

            <Card className="display-detail__exposure-card">
              <div className="display-detail__eyebrow">Exposure summary</div>
              <div className="display-detail__tiles">
                {seededHours != null ? (
                  <>
                    <div className="display-detail__tile">
                      <div className="display-detail__tile-label">Seeded from panel</div>
                      <div className="display-detail__tile-value">{seededHours}h</div>
                    </div>
                    <div className="display-detail__tile">
                      <div className="display-detail__tile-label">dormant-measured</div>
                      <div className="display-detail__tile-value">{measuredHours.toFixed(1)}h</div>
                    </div>
                  </>
                ) : (
                  <div className="display-detail__tile">
                    <div className="display-detail__tile-label">Total on-hours</div>
                    <div className="display-detail__tile-value">{wear.total_on_hours.toFixed(1)}h</div>
                  </div>
                )}
                <div className="display-detail__tile">
                  <div className="display-detail__tile-label">Samples</div>
                  <div className="display-detail__tile-value">{wear.sample_count.toLocaleString()}</div>
                </div>
                <div className={`display-detail__tile${isStale ? " display-detail__tile--warning" : ""}`}>
                  <div className="display-detail__tile-label">Last sample</div>
                  <div className="display-detail__tile-value">
                    {relativeTime(wear.last_sample_at_epoch_s, nowS)}
                  </div>
                </div>
                {isStale && (
                  <div className="display-detail__stale-warning">
                    ledger may be stale — check wear.enabled and the panel read path
                  </div>
                )}
                <div className={`display-detail__tile${wear.advisory ? " display-detail__tile--warning" : ""}`}>
                  <div className="display-detail__tile-label">Since long-dwell</div>
                  <div className="display-detail__tile-value">
                    {Math.floor(wear.hours_since_long_dwell / 24)}d
                  </div>
                </div>
                <div className="display-detail__tile">
                  <div className="display-detail__tile-label">Panel type</div>
                  <div className="display-detail__tile-value">{panelTypeLabel(wear.panel_type)}</div>
                </div>
                <div className={`display-detail__tile${wear.advisory ? " display-detail__tile--warning" : " display-detail__tile--success"}`}>
                  <div className="display-detail__tile-label">Advisory</div>
                  <div className="display-detail__tile-value">{wear.advisory ? "Active" : "Clear"}</div>
                </div>
                {grid.hasHeatSamples && averagePercent !== null && uniformityPercent !== null ? (
                  <>
                    <div className="display-detail__tile">
                      <div className="display-detail__tile-label">Average hotness</div>
                      <div className="display-detail__tile-value">{averagePercent}%</div>
                    </div>
                    <div className="display-detail__tile">
                      <div className="display-detail__tile-label">Uniformity</div>
                      <div className="display-detail__tile-value">{uniformityPercent}%</div>
                    </div>
                  </>
                ) : (
                  <div className="display-detail__metric display-detail__metric--muted">
                    Heat metrics unavailable — no valid samples.
                  </div>
                )}
              </div>
            </Card>
          </div>
        ) : (
          <Card>
            {wearError ? (
              <div className="display-detail__metric display-detail__metric--danger">
                panel exposure unavailable — {wearError}
              </div>
            ) : (
              /* Always render the heat map so the empty-sample state is shown. */
              <div className="display-detail__heat-map-wrap">
                <WearHeatMap display={id} grid={grid} />
              </div>
            )}
          </Card>
        )}
      </section>

      {/* ── 4. Health ── */}
      <section id="health" className="display-detail__section">
        <h2 className="display-detail__section-title">Health</h2>
        <Card>
          <div className="display-detail__facts">
            {snapshot.last_blank_failed && (
              <div className="display-detail__fact">
                <div className="display-detail__fact-label">Last blank</div>
                <div className="display-detail__fact-value display-detail__fact-value--danger">
                  failed
                </div>
              </div>
            )}
            <div className="display-detail__fact">
              <div className="display-detail__fact-label">Wake attempts</div>
              <div className="display-detail__fact-value">{snapshot.wake_attempts ?? 0}</div>
            </div>
          </div>

          {snapshot.controllers.length > 0 && (
            <div className="display-detail__controllers">
              <div className="display-detail__controllers-label">Controller chain (fallback order)</div>
              <div className="display-detail__controllers-list">
                {snapshot.controllers.map((c) => (
                  <div key={c.name} className="display-detail__controller">
                    <HealthChip health={c} />
                    {!c.healthy && c.detail && (
                      <span className="display-detail__controller-detail">{c.detail}</span>
                    )}
                  </div>
                ))}
              </div>
            </div>
          )}

          {snapshot.controllers.length === 0 && (
            <div className="display-detail__metric display-detail__metric--muted">
              no blank/wake attempts recorded yet
            </div>
          )}

          {/* Exercise runner — pre-bound to this display, same component as Doctor. */}
          <div className="display-detail__exercise">
            <ExerciseRunner display={id} compact />
          </div>
        </Card>
      </section>

      {dialog}
    </div>
  );
}
