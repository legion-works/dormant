/**
 * Apply bar — dirty-count badge, Apply / Discard buttons, and
 * outcome banner (reloaded / rejected / pending / superseded).
 *
 * The conflict dialog (409) is also rendered inline here because
 * it's closely coupled to the apply flow.
 */

export type ApplyOutcome =
  | { kind: "reloaded" }
  | { kind: "rejected"; detail?: string; fileWritten?: boolean }
  | { kind: "pending" | "superseded"; detail?: string };

interface ApplyBarProps {
  dirtyCount: number;
  applying: boolean;
  outcome: ApplyOutcome | null;
  conflict: boolean;
  onApply: () => void;
  onDiscard: () => void;
  onReload: () => void;
  onDismissConflict: () => void;
}

export default function ApplyBar({
  dirtyCount,
  applying,
  outcome,
  conflict,
  onApply,
  onDiscard,
  onReload,
  onDismissConflict,
}: ApplyBarProps) {
  return (
    <div className="cf-apply">
      {/* Conflict dialog — overrides the normal bar when active */}
      {conflict && (
        <div className="cf-apply__conflict" role="alertdialog">
          <p className="cf-apply__conflict-msg">
            Config changed on disk — your edits are against an outdated
            version. Reload the form to get the latest config, or keep
            editing (your changes will be lost).
          </p>
          <div className="cf-apply__conflict-actions">
            <button
              type="button"
              className="cf-apply__btn cf-apply__btn--danger"
              onClick={onReload}
            >
              Reload form
            </button>
            <button
              type="button"
              className="cf-apply__btn"
              onClick={onDismissConflict}
            >
              Keep editing
            </button>
          </div>
        </div>
      )}

      {/* Outcome banner */}
      {outcome && (
        <div className={`cf-apply__banner cf-apply__banner--${outcome.kind}`}>
          {outcome.kind === "reloaded" && (
            <span>✓ Config applied successfully — daemon reloaded.</span>
          )}
          {outcome.kind === "rejected" && (
            <span>
              ✕ Config rejected{outcome.detail ? `: ${outcome.detail}` : ""}.
              {outcome.fileWritten &&
                " The file on disk contains your change; the daemon is running the previous config. Fix the error and apply again."}
            </span>
          )}
          {(outcome.kind === "pending" || outcome.kind === "superseded") && (
            <span>
              Config {outcome.kind}
              {outcome.detail ? ` — ${outcome.detail}` : ""}. Refreshing…
            </span>
          )}
        </div>
      )}

      {/* Main bar */}
      <div className="cf-apply__bar">
        <span className="cf-apply__count">
          {dirtyCount} unsaved change{dirtyCount !== 1 ? "s" : ""}
        </span>
        <div className="cf-apply__actions">
          <button
            type="button"
            className="cf-apply__btn cf-apply__btn--discard"
            onClick={onDiscard}
            disabled={dirtyCount === 0}
          >
            Discard
          </button>
          <button
            type="button"
            className="cf-apply__btn cf-apply__btn--apply"
            onClick={onApply}
            disabled={dirtyCount === 0 || applying}
          >
            {applying ? "Applying…" : "Apply"}
          </button>
        </div>
      </div>
    </div>
  );
}
