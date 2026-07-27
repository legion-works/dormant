/**
 * Pull/push state machine — the interactive switching controls for a
 * shared display.
 *
 * Six pull states (idle → writing → verified / unverified / aborted / failed)
 * and five push states (idle → writing → released / ignored / degraded / failed).
 *
 * Uses optimistic UI during the POST flight, then reconciles against the
 * snapshot poll and the BG-1 `DaemonEvent::Ownership` stream.
 *
 * Source: flows/kvm-pull-push.md
 */
import { useState, useCallback, useEffect, useRef } from "react";
import type { OwnershipEvent, CoordinationConfig } from "../../api/types";
import { useEventLog, useLiveState } from "../hooks/useLiveState";
import { postSwitch, postPush } from "../../api/client";
import "./SwitchState.css";

// ── Types ───────────────────────────────────────────────────────────────────

type PullState =
  | { kind: "idle" }
  | { kind: "writing"; writeCode: number }
  | { kind: "verified" }
  | { kind: "unverified" }
  | { kind: "aborted"; reason: string }
  | { kind: "failed"; error: string };

type PushState =
  | { kind: "idle" }
  | { kind: "writing"; writeCode: number }
  | { kind: "released" }
  | { kind: "ignored" }
  | { kind: "degraded" }
  | { kind: "failed"; error: string };

export interface SwitchStateProps {
  displayId: string;
  switchCapable: boolean;
  pushCapable: boolean;
  /** Local write code (used in pull button copy). */
  localWriteCode: number | undefined;
  /** Peer write code (used in push button copy). */
  peerWriteCode: number | undefined;
  /** Whether this machine currently owns the display. */
  owned: boolean | undefined;
  /** Last observed input code from the snapshot. */
  observedInputCode: number | null | undefined;
  /** Coordination config for poll interval (optimistic fallback). */
  coordination?: CoordinationConfig;
}

// ── Helpers ────────────────────────────────────────────────────────────────

function hexPad(code: number | undefined): string {
  if (code == null) return "??";
  return `0x${code.toString(16)}`;
}

/** Classify a fetch error into the spec's error-copy table. */
function classifyError(err: unknown): string {
  if (err instanceof Error) {
    const msg = err.message;
    // The API client wraps fetch errors in ApiError with status/body.
    // Check for known patterns.
    if (msg.includes("409")) return "another switch is in flight";
    // "the daemon did not answer" — any network error from the fetch layer.
    if (msg.includes("fetch") || msg.includes("Network") || msg.includes("Failed to fetch"))
      return "the daemon did not answer — the panel was not touched";
    return msg;
  }
  return "unknown error";
}

// ── Component ──────────────────────────────────────────────────────────────

export default function SwitchState({
  displayId,
  switchCapable,
  pushCapable,
  localWriteCode,
  peerWriteCode,
  owned: _owned,
  observedInputCode,
  coordination,
}: SwitchStateProps) {
  const owned = _owned;
  const [pullState, setPullState] = useState<PullState>({ kind: "idle" });
  const [pushState, setPushState] = useState<PushState>({ kind: "idle" });
  const pullTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const { events } = useEventLog();
  const { snapshot } = useLiveState();

  // ── BG-1 Ownership event reconciliation (pull + push) ──────────────────
  useEffect(() => {
    if (events.length === 0) return;
    const latestEvent = events[0]?.event;
    if (!latestEvent || latestEvent.event !== "ownership") return;
    const oe = latestEvent as OwnershipEvent;
    if (oe.display !== displayId) return;

    // Pull path
    if (oe.cause === "pull" || oe.cause === "activity_follow" ||
        oe.cause === "hotkey" || oe.cause === "cli" ||
        oe.cause === "web" || oe.cause === "tray" || oe.cause === "toggle") {
      if (oe.verified === true) {
        setPullState({ kind: "verified" });
        if (pullTimerRef.current != null) {
          clearTimeout(pullTimerRef.current);
          pullTimerRef.current = null;
        }
      } else if (oe.verified === false) {
        setPullState({ kind: "unverified" });
        if (pullTimerRef.current != null) {
          clearTimeout(pullTimerRef.current);
          pullTimerRef.current = null;
        }
      }
    }

    // Push path
    if (oe.cause === "push") {
      if (oe.verified === true) {
        if (oe.degraded) {
          setPushState({ kind: "degraded" });
        } else {
          setPushState({ kind: "released" });
        }
      } else if (oe.verified === false) {
        setPushState({ kind: "ignored" });
      }
    }
  }, [events, displayId]);

  // ── Pull handler ────────────────────────────────────────────────────────
  const handlePull = useCallback(async () => {
    const wc = localWriteCode ?? 0x00;
    setPullState({ kind: "writing", writeCode: wc });

    // Optimistic poll reconciliation: after 2 × poll_interval, check snapshot.
    const pollMs = coordination?.poll_interval
      ? parseFloat(coordination.poll_interval) * 1000
      : 2000;
    pullTimerRef.current = setTimeout(() => {
      const snap = snapshot;
      const displaySnap = snap?.displays?.find(([id]) => id === displayId)?.[1];
      if (displaySnap?.owned === true) {
        // Snapshot confirms ownership — transition to verified.
        setPullState({ kind: "verified" });
      } else {
        // After 2 × poll_interval without confirmation, fall to unverified.
        setPullState({ kind: "unverified" });
      }
    }, Math.max(pollMs * 2, 2500));

    try {
      await postSwitch(displayId);
    } catch (err: unknown) {
      if (pullTimerRef.current != null) {
        clearTimeout(pullTimerRef.current);
        pullTimerRef.current = null;
      }
      setPullState({ kind: "failed", error: classifyError(err) });
    }
  }, [displayId, localWriteCode, coordination?.poll_interval, snapshot]);

  // ── Push handler ────────────────────────────────────────────────────────
  const handlePush = useCallback(async () => {
    const wc = peerWriteCode ?? 0x00;
    setPushState({ kind: "writing", writeCode: wc });

    try {
      await postPush(displayId);
      // Optimistic: mark as released. BG-1 event will refine to ignored/degraded.
      setPushState({ kind: "released" });
    } catch (err: unknown) {
      setPushState({ kind: "failed", error: classifyError(err) });
    }
  }, [displayId, peerWriteCode]);

  // ── Auto-reset terminal states after a few seconds ──────────────────────
  useEffect(() => {
    if (pullState.kind !== "verified" && pullState.kind !== "unverified") return;
    const t = setTimeout(() => setPullState({ kind: "idle" }), 3000);
    return () => clearTimeout(t);
  }, [pullState.kind]);

  useEffect(() => {
    if (pushState.kind !== "released" && pushState.kind !== "ignored" &&
        pushState.kind !== "degraded") return;
    const t = setTimeout(() => setPushState({ kind: "idle" }), 5000);
    return () => clearTimeout(t);
  }, [pushState.kind]);

  // ── Render helpers ──────────────────────────────────────────────────────

  function renderPull() {
    if (!switchCapable) return null;

    const disabled = pullState.kind === "writing";

    return (
      <>
        <button
          type="button"
          className="switch-btn switch-btn--pull"
          disabled={disabled}
          onClick={handlePull}
        >
          {renderPullLabel()}
        </button>
        {renderPullHint()}
        {renderPullState()}
      </>
    );
  }

  function renderPullHint() {
    if (pullState.kind !== "idle") return null;
    return (
      <span className="switch-state__hint">
        {owned ? "already ours" : "peer holds the panel"}
      </span>
    );
  }

  function renderPullLabel(): string {
    switch (pullState.kind) {
      case "idle": return "◀ Pull here";
      case "writing": return `◌ writing ${hexPad(pullState.writeCode)}…`;
      case "verified": return "✓ ours";
      case "unverified": return "⚠ wrote, not confirmed";
      case "aborted": return "✕ hook aborted the pull";
      case "failed": return `✕ ${pullState.error}`;
    }
  }

  function renderPullState() {
    switch (pullState.kind) {
      case "aborted":
        return (
          <div className="switch-detail switch-detail--danger">
            <span>{pullState.reason}</span>
            {" · "}
            <a href="#/config/switching">Switching › Hooks</a>
          </div>
        );
      case "failed":
        return <div className="switch-detail switch-detail--danger">{pullState.error}</div>;
      case "unverified":
        return (
          <div className="switch-detail switch-detail--warning">
            wrote {hexPad(localWriteCode)}, panel reports {hexPad(observedInputCode ?? undefined)}
            {observedInputCode == null && " — the write may still have worked. Run doctor ddcci."}
          </div>
        );
      default:
        return null;
    }
  }

  function renderPush() {
    return (
      <>
        {pushCapable ? (
          <button
            type="button"
            className="switch-btn switch-btn--push"
            disabled={pushState.kind === "writing"}
            onClick={handlePush}
          >
            {renderPushLabel()}
          </button>
        ) : (
          <div className="switch-push__absent">
            no shared_peer_input_write_code ·{" "}
            <a href="#/config/displays">configure</a>
          </div>
        )}
        {renderPushHint()}
        {renderPushState()}
      </>
    );
  }

  function renderPushHint() {
    if (!pushCapable || pushState.kind !== "idle") return null;
    return (
      <span className="switch-state__hint">
        writes {hexPad(peerWriteCode)} to the peer
      </span>
    );
  }

  function renderPushLabel(): string {
    switch (pushState.kind) {
      case "idle": return "Send to peer ▶";
      case "writing": return `◌ writing ${hexPad(pushState.writeCode)}…`;
      case "released": return "✓ sent";
      case "ignored":
        return "⚠ peer did not take it";
      case "degraded": return "✓ sent · unverified";
      case "failed": return `✕ ${pushState.error}`;
    }
  }

  function renderPushState() {
    switch (pushState.kind) {
      case "ignored":
        return (
          <div className="switch-detail switch-detail--warning">
            the panel ACKed the write but stayed on this input — the peer's output is
            probably asleep. dormant does not wake peers.
          </div>
        );
      case "degraded":
        return (
          <div className="switch-detail switch-detail--warning">
            no peer read code configured — verification degraded to 'changed away
            from our code'
          </div>
        );
      case "failed":
        return <div className="switch-detail switch-detail--danger">{pushState.error}</div>;
      default:
        return null;
    }
  }

  return (
    <div className="switch-state">
      <div className="switch-state__row">
        {/* LEFT: Pull */}
        <div className="switch-state__left">
          {renderPull()}
        </div>
        {/* RIGHT: Push */}
        <div className="switch-state__right">
          {renderPush()}
        </div>
      </div>
    </div>
  );
}
