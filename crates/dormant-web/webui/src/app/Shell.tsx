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
  displays: { label: "Displays", icon: "▤", Component: Displays, badge: "0" },
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
  const { connected } = useLiveState();

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

  const ActiveComponent = VIEWS[activeView].Component;

  return (
    <div className="shell lw-aurora lw-aurora--drift" data-theme="default">
      <aside className="sidebar">
        <div className="sidebar-brand">
          <span className="brand-mark" aria-hidden="true">☽</span>
          <div>
            <div className="brand-wordmark">dormant</div>
            <div className="brand-sub">v0.1.0 · pre-alpha</div>
          </div>
        </div>

        <nav className="sidebar-nav">
          {VIEW_KEYS.map((key) => {
            const v = VIEWS[key];
            return (
              <button
                key={key}
                className={`nav-item${key === activeView ? " nav-item--active" : ""}`}
                onClick={() => navigate(key)}
              >
                <span className="nav-icon">{v.icon}</span>
                <span className="nav-label">{v.label}</span>
                {"badge" in v && v.badge != null && (
                  <span className={`nav-badge${v.badge === "live" ? " nav-badge--live" : ""}`}>
                    {v.badge}
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
