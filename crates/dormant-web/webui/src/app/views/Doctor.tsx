/**
 * Doctor view — diagnostics runner plus the control-path exercise.
 *
 * Checks group by subject (BG-7 category/subject).  Falls back to a
 * client-side heuristic on `name` when category/subject are absent
 * (older daemons).  The `Other` bucket is always visible.
 *
 * Exercise is promoted to a peer panel; its button is disabled from
 * `operations.exercise_in_flight`, not local state.
 */
import { useState, useCallback, useRef, useEffect } from "react";
import { runDoctor } from "../../api/client";
import type { CheckStatus, Check } from "../../api/types";
import { Card, StatusChip, ExerciseRunner } from "../components";
import { useLiveState } from "../hooks/useLiveState";
import "./Doctor.css";

// ── Check icon mapping (kept verbatim from current Doctor.tsx) ─────────────
interface CheckIcon {
  icon: string;
  color: string;
  bg: string;
}

function checkIcon(status: CheckStatus): CheckIcon {
  switch (status) {
    case "ok":
      return {
        icon: "\u2713",
        color: "var(--success)",
        bg: "color-mix(in oklab, var(--success) 13%, transparent)",
      };
    case "fail":
      return {
        icon: "\u2715",
        color: "var(--danger)",
        bg: "color-mix(in oklab, var(--danger) 13%, transparent)",
      };
    case "skip":
      return {
        icon: "\u2192",
        color: "var(--text-muted)",
        bg: "var(--bg-sunken)",
      };
    case "not_supported":
      return {
        icon: "\u2014",
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

// ── Grouping logic ─────────────────────────────────────────────────────────

interface CheckGroup {
  /** Category label: "Config", "Displays", "Sensors", "Platform", "Other" */
  category: string;
  /** Subject id within the category, or null for category-level checks. */
  subject: string | null;
  /** Display name within the group header. */
  label: string;
  checks: Check[];
  failing: number;
}

/**
 * Group checks by BG-7 category/subject when available.
 * Falls back to a client-side heuristic on `name` for older daemons.
 */
function groupChecks(checks: Check[], displayIds: string[]): CheckGroup[] {
  const groups: CheckGroup[] = [];
  const seen = new Map<string, CheckGroup>();

  const pushGroup = (category: string, subject: string | null, label: string, c: Check) => {
    const key = `${category}\x00${subject ?? ""}`;
    let g = seen.get(key);
    if (!g) {
      g = { category, subject, label, checks: [], failing: 0 };
      seen.set(key, g);
      groups.push(g);
    }
    g.checks.push(c);
    if (c.status === "fail") g.failing++;
  };

  for (const c of checks) {
    // BG-7 path: category/subject present
    if (c.category) {
      const label = c.subject ?? c.category;
      pushGroup(c.category, c.subject ?? null, label, c);
      continue;
    }

    // Heuristic fallback (no category/subject)
    const name = c.name;
    const displayMatch = name.match(/\(([^)]+)\)$/);
    if (displayMatch && displayIds.includes(displayMatch[1])) {
      pushGroup("display", displayMatch[1], displayMatch[1], c);
      continue;
    }
    // Sensor heuristic: "mqtt <id>", "ha <id>", "usb /dev/..."
    const sensorMatch = name.match(/^(mqtt|ha|usb)\s+(.+)/);
    if (sensorMatch) {
      pushGroup("sensor", sensorMatch[2], sensorMatch[2], c);
      continue;
    }
    // Platform heuristic
    if (name.startsWith("macos_") || name === "input-filter") {
      pushGroup("platform", null, "platform", c);
      continue;
    }
    // Config heuristic
    if (name === "config") {
      pushGroup("config", null, "config", c);
      continue;
    }
    // Other bucket
    pushGroup("other", null, "other", c);
  }

  return groups;
}

/** Sort groups: config first, then displays, sensors, platform, other. */
function sortGroups(groups: CheckGroup[]): CheckGroup[] {
  const order: Record<string, number> = {
    config: 0,
    display: 1,
    sensor: 2,
    platform: 3,
    other: 4,
  };
  return [...groups].sort((a, b) => {
    const oa = order[a.category] ?? 99;
    const ob = order[b.category] ?? 99;
    if (oa !== ob) return oa - ob;
    return (a.label).localeCompare(b.label);
  });
}

// ── Component ──────────────────────────────────────────────────────────────

export default function Doctor() {
  const { snapshot, doctorReport, setDoctorReport } = useLiveState();
  const [running, setRunning] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const runningRef = useRef(false);
  // Exercise in-flight from operations — ExerciseRunner reads this internally.
  const displayIds = snapshot?.displays.map(([id]) => id) ?? [];

  // ?subject= query parameter
  const subjectFromHash = (): string | null => {
    const hash = window.location.hash;
    const m = hash.match(/[?&]subject=([^&]+)/);
    return m ? decodeURIComponent(m[1]) : null;
  };
  const [highlightSubject, setHighlightSubject] = useState<string | null>(subjectFromHash);

  useEffect(() => {
    const onHashChange = () => setHighlightSubject(subjectFromHash());
    window.addEventListener("hashchange", onHashChange);
    return () => window.removeEventListener("hashchange", onHashChange);
  }, []);

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

  const summaryCards = [
    { label: "Passing", count: passing, color: "var(--success)" },
    { label: "Failing", count: failing, color: "var(--danger)" },
    { label: "Skipped", count: skipped, color: "var(--text-muted)" },
  ];

  const groups = sortGroups(groupChecks(checks, displayIds));

  // Detect daemon-unresponsive: no display/sensor checks, only network/platform.
  const hasDisplayOrSensorCheck = checks.some(
    (c) => (c.category === "display" || c.category === "sensor") ||
      (!c.category && (c.name.startsWith("display ") || c.name.includes("("))),
  );
  const engineUnresponsive = doctorReport && checks.length > 0 && !hasDisplayOrSensorCheck;

  // Last-run timestamp
  const lastRunLabel = doctorReport
    ? `last run ${new Date().toLocaleTimeString("en-GB", { hour12: false })}`
    : null;

  return (
    <div className="doctor">
      <div className="doctor-run-row">
        <button
          className="doctor-run-btn"
          onClick={handleRun}
          disabled={running}
        >
          {running ? "Running\u2026" : doctorReport ? "Run again" : "Run doctor"}
        </button>
        {lastRunLabel && <span className="doctor-last-run">{lastRunLabel} \u00b7 {checks.length} checks \u00b7 {failing} failing</span>}
      </div>

      {error && <div className="doctor-error">Error: {error}</div>}

      {!doctorReport && !running && !error && (
        <div className="doctor-empty">
          Run diagnostics to check daemon environment and integration health.
        </div>
      )}

      {engineUnresponsive && (
        <div className="doctor-warning">
          daemon did not answer the snapshot request \u2014 checks below cover network probes only
        </div>
      )}

      {doctorReport && checks.length > 0 && (
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

          {/* Grouped checks */}
          {groups.map((g) => {
            const isHighlighted = highlightSubject != null && g.subject === highlightSubject;
            return (
              <div
                key={`${g.category}\x00${g.subject ?? ""}`}
                id={g.subject ? `doctor-subject-${g.subject}` : undefined}
                className={`doctor-group${isHighlighted ? " doctor-group--highlighted" : ""}`}
              >
                <div className="doctor-group__header">
                  <span className="doctor-group__category">{g.category.toUpperCase()}</span>
                  <span className="doctor-group__label">{g.label}</span>
                  <span className="doctor-group__counts">
                    {g.checks.length} check{g.checks.length !== 1 ? "s" : ""}
                    {g.failing > 0 && <span className="doctor-group__failing"> \u00b7 {g.failing} failing</span>}
                  </span>
                </div>

                <Card opaque>
                  {g.checks.map((c, i) => {
                    const icon = checkIcon(c.status);
                    const failing = c.status === "fail";
                    // not_supported gets a specific label
                    const label = c.status === "not_supported" ? "not applicable on this platform" : undefined;
                    return (
                      <div
                        key={`${c.name}-${i}`}
                        className={`doctor-check-row${failing ? " doctor-check-row--failing" : ""}`}
                      >
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
                        <StatusChip kind={c.status} dot={false} label={label} />
                      </div>
                    );
                  })}
                </Card>
              </div>
            );
          })}
        </>
      )}

      {/* Exercise — promoted to peer panel */}
      {displayIds.length > 0 && (
        <div className="doctor-exercise-panel">
          <div className="doctor-group__header">
            <span className="doctor-group__category">CONTROL-PATH EXERCISE</span>
          </div>
          <p className="doctor-exercise-desc">
            Proves a display can actually be blanked and woken. Pauses the
            display\u2019s rule for the window and always restores wake state.
          </p>
          <div className="doctor-exercise-grid">
            {displayIds.map((id) => (
              <Card key={id} opaque className="doctor-exercise-tile">
                <div className="doctor-exercise-tile__header">
                  <span className="doctor-exercise-tile__id">{id}</span>
                </div>
                <ExerciseRunner display={id} />
              </Card>
            ))}
          </div>
        </div>
      )}
    </div>
  );
}
