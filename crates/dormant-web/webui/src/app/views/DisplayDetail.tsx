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
} from "../components";
import { postBlank, postWake, postPause, postResume } from "../../api/client";
import type { DisplayConfig, DisplayRuleInfo, DisplaySnapshot, WearDetail } from "../../api/types";
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
        <Card className="display-detail__heat-card">
          <div className="display-detail__heat-map-wrap">
            <WearHeatMap display={id} grid={grid} />
          </div>
          {wear && (
            <div className="display-detail__heat-caption">
              {grid.cols}×{grid.rows} grid · per-cell brightness-weighted on-hours
            </div>
          )}

          <div className="display-detail__metrics">
            {wear ? (
              <>
                <div className="display-detail__metric">{wear.total_on_hours.toFixed(1)}h</div>
                {grid.hasHeatSamples && averagePercent !== null && uniformityPercent !== null ? (
                  <>
                    <div className="display-detail__metric">{averagePercent}% average hotness</div>
                    <div className="display-detail__metric">{uniformityPercent}% uniform</div>
                  </>
                ) : (
                  <div className="display-detail__metric display-detail__metric--muted">
                    Heat metrics unavailable — no valid samples.
                  </div>
                )}
                <div
                  className={`display-detail__advisory display-detail__advisory--${wear.advisory ? "warning" : "success"}`}
                >
                  {wear.advisory
                    ? `no long standby window in ${Math.floor(wear.hours_since_long_dwell / 24)} days`
                    : "compensation window healthy"}
                </div>
              </>
            ) : (
              <div className="display-detail__metric display-detail__metric--muted">
                Heat metrics unavailable — no valid samples.
              </div>
            )}
          </div>
        </Card>

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
              <div className="display-detail__fact-value">Command generation {snapshot.cmd_gen}</div>
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
            <div data-testid="exercise-slot" />
          </div>
        </Card>
      </div>

      {dialog}
    </div>
  );
}
