/**
 * Display detail view — single-display wear heat map, summaries, and
 * guarded controls. Reached from the Displays list ("Open detail →") or
 * from the Dashboard's panel-exposure card ("Open <id> panel detail").
 *
 * Data is passed in by the caller (Displays.tsx reads `useLiveState()`
 * and resolves `config`/`rule`/`wear` for the selected id) — this
 * component has no private fetch and renders exactly what it is given.
 *
 * Honesty rules (do not invent facts the backend doesn't expose):
 *   - No last-command timestamp exists on the wire — only `cmd_gen`.
 *   - No per-display emergency wake exists — that is global chrome only
 *     (`EmergencyWakeControl`, mounted once in Shell's topbar).
 *   - When the wear grid has no valid heat samples, the metrics panel
 *     says so explicitly instead of rendering 0%/NaN.
 */
import { useCallback, useState } from "react";
import {
  Card,
  HealthChip,
  StatusChip,
  phaseChipLabel,
  useConfirmDialog,
  normalizeWearGrid,
  WearHeatMap,
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
  onBack: () => void;
}

function blankModeLabel(mode: string | undefined): string {
  if (!mode) return "—";
  return mode.split("_").map((w) => w[0].toUpperCase() + w.slice(1)).join(" ");
}

/** rust: wear.rs PanelType — display label for the heat-card chip. */
const PANEL_TYPE_LABELS: Record<PanelType, string> = {
  woled: "WOLED",
  "qd-oled": "QD-OLED",
  unknown: "Unknown",
};

function panelTypeLabel(panelType: PanelType): string {
  return PANEL_TYPE_LABELS[panelType] ?? panelType;
}

interface ExposureTileProps {
  label: string;
  value: string;
  tone?: "warning" | "success";
}

/** One metric tile in the Exposure summary 2-col grid (F3). */
function ExposureTile({ label, value, tone }: ExposureTileProps) {
  return (
    <div className={`display-detail__tile${tone ? ` display-detail__tile--${tone}` : ""}`}>
      <div className="display-detail__tile-label">{label}</div>
      <div className="display-detail__tile-value">{value}</div>
    </div>
  );
}

export default function DisplayDetail({ id, snapshot, config, rule, wear, onBack }: DisplayDetailProps) {
  const { confirm, dialog } = useConfirmDialog();
  const [actionError, setActionError] = useState<string | null>(null);

  const grid = normalizeWearGrid(wear);
  const ruleName = rule?.rule;

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
    const accepted = await confirm({
      title: `Force wake ${id}?`,
      description: `Immediately wakes ${id}, bypassing the normal presence rules.`,
      confirmLabel: "Force wake",
    });
    if (!accepted) return;
    setActionError(null);
    try {
      await postWake(id);
    } catch (err: unknown) {
      setActionError(err instanceof Error ? err.message : "Force wake failed");
    }
  }, [confirm, id]);

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
    const accepted = await confirm({
      title: `Resume ${ruleName}?`,
      description: `Resumes rule "${ruleName}" immediately.`,
      confirmLabel: "Resume rule",
    });
    if (!accepted) return;
    setActionError(null);
    try {
      await postResume({ rule: ruleName });
    } catch (err: unknown) {
      setActionError(err instanceof Error ? err.message : "Resume rule failed");
    }
  }, [confirm, ruleName]);

  const averagePercent = grid.averageHeat !== null ? Math.round(grid.averageHeat * 100) : null;
  const uniformityPercent = grid.uniformity !== null ? Math.round(grid.uniformity * 100) : null;

  return (
    <div className="display-detail">
      <button type="button" className="display-detail__back" onClick={onBack}>
        ← Displays
      </button>

      <div className="display-detail__columns">
        <div className="display-detail__left-column">
          <Card className="display-detail__heat-card">
            <div className="display-detail__heat-header">
              <div>
                <div className="display-detail__eyebrow">Panel wear heat map</div>
                {wear && (
                  <div className="display-detail__heat-caption">
                    {grid.cols}×{grid.rows} grid · per-cell brightness-weighted on-hours
                  </div>
                )}
              </div>
              {wear && (
                <span className="display-detail__panel-chip">{panelTypeLabel(wear.panel_type)}</span>
              )}
            </div>

            <div className="display-detail__heat-map-wrap">
              <WearHeatMap display={id} grid={grid} />
            </div>

            {grid.hasGridSamples || grid.hasHeatSamples ? (
              <div className="display-detail__legend">
                <span className="display-detail__legend-label">low</span>
                <div className="display-detail__legend-bar" />
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
            {wear ? (
              <div className="display-detail__tiles">
                <ExposureTile label="Total on-hours" value={`${wear.total_on_hours.toFixed(1)}h`} />
                <ExposureTile
                  label="Seeded prior"
                  value={
                    wear.seeded_usage_hours != null
                      ? `${wear.seeded_usage_hours}h · VCP 0xC0 seed`
                      : "not seeded"
                  }
                />
                <ExposureTile label="Samples" value={wear.sample_count.toLocaleString()} />
                <ExposureTile
                  label="Since long-dwell"
                  value={`${Math.floor(wear.hours_since_long_dwell / 24)}d`}
                  tone={wear.advisory ? "warning" : undefined}
                />
                <ExposureTile label="Panel type" value={panelTypeLabel(wear.panel_type)} />
                <ExposureTile
                  label="Advisory"
                  value={wear.advisory ? "Active" : "Clear"}
                  tone={wear.advisory ? "warning" : "success"}
                />
                {grid.hasHeatSamples && averagePercent !== null && uniformityPercent !== null ? (
                  <>
                    <ExposureTile label="Average hotness" value={`${averagePercent}%`} />
                    <ExposureTile label="Uniformity" value={`${uniformityPercent}%`} />
                  </>
                ) : (
                  <div className="display-detail__metric display-detail__metric--muted">
                    Heat metrics unavailable — no valid samples.
                  </div>
                )}
              </div>
            ) : (
              <div className="display-detail__metric display-detail__metric--muted">
                Heat metrics unavailable — no valid samples.
              </div>
            )}
          </Card>
        </div>

        <Card className="display-detail__info-card">
          <div className="display-detail__title-row">
            <span className="display-detail__id">{id}</span>
            <StatusChip kind={snapshot.phase} label={phaseChipLabel(snapshot.phase, snapshot.stage)} />
            {snapshot.paused && <StatusChip kind="paused" />}
            {snapshot.inhibited && <StatusChip kind="inhibited" />}
          </div>

          <div className="display-detail__facts">
            <div className="display-detail__fact">
              <div className="display-detail__fact-label">Blank mode</div>
              <div className="display-detail__fact-value">{blankModeLabel(config?.blank_mode)}</div>
            </div>
            <div className="display-detail__fact">
              <div className="display-detail__fact-label">Driven by zone</div>
              <div className="display-detail__fact-value">{rule?.zone ?? "—"}</div>
            </div>
            <div className="display-detail__fact">
              <div className="display-detail__fact-label">Rule</div>
              <div className="display-detail__fact-value">{rule?.rule ?? "—"}</div>
            </div>
            <div className="display-detail__fact">
              <div className="display-detail__fact-label">Cmd gen</div>
              <div className="display-detail__fact-value">{snapshot.cmd_gen}</div>
            </div>
          </div>

          {snapshot.controllers.length > 0 && (
            <div className="display-detail__controllers">
              <div className="display-detail__controllers-label">Controller chain (fallback order)</div>
              <div className="display-detail__controllers-row">
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

          {actionError && <div className="display-detail__action-error">{actionError}</div>}

          <div className="display-detail__controls">
            {!dialog && (
              <>
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
              </>
            )}
          </div>
          <ExerciseRunner display={id} compact />
        </Card>
      </div>

      {dialog}
    </div>
  );
}
