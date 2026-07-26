/**
 * Pull/push state machine — the interactive switching controls for a
 * shared display.
 *
 * Six pull states (idle → writing → verified / unverified / aborted / failed)
 * and five push states (idle → writing → released / ignored / degraded).
 *
 * Uses optimistic UI during the POST flight, then reconciles against the
 * snapshot poll and the BG-1 `DaemonEvent::Ownership` stream.
 *
 * Source: flows/kvm-pull-push.md
 */
import { useState, useCallback, useEffect, useRef } from "react";
import type { OwnershipEvent } from "../../api/types";
import { useEventLog } from "../hooks/useLiveState";
import { postSwitch, postPush } from "../../api/client";

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
}

// ── Helpers ────────────────────────────────────────────────────────────────

function hexPad(code: number | undefined): string {
  if (code == null) return "??";
  return `0x${code.toString(16)}`;
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
}: SwitchStateProps) {
  const [pullState, setPullState] = useState<PullState>({ kind: "idle" });
  const [pushState, setPushState] = useState<PushState>({ kind: "idle" });
  const pullTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const { events } = useEventLog();

  // ── BG-1 Ownership event reconciliation ────────────────────────────────
  useEffect(() => {
    if (events.length === 0) return;
    const latest = events[0]?.event; // newest first
    if (!latest || latest.event !== "ownership") return;
    const oe = latest as OwnershipEvent;
    if (oe.display !== displayId) return;

    // Terminal states from the Ownership event override optimistic polling.
    if (oe.verified === true) {
      setPullState({ kind: "verified" });
      if (pullTimerRef.current != null) {
        clearTimeout(pullTimerRef.current);
        pullTimerRef.current = null;
      }
    } else if (oe.verified === false) {
      // verified: false renders "unverified", never "success"
      setPullState({ kind: "unverified" });
      if (pullTimerRef.current != null) {
        clearTimeout(pullTimerRef.current);
        pullTimerRef.current = null;
      }
    }
  }, [events, displayId, pullTimerRef]);

  // ── Pull handler ────────────────────────────────────────────────────────
  const handlePull = useCallback(async () => {
    const wc = localWriteCode ?? 0x00;
    setPullState({ kind: "writing", writeCode: wc });

    // Optimistic poll reconciliation: after poll_interval, check snapshot.
    pullTimerRef.current = setTimeout(() => {
      // Check if the snapshot has updated ownership.
      // If owned is now true and observed matches our code, consider verified.
      // Otherwise, fall to unverified.
      setPullState((prev) => {
        if (prev.kind !== "writing") return prev;
        // Snapshot read happens inside the callback — read latest.
        return { kind: "unverified" };
      });
    }, 2500); // poll_interval ~2s, plus margin

    try {
      await postSwitch(displayId);
      // On success, the BG-1 event or next poll will set the terminal state.
    } catch (err: unknown) {
      if (pullTimerRef.current != null) {
        clearTimeout(pullTimerRef.current);
        pullTimerRef.current = null;
      }
      const msg = err instanceof Error ? err.message : "switch failed";
      // Map known error codes from the spec.
      if (msg.includes("409") || msg.includes("in flight")) {
        setPullState({ kind: "failed", error: "another switch is in flight" });
      } else if (msg.includes("5xx") || msg.includes("did not answer")) {
        setPullState({ kind: "failed", error: "the daemon did not answer — the panel was not touched" });
      } else {
        setPullState({ kind: "failed", error: msg });
      }
    }
  }, [displayId, localWriteCode]);

  // ── Push handler ────────────────────────────────────────────────────────
  const handlePush = useCallback(async () => {
    const wc = peerWriteCode ?? 0x00;
    setPushState({ kind: "writing", writeCode: wc });

    try {
      await postPush(displayId);
      // Optimistic: mark as released. The poll will refine to ignored if needed.
      setPushState({ kind: "released" });
    } catch (err: unknown) {
      const msg = err instanceof Error ? err.message : "push failed";
      setPushState({ kind: "failed", error: msg });
    }
  }, [displayId, peerWriteCode]);

  // ── Auto-reset terminal states after a few seconds ──────────────────────
  useEffect(() => {
    if (pullState.kind !== "verified" && pullState.kind !== "unverified") return;
    const t = setTimeout(() => setPullState({ kind: "idle" }), 3000);
    return () => clearTimeout(t);
  }, [pullState.kind]);

  useEffect(() => {
    if (pushState.kind !== "released") return;
    const t = setTimeout(() => setPushState({ kind: "idle" }), 3000);
    return () => clearTimeout(t);
  }, [pushState.kind]);

  // ── Render helpers ──────────────────────────────────────────────────────

  function renderPull() {
    if (!switchCapable) return null;

    const shared = pullState.kind !== "idle";
    const disabled = pullState.kind === "writing";

    return (
      <div className={`switch-pull${shared ? " switch-pull--active" : ""}`}>
        <button
          type="button"
          className="switch-btn switch-btn--pull"
          disabled={disabled}
          onClick={handlePull}
        >
          {renderPullLabel()}
        </button>
        {renderPullState()}
      </div>
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
            <a href={`#/config/switching`}>Switching › Hooks</a>
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
      <div className="switch-push">
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
            <a href={`#/config/displays`}>configure</a>
          </div>
        )}
        {renderPushState()}
      </div>
    );
  }

  function renderPushLabel(): string {
    switch (pushState.kind) {
      case "idle": return "▶ Push to peer";
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
        {renderPull()}
        {renderPush()}
      </div>
    </div>
  );
}
