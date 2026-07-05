/**
 * Global application shell — renders the fixed two-pane layout
 * (244px sidebar + 64px top bar) and routes the active view into
 * the content area via simple client-side hash routing.
 *
 * Design authority: design/web-ui/README.md §"Global chrome"
 * updated per the Legion Works reskin tokens.
 *
 * DS token usage:
 *   - Sidebar: `--bg-overlay`, `--border`
 *   - Top bar: height 64px, `--border` on bottom
 *   - Nav items: active → `--text-strong` + `--accent-muted` bg
 *   - dormant green = `--success`; Legion cyan = `--accent`
 *   - Fonts: `--font-display` (wordmark), `--font-ui` (nav/body), `--font-mono` (clock/version)
 *   - Pulsing connection dot uses --success + box-shadow (no infinite CSS loop).
 */
import { useState, useEffect, useCallback } from "react";
import Dashboard from "./views/Dashboard";
import Displays from "./views/Displays";
import Events from "./views/Events";
import Config from "./views/Config";
import Doctor from "./views/Doctor";
import "./Shell.css";

// ── Type-safe view registry ─────────────────────────────────────────────

const VIEWS = {
  dashboard: { label: "Dashboard", icon: "▦", Component: Dashboard },
  displays: { label: "Displays", icon: "▤", Component: Displays, badge: "0" },
  events: { label: "Events", icon: "≣", Component: Events, badge: "live" },
  config: { label: "Config", icon: "{ }", Component: Config },
  doctor: { label: "Doctor", icon: "✚", Component: Doctor },
} as const;

type ViewKey = keyof typeof VIEWS;

const VIEW_KEYS = Object.keys(VIEWS) as ViewKey[];

// ── Helpers ─────────────────────────────────────────────────────────────

function getViewFromHash(): ViewKey {
  const hash = window.location.hash.replace(/^#\/?/, "");
  return hash in VIEWS ? (hash as ViewKey) : "dashboard";
}

function formatClock(): string {
  return new Date().toLocaleTimeString("en-US", { hour12: false });
}

// ── Shell ───────────────────────────────────────────────────────────────

export default function Shell() {
  const [activeView, setActiveView] = useState<ViewKey>(getViewFromHash);
  const [clock, setClock] = useState(formatClock);
  const [connected, _setConnected] = useState(false); // TODO: wire to useEvents in Task 14

  // Hash-based routing — keep URL in sync with nav clicks
  useEffect(() => {
    const onHashChange = () => setActiveView(getViewFromHash());
    window.addEventListener("hashchange", onHashChange);
    return () => window.removeEventListener("hashchange", onHashChange);
  }, []);

  // Live clock (1s tick)
  useEffect(() => {
    const id = setInterval(() => setClock(formatClock()), 1_000);
    return () => clearInterval(id);
  }, []);

  const navigate = useCallback((key: ViewKey) => {
    setActiveView(key);
    window.location.hash = `#/${key}`;
  }, []);

  const handleReload = useCallback(() => {
    // Placeholder — wired to postReload() in a later task.
    console.log("Reload config requested (not yet wired)");
  }, []);

  const ActiveComponent = VIEWS[activeView].Component;

  return (
    <div className="shell" data-theme="default">
      {/* ── Sidebar ─────────────────────────────────────────────── */}
      <aside className="sidebar">
        {/* Brand block */}
        <div className="sidebar-brand">
          <span className="brand-mark" aria-hidden="true">☽</span>
          <div>
            <div className="brand-wordmark">dormant</div>
            <div className="brand-sub">v0.1.0 · pre-alpha</div>
          </div>
        </div>

        {/* Nav */}
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

        {/* Footer — connection status */}
        <div className="sidebar-footer">
          <span className={`conn-dot${connected ? " conn-dot--live" : ""}`} />
          <span className="conn-label">
            {connected ? "dormantd running" : "connecting…"}
          </span>
        </div>
      </aside>

      {/* ── Main column ────────────────────────────────────────── */}
      <main className="main">
        {/* Top bar */}
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

        {/* Content area */}
        <div className="content">
          <ActiveComponent />
        </div>
      </main>
    </div>
  );
}
