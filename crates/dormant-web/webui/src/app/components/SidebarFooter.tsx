/**
 * Sidebar footer — connection status + the daemon identity block (P1-D):
 * a mono `pid <pid> · up <duration>` / socket line, a web-surface posture
 * chip (§W5-1), a hairline divider, then the "Legion fleet daemon" label
 * row and the GitHub link.
 *
 * Receives daemon identity from the shell so the brand and footer share
 * one `GET /api/daemon` request at startup and after reconnects.
 */
import { useCallback, useEffect, useRef, useState } from "react";
import type { DaemonIdentity } from "../../api/types";
import { postStarNudgeDismiss, postStarNudgeStar } from "../../api/client";
import "./SidebarFooter.css";

export interface SidebarFooterProps {
  connected: boolean;
  daemon: DaemonIdentity | null;
  /** rust: daemon.web_bind — the configured bind address. */
  webBind?: unknown;
  /** rust: daemon.web_allow_nonloopback — opt-in flag. */
  webAllowNonloopback?: unknown;
}

/** Format seconds elapsed as `6h 12m` style — hours are always shown
 * (even 0h) once the daemon has run past a minute, matching the proto's
 * `daemon.uptime` example (`6h 12m`); under a minute renders `<1m`. */
function formatUptime(elapsedSeconds: number): string {
  const totalMinutes = Math.floor(elapsedSeconds / 60);
  if (totalMinutes < 1) return "<1m";
  const hours = Math.floor(totalMinutes / 60);
  const minutes = totalMinutes % 60;
  return hours > 0 ? `${hours}h ${minutes}m` : `${minutes}m`;
}

export default function SidebarFooter({ connected, daemon, webBind, webAllowNonloopback }: SidebarFooterProps) {
  const [now, setNow] = useState(() => Date.now());

  // Local dismiss state for the star nudge — starts from the server flag,
  // then stays true in this session once the user dismisses (optimistic).
  const [starNudgeDismissed, setStarNudgeDismissed] = useState(
    () => daemon?.star_nudge_dismissed ?? false,
  );
  // Transient "Starred ✓" state shown briefly after a successful gh star.
  const [starNudgeStarred, setStarNudgeStarred] = useState(false);
  // CORR 2: timer id so we can clear it on unmount and prevent a second
  // timer from stacking on double-click.
  const starTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  // Keep local state in sync if the daemon identity reloads (e.g. after
  // a WebSocket reconnect and re-fetch).
  useEffect(() => {
    if (daemon?.star_nudge_dismissed) {
      setStarNudgeDismissed(true);
    }
  }, [daemon?.star_nudge_dismissed]);

  // CORR 2: clear the "Starred ✓" timer on unmount so it cannot fire
  // after the component is gone.
  useEffect(() => {
    return () => {
      if (starTimerRef.current !== null) {
        clearTimeout(starTimerRef.current);
      }
    };
  }, []);

  // Tick the displayed uptime once a minute — cheap, and daemon uptime
  // never needs sub-minute precision.
  useEffect(() => {
    const id = setInterval(() => setNow(Date.now()), 60_000);
    return () => clearInterval(id);
  }, []);

  const uptime = daemon
    ? formatUptime(now / 1000 - daemon.started_epoch_s)
    : null;

  // W5-1: truth table — LAN posture requires BOTH a non-loopback bind
  // AND the explicit web_allow_nonloopback opt-in.  A loopback bind with
  // the flag true is still loopback-only (the flag doesn't override the
  // actual listener).  Non-loopback without the flag is rejected by the
  // server security guard at startup, so that combination never appears
  // live; classify it as loopback-only for safety.
  const isLoopbackBind = typeof webBind === "string"
    && (webBind === "127.0.0.1" || webBind === "::1" || webBind.startsWith("127."));
  const isNonloopback = !isLoopbackBind && Boolean(webAllowNonloopback);

  const handleStarClick = useCallback(async () => {
    // CORR 2: prevent double-timer on fast double-click.
    if (starTimerRef.current !== null) {
      clearTimeout(starTimerRef.current);
      starTimerRef.current = null;
    }
    try {
      const resp = await postStarNudgeStar();
      if (resp.starred) {
        // gh CLI succeeded — show brief "Starred ✓", then hide.
        setStarNudgeStarred(true);
        starTimerRef.current = setTimeout(() => {
          starTimerRef.current = null;
          setStarNudgeDismissed(true);
          setStarNudgeStarred(false);
        }, 2000);
        return;
      }
    } catch {
      // Network error — proceed to fallback below.
    }
    // gh CLI failed or unavailable — open the repo page as fallback.
    window.open("https://github.com/legion-works/dormant", "_blank", "noopener,noreferrer");
    setStarNudgeDismissed(true);
  }, []);

  const handleStarDismiss = useCallback(() => {
    setStarNudgeDismissed(true);
    postStarNudgeDismiss();
  }, []);

  const showStarNudge = daemon && !starNudgeDismissed;

  return (
    <div className="sidebar-footer">
      <div className="sidebar-footer__conn-row">
        <span className={`conn-dot${connected ? " conn-dot--live" : ""}`} />
        <span className="conn-label">
          {connected ? "dormantd running" : "connecting…"}
        </span>
      </div>

      {/* W5-1: web-surface posture chip */}
      {daemon && (
        <div className="sidebar-footer__posture">
          <span className={`posture-chip${isNonloopback ? " posture-chip--lan" : " posture-chip--loopback"}`}>
            {isNonloopback ? "LAN · unauthenticated" : "loopback only"}
          </span>
        </div>
      )}

      {daemon && (
        <div className="sidebar-footer__daemon">
          <span className="sidebar-footer__daemon-line">
            pid {daemon.pid} · up {uptime}
          </span>
          <span className="sidebar-footer__daemon-line sidebar-footer__daemon-socket" title={daemon.socket}>
            {daemon.socket}
          </span>
        </div>
      )}

      <div className="sidebar-footer__divider" />

      {/* Star-the-repo nudge — shown once, dismissed permanently server-side */}
      {showStarNudge && (
        <div className="sidebar-footer__star-nudge">
          {starNudgeStarred ? (
            <span className="star-nudge-starred">☆ Starred ✓</span>
          ) : (
            <>
              <span className="star-nudge-glyph" aria-hidden="true">☆</span>
              <button
                type="button"
                className="star-nudge-link"
                onClick={handleStarClick}
              >
                Star the repo
              </button>
              <button
                type="button"
                className="star-nudge-dismiss"
                onClick={handleStarDismiss}
                aria-label="Dismiss star nudge"
              >
                ×
              </button>
            </>
          )}
        </div>
      )}

      <div className="sidebar-footer__fleet-row">
        <img src="/legion-mark.svg" alt="" aria-hidden="true" className="footer-fleet-mark" />
        <span className="sidebar-footer__fleet-label">Legion fleet daemon</span>
        <a
          href="https://github.com/legion-works/dormant"
          target="_blank"
          rel="noopener noreferrer"
          aria-label="GitHub repository"
          className="footer-github"
        >
          <svg
            width="16"
            height="16"
            viewBox="0 0 16 16"
            fill="currentColor"
            aria-hidden="true"
          >
            <path fillRule="evenodd" d="M8 0C3.58 0 0 3.58 0 8c0 3.54 2.29 6.53 5.47 7.59.4.07.55-.17.55-.38 0-.19-.01-.82-.01-1.49-2.01.37-2.53-.49-2.69-.94-.09-.23-.48-.94-.82-1.13-.28-.15-.68-.52-.01-.53.63-.01 1.08.58 1.23.82.72 1.21 1.87.87 2.33.66.07-.52.28-.87.51-1.07-1.78-.2-3.64-.89-3.64-3.95 0-.87.31-1.59.82-2.15-.08-.2-.36-1.02.08-2.12 0 0 .67-.21 2.2.82.64-.18 1.32-.27 2-.27.68 0 1.36.09 2 .27 1.53-1.04 2.2-.82 2.2-.82.44 1.1.16 1.92.08 2.12.51.56.82 1.27.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 1.48 0 1.07-.01 1.93-.01 2.2 0 .21.15.46.55.38A8.013 8.013 0 0 0 16 8c0-4.42-3.58-8-8-8Z" />
          </svg>
          <span>GitHub</span>
        </a>
      </div>
    </div>
  );
}
