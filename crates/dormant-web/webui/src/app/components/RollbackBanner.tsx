/**
 * Rollback banner — global chrome (mounted once in `Shell`, not per
 * view). Surfaces the daemon running on a last-known-good rollback
 * config after a rejected reload (`StateSnapshot.rollback`, boot-time
 * metadata set by the Runner's rollback lifecycle — see
 * `crates/dormant-core/src/rules.rs` `RollbackStatus`).
 *
 * Always the FIRST global alert (before `FailureBanner`) — an operator
 * needs to know the config is stale before they see live failure noise.
 */
import { useLiveState } from "../hooks/useLiveState";
import "./GlobalBanners.css";

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
      </div>
      <button type="button" className="global-banner__action" onClick={onReviewConfig}>
        Review config
      </button>
    </div>
  );
}
