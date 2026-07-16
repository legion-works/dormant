/**
 * Control-path exercise runner — the real blank → read → wake → read →
 * restore sequence the engine drives directly against a display's
 * controller chain (`POST /api/doctor/exercise/:display`), guarded by the
 * shared `useConfirmDialog`. Reused compact inside `DisplayDetail` (per
 * display) and full-size inside `Doctor` (chosen display).
 *
 * Local single-flight + honest report-timeout handling, identical in shape
 * to `EmergencyWakeControl` — see that file's module doc for the full
 * rationale. Short version: `uiState` tracks this component's own request
 * ("pending" while the POST is in flight). A 504 (report timeout) or 409
 * (already running) response does NOT mean the underlying exercise is
 * idle — the daemon command drives hardware directly and can outlive the
 * HTTP round trip. Both cases move to "uncertain" and start a *dedicated*
 * authoritative `GET /api/operations` poll via `refreshOperations`.
 *
 * Freshness contract (do not simplify): the uncertain state clears ONLY
 * when the live-state provider's `operationsRequestId` — patched via
 * `refreshOperations`'s `onStart` callback, called synchronously before
 * any network I/O — is at least as large as the id recorded when THIS
 * uncertain window began (`uncertainStartedAfterRequestId`), AND the
 * committed `operations.exercise_in_flight` no longer contains `display`.
 * A stale, lower-id observation that resolves late (started before the
 * timeout, commits after) must NOT release the UI even though it reports
 * the guard cleared — only the dedicated request (or a later poll with an
 * equal-or-higher id) may. `StateSnapshot.phase` never participates in
 * this decision; only the authoritative WebState guard endpoint does. Do
 * not send pause/resume requests; the backend engine owns that safety
 * sequence.
 *
 * The remount guard (`uiState === "idle" && serverInFlight`) covers
 * navigation/reload while an operation — possibly started by another tab
 * or client — is still running server-side: this component has no local
 * request to track, but must still refuse to re-enable the button.
 *
 * Known limitations (issue #71): the UI and WebState guards cover
 * duplicate browser requests only. No engine-side serialization,
 * cross-generation operation transfer, CLI+web locking, or panic recovery
 * exists here.
 */
import { useCallback, useEffect, useRef, useState } from "react";
import { ApiError, postExercise } from "../../api/client";
import { useLiveState } from "../hooks/useLiveState";
import { useConfirmDialog } from "./useConfirmDialog";
import type { ExerciseReport, ExerciseStep, ExerciseVerdict, PanelState } from "../../api/types";
import "./ExerciseRunner.css";

export interface ExerciseRunnerProps {
  display: string;
  compact?: boolean;
}

type ExerciseUiState = "idle" | "pending" | "uncertain";

/** Aggregate a step list into a single verdict: failed takes precedence
 * over unconfirmable, which takes precedence over confirmed. An empty
 * step list (never run) is `"not_run"` — distinct from any real verdict. */
export function aggregateExerciseVerdict(steps: ExerciseStep[]): ExerciseVerdict | "not_run" {
  if (steps.length === 0) return "not_run";
  if (steps.some((step) => step.verdict === "failed")) return "failed";
  if (steps.some((step) => step.verdict === "unconfirmable")) return "unconfirmable";
  return "confirmed";
}

/** Format a `PanelState` honestly: only render fields the wire actually
 * sent. `power` alone, `brightness N` alone, `power · brightness N` when
 * both exist, `unknown` when the object/fields are absent. */
function formatPanelState(state: PanelState | undefined): string {
  const hasPower = state?.power !== undefined;
  const hasBrightness = state?.brightness !== undefined;
  if (hasPower && hasBrightness) return `${state!.power} · brightness ${state!.brightness}`;
  if (hasPower) return state!.power!;
  if (hasBrightness) return `brightness ${state!.brightness}`;
  return "unknown";
}

function formatTransition(before: PanelState | undefined, after: PanelState | undefined): string {
  return `${formatPanelState(before)} → ${formatPanelState(after)}`;
}

export default function ExerciseRunner({ display, compact = false }: ExerciseRunnerProps) {
  const [uiState, setUiState] = useState<ExerciseUiState>("idle");
  const [elapsed, setElapsed] = useState(0);
  const [error, setError] = useState<string | null>(null);
  const [report, setReport] = useState<ExerciseReport | null>(null);
  const { operations, operationsRequestId, refreshOperations } = useLiveState();
  const uncertainStartedAfterRequestId = useRef<number | null>(null);
  const { confirm, dialog } = useConfirmDialog();

  const serverInFlight = operations?.exercise_in_flight.includes(display) ?? false;
  const disabled = operations === null || uiState === "pending" || uiState === "uncertain" || serverInFlight;

  const startUncertainReconciliation = useCallback(() => {
    void refreshOperations((requestId) => {
      uncertainStartedAfterRequestId.current = requestId;
    }).catch(() => undefined);
  }, [refreshOperations]);

  // Elapsed-time ticker while a local request is in flight.
  useEffect(() => {
    if (uiState !== "pending") return;
    setElapsed(0);
    const id = setInterval(() => setElapsed((prev) => prev + 1), 1000);
    return () => clearInterval(id);
  }, [uiState]);

  // Freshness-gated release of the uncertain state. See the module doc
  // comment above — this is the load-bearing contract.
  useEffect(() => {
    const startedAfter = uncertainStartedAfterRequestId.current;
    if (
      uiState === "uncertain" &&
      operations &&
      startedAfter !== null &&
      operationsRequestId >= startedAfter &&
      !operations.exercise_in_flight.includes(display)
    ) {
      uncertainStartedAfterRequestId.current = null;
      setError(null);
      setUiState("idle");
    }
  }, [display, operations, operationsRequestId, uiState]);

  const handleClick = useCallback(async () => {
    const accepted = await confirm({
      title: `Exercise ${display}?`,
      description: `Proves the control path end-to-end: blank → read → wake → read → restore. ${display} will visibly go dark and back — do not run while the panel is in use.`,
      confirmLabel: "Run exercise",
      tone: "danger",
    });
    if (!accepted) return;

    try {
      uncertainStartedAfterRequestId.current = null;
      setUiState("pending");
      setElapsed(0);
      setError(null);
      setReport(await postExercise(display));
      uncertainStartedAfterRequestId.current = null;
      setUiState("idle");
    } catch (requestError: unknown) {
      if (requestError instanceof ApiError && requestError.status === 504) {
        setError("Report timed out. Exercise may still be running — do not retry.");
        setUiState("uncertain");
        startUncertainReconciliation();
      } else if (requestError instanceof ApiError && requestError.status === 409) {
        setError("An exercise is already running for this display — do not retry until operation status clears.");
        setUiState("uncertain");
        startUncertainReconciliation();
      } else {
        uncertainStartedAfterRequestId.current = null;
        setError(requestError instanceof Error ? requestError.message : "Exercise failed");
        setUiState("idle");
      }
    }
  }, [confirm, display, startUncertainReconciliation]);

  // Render precedence: live network/probe state first (operations
  // unknown, local pending, remount guard), otherwise nothing.
  // StateSnapshot.phase never participates.
  let statusText: string | null = null;
  if (operations === null) {
    statusText = "Checking operation status…";
  } else if (uiState === "pending") {
    statusText = `Exercise in progress · ${elapsed}s elapsed · display will blink`;
  } else if (uiState === "idle" && serverInFlight) {
    statusText = `Exercise already in progress for ${display}`;
  }

  const verdict = report ? aggregateExerciseVerdict(report.steps) : null;

  return (
    <div className={`exercise-runner${compact ? " exercise-runner--compact" : ""}`}>
      <h3 className="exercise-runner__title">Control-path exercise</h3>
      <p className="exercise-runner__description">
        Proves blank → read → wake → read → restore through the configured controller chain.
      </p>
      <button
        type="button"
        className="exercise-runner__button"
        onClick={() => void handleClick()}
        disabled={disabled}
      >
        Run control-path exercise
      </button>
      {statusText && (
        <div className="exercise-runner__status" role="status">
          {uiState === "pending" && <span className="exercise-runner__spinner" aria-hidden="true" />}
          {statusText}
        </div>
      )}
      {error && (
        <div className="exercise-runner__alert" role="alert">
          {error}
        </div>
      )}
      {report && (
        <div className="exercise-runner__report">
          <div className="exercise-runner__meta">
            <div className="exercise-runner__meta-item">
              <span className="exercise-runner__meta-label">Verdict</span>
              <span className={`exercise-runner__verdict exercise-runner__verdict--${verdict}`}>{verdict}</span>
            </div>
            <div className="exercise-runner__meta-item">
              <span className="exercise-runner__meta-label">Pre-phase</span>
              <span className="exercise-runner__meta-value">{report.pre_phase}</span>
            </div>
            <div className="exercise-runner__meta-item">
              <span className="exercise-runner__meta-label">Paused rules</span>
              <span className="exercise-runner__meta-value">{report.paused_rules?.join(", ") || "none"}</span>
            </div>
          </div>
          <ul className="exercise-runner__steps">
            {report.steps.map((step, i) => (
              <li key={`${step.command}-${i}`} className="exercise-runner__step">
                <span className="exercise-runner__step-command">{step.command}</span>
                {step.blank_mode && (
                  <span className="exercise-runner__step-blank-mode">{step.blank_mode}</span>
                )}
                <span className="exercise-runner__step-returned">
                  {step.returned_ok ? "returned Ok" : "returned error"}
                </span>
                <span className="exercise-runner__step-transition">
                  {formatTransition(step.state_before, step.state_after)}
                </span>
                <span className={`exercise-runner__step-verdict exercise-runner__step-verdict--${step.verdict}`}>
                  {step.verdict}
                </span>
                {step.error && <span className="exercise-runner__step-error">{step.error}</span>}
              </li>
            ))}
          </ul>
        </div>
      )}
      {dialog}
    </div>
  );
}
