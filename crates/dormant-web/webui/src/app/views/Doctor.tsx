/**
 * Doctor view — daemon diagnostics runner.
 *
 * "Run" button triggers POST /api/doctor → renders DoctorReport.checks[].
 * Summary row shows passing/warning/failing counts.  Each check gets a
 * status icon + title + detail + StatusChip tag.
 *
 * Data: POST /api/doctor  |  Visual authority: design README §5 /
 * Dormant Dashboard.dc.html lines 305-328.
 */
import { useState, useCallback, useRef } from "react";
import { runDoctor } from "../../api/client";
import type { DoctorReport, Check } from "../../api/types";
import { Card } from "../components";
import "./Doctor.css";

/** Per-check rendered data with precomputed styling. */
interface CheckRow {
  check: Check;
  icon: string;
  color: string;
  iconBg: string;
  tag: string;
}

function checkRow(check: Check): CheckRow {
  switch (check.status) {
    case "ok":
      return {
        check,
        icon: "✓",
        color: "var(--success)",
        iconBg: "color-mix(in oklab, var(--success) 13%, transparent)",
        tag: "pass",
      };
    case "fail":
      return {
        check,
        icon: "✕",
        color: "var(--danger)",
        iconBg: "color-mix(in oklab, var(--danger) 13%, transparent)",
        tag: "fail",
      };
    case "skip":
      return {
        check,
        icon: "→",
        color: "var(--text-muted)",
        iconBg: "var(--bg-sunken)",
        tag: "skip",
      };
    case "not_supported":
      return {
        check,
        icon: "—",
        color: "var(--text-faint)",
        iconBg: "var(--bg-sunken)",
        tag: "n/a",
      };
    default:
      return {
        check,
        icon: "?",
        color: "var(--text-muted)",
        iconBg: "var(--bg-sunken)",
        tag: check.status,
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

  const rows: CheckRow[] = report ? report.checks.map(checkRow) : [];

  const passing = rows.filter((r) => r.check.status === "ok").length;
  const warnings = rows.filter((r) => r.check.status === "skip").length;
  const failing = rows.filter((r) => r.check.status === "fail" || r.check.status === "not_supported").length;

  const summaryCards = [
    { label: "Passing", count: passing, color: "var(--success)" },
    { label: "Warnings", count: warnings, color: "var(--accent-warm)" },
    { label: "Failing", count: failing, color: "var(--danger)" },
  ];

  return (
    <div className="doctor">
      {/* Run button row */}
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

      {/* Empty state: no report yet */}
      {!report && !running && !error && (
        <div className="doctor-empty">
          Run diagnostics to check daemon environment and integration health.
        </div>
      )}

      {report && (
        <>
          {/* Summary row */}
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

          {/* Checks list */}
          <Card opaque>
            {rows.map((r, i) => (
              <div key={`${r.check.name}-${i}`} className="doctor-check-row">
                <span
                  className="doctor-check-row__icon"
                  style={{ color: r.color, backgroundColor: r.iconBg }}
                >
                  {r.icon}
                </span>
                <div className="doctor-check-row__body">
                  <div className="doctor-check-row__title">{r.check.name}</div>
                  {r.check.detail && (
                    <div className="doctor-check-row__detail">{r.check.detail}</div>
                  )}
                </div>
                <span
                  className="doctor-check-row__tag"
                  style={{ color: r.color, backgroundColor: r.iconBg }}
                >
                  {r.tag}
                </span>
              </div>
            ))}
          </Card>
        </>
      )}
    </div>
  );
}
