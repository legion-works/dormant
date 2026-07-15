/**
 * Emergency wake control — global chrome (mounted once in `Shell`'s
 * topbar; never per-display). Pauses every rule and asks every
 * configured display to wake via `POST /api/emergency-wake`, guarded by
 * the shared `useConfirmDialog`.
 *
 * Local single-flight + honest report-timeout handling: `uiState`
 * tracks this component's own request ("pending" while the POST is in
 * flight). A 504 (report timeout) or 409 (already running) response
 * does NOT mean the underlying engine operation is idle — the daemon
 * command drives hardware directly and can outlive the HTTP round trip.
 * Both cases move to "uncertain" and start a *dedicated* authoritative
 * `GET /api/operations` poll via `refreshOperations`.
 *
 * Freshness contract (do not simplify): the uncertain state clears ONLY
 * when the live-state provider's `operationsRequestId` — patched via
 * `refreshOperations`'s `onStart` callback, called synchronously before
 * any network I/O — is at least as large as the id recorded when THIS
 * uncertain window began (`uncertainStartedAfterRequestId`), AND the
 * committed `operations.emergency_wake_in_flight` is false. A stale,
 * lower-id observation that resolves late (started before the timeout,
 * commits after) must NOT release the UI even though it reports
 * `false` — only the dedicated request (or a later poll with an
 * equal-or-higher id) may. `StateSnapshot.phase` never participates in
 * this decision; only the authoritative WebState guard endpoint does.
 *
 * The remount guard (`uiState === "idle" && serverInFlight`) covers
 * navigation/reload while an operation — possibly started by another
 * tab or client — is still running server-side: this component has no
 * local request to track, but must still refuse to re-enable the
 * button.
 */
import { useCallback, useEffect, useRef, useState } from "react";
import { ApiError, postEmergencyWake } from "../../api/client";
import { useLiveState } from "../hooks/useLiveState";
import { useConfirmDialog } from "./useConfirmDialog";
import "./GlobalBanners.css";

type EmergencyUiState = "idle" | "pending" | "uncertain";
type MessageKind = "status" | "alert";

export default function EmergencyWakeControl() {
  const [uiState, setUiState] = useState<EmergencyUiState>("idle");
  const [elapsed, setElapsed] = useState(0);
  const [message, setMessage] = useState<string | null>(null);
  const [messageKind, setMessageKind] = useState<MessageKind>("status");
  const [failures, setFailures] = useState<string[]>([]);
  const { operations, operationsRequestId, refreshOperations } = useLiveState();
  const uncertainStartedAfterRequestId = useRef<number | null>(null);
  const { confirm, dialog } = useConfirmDialog();

  const serverInFlight = operations?.emergency_wake_in_flight ?? false;
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
      !operations.emergency_wake_in_flight
    ) {
      uncertainStartedAfterRequestId.current = null;
      setMessage("Emergency wake operation completed.");
      setMessageKind("status");
      setUiState("idle");
    }
  }, [operations, operationsRequestId, uiState]);

  const handleClick = useCallback(async () => {
    const accepted = await confirm({
      title: "Emergency wake every display?",
      description:
        "This pauses all rules and asks every configured display to wake. Use it when normal control has failed.",
      confirmLabel: "Wake every display",
      tone: "danger",
    });
    if (!accepted) return;

    setMessage(null);
    setFailures([]);
    setUiState("pending");

    try {
      uncertainStartedAfterRequestId.current = null;
      const report = await postEmergencyWake();
      const woke = report.displays.filter((result) => result.ok).length;
      setMessage(`${woke}/${report.displays.length} displays woke${report.paused ? " · rules paused" : ""}`);
      setMessageKind("status");
      setFailures(
        report.displays
          .filter((result) => !result.ok)
          .map((result) => `${result.display}: ${result.error ?? "wake failed"}`),
      );
      uncertainStartedAfterRequestId.current = null;
      setUiState("idle");
    } catch (error: unknown) {
      if (error instanceof ApiError && error.status === 504) {
        setMessage("Report timed out. Emergency wake may still be running — do not retry.");
        setMessageKind("alert");
        setUiState("uncertain");
        startUncertainReconciliation();
      } else if (error instanceof ApiError && error.status === 409) {
        setMessage("Emergency wake is already running — do not retry until operation status clears.");
        setMessageKind("alert");
        setUiState("uncertain");
        startUncertainReconciliation();
      } else {
        uncertainStartedAfterRequestId.current = null;
        setMessage(error instanceof Error ? error.message : "Emergency wake failed");
        setMessageKind("alert");
        setUiState("idle");
      }
    }
  }, [confirm, startUncertainReconciliation]);

  // Render precedence: live network/probe state first (operations
  // unknown, local pending, remount guard), then the last message this
  // component owns. StateSnapshot.phase never participates.
  let statusText: string | null = null;
  let statusKind: MessageKind = "status";
  if (operations === null) {
    statusText = "Checking operation status…";
  } else if (uiState === "pending") {
    statusText = `Emergency wake in progress · ${elapsed}s elapsed`;
  } else if (uiState === "idle" && serverInFlight) {
    statusText = "Emergency wake already in progress";
  } else if (message) {
    statusText = message;
    statusKind = messageKind;
  }

  return (
    <div className="emergency-wake">
      <button
        type="button"
        className="emergency-wake__button"
        onClick={() => void handleClick()}
        disabled={disabled}
      >
        Emergency wake
      </button>
      {statusText && (
        <div
          className={statusKind === "alert" ? "emergency-wake__alert" : "emergency-wake__status"}
          role={statusKind === "alert" ? "alert" : "status"}
        >
          {uiState === "pending" && <span className="emergency-wake__spinner" aria-hidden="true" />}
          {statusText}
        </div>
      )}
      {failures.length > 0 && (
        <ul className="emergency-wake__failures">
          {failures.map((f) => (
            <li key={f}>{f}</li>
          ))}
        </ul>
      )}
      {dialog}
    </div>
  );
}
