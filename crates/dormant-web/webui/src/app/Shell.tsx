import { useState, useEffect, useCallback } from "react";
import Dashboard from "./views/Dashboard";
import Displays from "./views/Displays";
import Events from "./views/Events";
import Config from "./views/Config";
import Doctor from "./views/Doctor";
import { LiveStateProvider } from "./state";
import { useLiveState } from "./hooks/useLiveState";
import { postReload } from "../api/client";
import "./Shell.css";

const VIEWS = {
  dashboard: { label: "Dashboard", icon: "▦", Component: Dashboard },
  displays: { label: "Displays", icon: "▤", Component: Displays },
  events: { label: "Events", icon: "≣", Component: Events, badge: "live" },
  config: { label: "Config", icon: "{ }", Component: Config },
  doctor: { label: "Doctor", icon: "✚", Component: Doctor },
} as const;

type ViewKey = keyof typeof VIEWS;

const VIEW_KEYS = Object.keys(VIEWS) as ViewKey[];

function getViewFromHash(): ViewKey {
  const hash = window.location.hash.replace(/^#\/?/, "");
  return hash in VIEWS ? (hash as ViewKey) : "dashboard";
}

function formatClock(): string {
  return new Date().toLocaleTimeString("en-US", { hour12: false });
}

/**
 * Shell wrapper — injects the live-state provider so the inner Shell
 * can read `connected` and all views can access the patched state.
 */
export default function Shell() {
  return (
    <LiveStateProvider>
      <ShellInner />
    </LiveStateProvider>
  );
}

function ShellInner() {
  const [activeView, setActiveView] = useState<ViewKey>(getViewFromHash);
  const [clock, setClock] = useState(formatClock);
  const { connected, snapshot } = useLiveState();

  useEffect(() => {
    const onHashChange = () => setActiveView(getViewFromHash());
    window.addEventListener("hashchange", onHashChange);
    return () => window.removeEventListener("hashchange", onHashChange);
  }, []);

  useEffect(() => {
    const id = setInterval(() => setClock(formatClock()), 1_000);
    return () => clearInterval(id);
  }, []);

  const navigate = useCallback((key: ViewKey) => {
    setActiveView(key);
    window.location.hash = `#/${key}`;
  }, []);

  const handleReload = useCallback(async () => {
    try {
      await postReload();
    } catch {
      // The WS event stream surfaces the reload outcome.
      console.log("Config reload failed or daemon unreachable.");
    }
  }, []);

  const displaysBadge = snapshot ? String(snapshot.displays.length) : undefined;

  const ActiveComponent = VIEWS[activeView].Component;

  return (
    <div className="shell lw-aurora lw-aurora--drift" data-theme="default">
      <aside className="sidebar">
        <div className="sidebar-brand">
          <span className="brand-mark" aria-hidden="true">☽</span>
          <div>
            <div className="brand-wordmark">dormant</div>
            <div className="brand-sub">{`v${__DORMANT_VERSION__}`}</div>
          </div>
        </div>

        <nav className="sidebar-nav">
          {VIEW_KEYS.map((key) => {
            const v = VIEWS[key];
            // Dynamic badge: Displays shows the live display count; Events keeps its "live" marker.
            const badge = key === "displays" ? displaysBadge : ("badge" in v && v.badge != null ? v.badge : undefined);
            return (
              <button
                key={key}
                className={`nav-item${key === activeView ? " nav-item--active" : ""}`}
                onClick={() => navigate(key)}
              >
                <span className="nav-icon">{v.icon}</span>
                <span className="nav-label">{v.label}</span>
                {badge != null && (
                  <span className={`nav-badge${badge === "live" ? " nav-badge--live" : ""}`}>
                    {badge}
                  </span>
                )}
              </button>
            );
          })}
        </nav>

        <div className="sidebar-footer">
          <span className={`conn-dot${connected ? " conn-dot--live" : ""}`} />
          <span className="conn-label">
            {connected ? "dormantd running" : "connecting…"}
          </span>
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
      </aside>

      <main className="main">
        <header className="topbar">
          <div className="topbar-left">
            <h1 className="topbar-title">{VIEWS[activeView].label}</h1>
            <span className="topbar-sub">dormant web dashboard</span>
          </div>
          <div className="topbar-right">
            <span className="topbar-pill topbar-clock">
              <span className="clock-dot" />
              {clock}
            </span>
            <button className="topbar-pill topbar-reload" onClick={handleReload}>
              <span aria-hidden="true">↻</span> Reload
            </button>
          </div>
        </header>

        <div className="content">
          <ActiveComponent />
        </div>
      </main>
    </div>
  );
}
