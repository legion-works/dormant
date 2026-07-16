/**
 * Doctor view — diagnostics runner plus the control-path exercise
 * launcher for a chosen display.
 *
 * The doctor report is provider-owned (`useLiveState().doctorReport` /
 * `setDoctorReport`) rather than local `useState` so it survives view
 * navigation the same way the rest of live state does; only the
 * request-in-flight/error bookkeeping for the *current* run stays local.
 *
 * Four summary tiles: passing/warnings/skipped/failing. The real wire
 * `CheckStatus` (crates/dormant-core/src/doctor.rs) is only
 * `"ok" | "fail" | "skip" | "not_supported"` — there is no `"warn"`
 * variant today. `not_supported` folds into "skipped" (a probe that
 * couldn't run is not a failure). "warnings" is computed as
 * `total - passing - skipped - failing` rather than a literal `"warn"`
 * comparison: comparing `CheckStatus` (a closed union) against a string
 * outside that union is a TypeScript error (no overlap), and inventing a
 * status the backend never sends would be dishonest. The subtraction
 * keeps the tile forward-compatible with a future real "warn" status
 * without lying about today's wire shape.
 */
import { useState, useCallback, useRef } from "react";
import { runDoctor } from "../../api/client";
import type { CheckStatus } from "../../api/types";
import { Card, StatusChip, ExerciseRunner } from "../components";
import { useLiveState } from "../hooks/useLiveState";
import "./Doctor.css";

interface CheckIcon {
  icon: string;
  color: string;
  bg: string;
}

function checkIcon(status: CheckStatus): CheckIcon {
  switch (status) {
    case "ok":
      return {
        icon: "✓",
        color: "var(--success)",
        bg: "color-mix(in oklab, var(--success) 13%, transparent)",
      };
    case "fail":
      return {
        icon: "✕",
        color: "var(--danger)",
        bg: "color-mix(in oklab, var(--danger) 13%, transparent)",
      };
    case "skip":
      return {
        icon: "→",
        color: "var(--text-muted)",
        bg: "var(--bg-sunken)",
      };
    case "not_supported":
      return {
        icon: "—",
        color: "var(--text-faint)",
        bg: "var(--bg-sunken)",
      };
    default:
      return {
        icon: "?",
        color: "var(--text-muted)",
        bg: "var(--bg-sunken)",
      };
  }
}

export default function Doctor() {
  const { snapshot, doctorReport, setDoctorReport } = useLiveState();
  const [running, setRunning] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const runningRef = useRef(false);
  const displayIds = snapshot?.displays.map(([id]) => id) ?? [];
  const [selectedDisplay, setSelectedDisplay] = useState<string | null>(null);
  const effectiveDisplay =
    selectedDisplay && displayIds.includes(selectedDisplay) ? selectedDisplay : (displayIds[0] ?? null);

  const handleRun = useCallback(async () => {
    if (runningRef.current) return;
    runningRef.current = true;
    setRunning(true);
    setError(null);
    try {
      const r = await runDoctor();
      setDoctorReport(r);
    } catch (err: unknown) {
      setError(err instanceof Error ? err.message : "Doctor runner failed");
      setDoctorReport(null);
    } finally {
      setRunning(false);
      runningRef.current = false;
    }
  }, [setDoctorReport]);

  const checks = doctorReport?.checks ?? [];
  const passing = checks.filter((c) => c.status === "ok").length;
  const failing = checks.filter((c) => c.status === "fail").length;
  const skipped = checks.filter((c) => c.status === "skip" || c.status === "not_supported").length;
  const warnings = checks.length - passing - skipped - failing;

  const summaryCards = [
    { label: "Passing", count: passing, color: "var(--success)" },
    { label: "Warnings", count: warnings, color: "var(--warning)" },
    { label: "Skipped", count: skipped, color: "var(--text-muted)" },
    { label: "Failing", count: failing, color: "var(--danger)" },
  ];

  return (
    <div className="doctor">
      <div className="doctor-run-row">
        <button
          className="doctor-run-btn"
          onClick={handleRun}
          disabled={running}
        >
          {running ? "Running…" : doctorReport ? "Run again" : "Run doctor"}
        </button>
      </div>

      {error && <div className="doctor-error">Error: {error}</div>}

      {!doctorReport && !running && !error && (
        <div className="doctor-empty">
          Run diagnostics to check daemon environment and integration health.
        </div>
      )}

      {doctorReport && (
        <>
          <div className="doctor-summary">
            {summaryCards.map((s) => (
              <Card key={s.label}>
                <div className="doctor-summary-card">
                  <div className="doctor-summary-card__count" style={{ color: s.color }}>
                    {s.count}
                  </div>
                  <div className="doctor-summary-card__label">{s.label}</div>
                </div>
              </Card>
            ))}
          </div>

          <Card opaque>
            {doctorReport.checks.map((c, i) => {
              const icon = checkIcon(c.status);
              return (
                <div key={`${c.name}-${i}`} className="doctor-check-row">
                  <span
                    className="doctor-check-row__icon"
                    style={{ color: icon.color, backgroundColor: icon.bg }}
                  >
                    {icon.icon}
                  </span>
                  <div className="doctor-check-row__body">
                    <div className="doctor-check-row__title">{c.name}</div>
                    {c.detail && (
                      <div className="doctor-check-row__detail">{c.detail}</div>
                    )}
                  </div>
                  <StatusChip kind={c.status} dot={false} />
                </div>
              );
            })}
          </Card>
        </>
      )}

      {displayIds.length > 0 && effectiveDisplay && (
        <Card className="doctor-exercise-card" opaque>
          <div className="doctor-exercise-card__picker">
            <label className="doctor-exercise-card__label" htmlFor="doctor-exercise-display">
              Exercise display
            </label>
            <select
              id="doctor-exercise-display"
              className="doctor-exercise-card__select"
              value={effectiveDisplay}
              onChange={(e) => setSelectedDisplay(e.target.value)}
            >
              {displayIds.map((id) => (
                <option key={id} value={id}>
                  {id}
                </option>
              ))}
            </select>
          </div>
          <ExerciseRunner display={effectiveDisplay} />
        </Card>
      )}
    </div>
  );
}
