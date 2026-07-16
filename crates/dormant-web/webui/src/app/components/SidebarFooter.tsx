/**
 * Sidebar footer — connection status + the daemon identity block (P1-D):
 * a mono `pid <pid> · up <duration>` / socket line, a hairline divider,
 * then the "Legion fleet daemon" label row and the GitHub link.
 *
 * Fetches `GET /api/daemon` once on mount and again whenever the event
 * stream reconnects (`connected` flips false → true) — cheap, no polling
 * loop; pid/version/socket are static for the daemon's lifetime and
 * uptime only needs to resync after a connectivity gap.
 */
import { useEffect, useRef, useState } from "react";
import { getDaemon } from "../../api/client";
import type { DaemonIdentity } from "../../api/types";
import "./SidebarFooter.css";

export interface SidebarFooterProps {
  connected: boolean;
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

export default function SidebarFooter({ connected }: SidebarFooterProps) {
  const [daemon, setDaemon] = useState<DaemonIdentity | null>(null);
  const [now, setNow] = useState(() => Date.now());
  const wasConnected = useRef(connected);

  useEffect(() => {
    void getDaemon()
      .then(setDaemon)
      .catch(() => undefined);
  }, []);

  useEffect(() => {
    if (connected && !wasConnected.current) {
      void getDaemon()
        .then(setDaemon)
        .catch(() => undefined);
    }
    wasConnected.current = connected;
  }, [connected]);

  // Tick the displayed uptime once a minute — cheap, and daemon uptime
  // never needs sub-minute precision.
  useEffect(() => {
    const id = setInterval(() => setNow(Date.now()), 60_000);
    return () => clearInterval(id);
  }, []);

  const uptime = daemon
    ? formatUptime(now / 1000 - daemon.started_epoch_s)
    : null;

  return (
    <div className="sidebar-footer">
      <div className="sidebar-footer__conn-row">
        <span className={`conn-dot${connected ? " conn-dot--live" : ""}`} />
        <span className="conn-label">
          {connected ? "dormantd running" : "connecting…"}
        </span>
      </div>

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
