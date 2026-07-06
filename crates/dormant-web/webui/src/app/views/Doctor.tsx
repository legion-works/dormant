/**
 * Doctor view — daemon diagnostics runner.
 *
 * "Run" button triggers POST /api/doctor → renders DoctorReport.checks[].
 * Summary row shows passing / warnings / skipped / failing counts.
 * Each check renders a status icon circle + title + detail + StatusChip tag.
 */
import { useState, useCallback, useRef } from "react";
import { runDoctor } from "../../api/client";
import type { DoctorReport, CheckStatus } from "../../api/types";
import { Card, StatusChip } from "../components";
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
  const [report, setReport] = useState<DoctorReport | null>(null);
  const [running, setRunning] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const runningRef = useRef(false);

  const handleRun = useCallback(async () => {
    if (runningRef.current) return;
    runningRef.current = true;
    setRunning(true);
    setError(null);
    try {
      const r = await runDoctor();
      setReport(r);
    } catch (err: unknown) {
      setError(err instanceof Error ? err.message : "Doctor runner failed");
      setReport(null);
    } finally {
      setRunning(false);
      runningRef.current = false;
    }
  }, []);

  const passing = report ? report.checks.filter((c) => c.status === "ok").length : 0;
  const failing = report ? report.checks.filter((c) => c.status === "fail").length : 0;
  const skipped = report ? report.checks.filter((c) => c.status === "skip" || c.status === "not_supported").length : 0;

  const summaryCards = [
    { label: "Passing", count: passing, color: "var(--success)" },
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
          {running ? "Running…" : report ? "⟳ Run again" : "▶ Run doctor"}
        </button>
      </div>

      {error && <div className="doctor-error">Error: {error}</div>}

      {!report && !running && !error && (
        <div className="doctor-empty">
          Run diagnostics to check daemon environment and integration health.
        </div>
      )}

      {report && (
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
            {report.checks.map((c, i) => {
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
    </div>
  );
}
