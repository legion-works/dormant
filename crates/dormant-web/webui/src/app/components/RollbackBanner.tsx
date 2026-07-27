/**
 * RollbackBanner — global alert when the daemon is running on last-known-good
 * config. Shows the rollback reason, the two config fingerprints, and a
 * copyable recovery command (§BG-5).
 */
import { useLiveState } from "../hooks/useLiveState";

export interface RollbackBannerProps {
  onReviewConfig: () => void;
}

export default function RollbackBanner({ onReviewConfig }: RollbackBannerProps) {
  const { snapshot } = useLiveState();
  const rollback = snapshot?.rollback;

  if (!rollback) return null;

  return (
    <div className="global-banner global-banner--rollback" role="alert" data-testid="rollback-banner">
      <div className="global-banner__body">
        <strong className="global-banner__title">
          Running on rolled-back config (last-known-good)
        </strong>
        <span className="global-banner__detail">{rollback.detail}</span>
        <span className="rollback-banner__fingerprints">
          failed {rollback.failed_fp} → lkg {rollback.lkg_fp}
        </span>
        {rollback.recovery_command && (
          <span className="rollback-banner__recovery">
            <span className="rollback-banner__recovery-label">Recovery:</span>{" "}
            <code
              className="rollback-banner__recovery-cmd"
              title="Copy to restart the daemon after fixing the config"
              onClick={(e) => {
                const el = e.currentTarget;
                void navigator.clipboard.writeText(el.textContent ?? "");
                el.classList.add("rollback-banner__recovery-cmd--copied");
                setTimeout(() => el.classList.remove("rollback-banner__recovery-cmd--copied"), 1200);
              }}
            >
              {rollback.recovery_command}
            </code>
          </span>
        )}
      </div>
      <button type="button" className="global-banner__action" onClick={onReviewConfig}>
        Review config
      </button>
    </div>
  );
}
