import { useState, useEffect, useCallback } from "react";
import Dashboard from "./views/Dashboard";
import Displays from "./views/Displays";
import Events from "./views/Events";
import Config from "./views/Config";
import Doctor from "./views/Doctor";
import { LiveStateProvider } from "./state";
import { useLiveState } from "./hooks/useLiveState";
import { postReload } from "../api/client";
import { navItems, navBadgeText, type ViewId } from "./nav";
import RollbackBanner from "./components/RollbackBanner";
import FailureBanner from "./components/FailureBanner";
import EmergencyWakeControl from "./components/EmergencyWakeControl";
import SidebarFooter from "./components/SidebarFooter";
import "./Shell.css";

const VIEW_COMPONENTS: Record<ViewId, React.ComponentType> = {
  dashboard: Dashboard,
  displays: Displays,
  events: Events,
  config: Config,
  doctor: Doctor,
};

const VIEW_LABELS: Record<ViewId, string> = {
  dashboard: "Dashboard",
  displays: "Displays",
  events: "Events",
  config: "Config",
  doctor: "Doctor",
};

/** Topbar subtitle per view — a one-line reminder of what the view covers. */
const VIEW_SUBTITLES: Record<ViewId, string> = {
  dashboard: "live presence-to-display state",
  displays: "per-display control & controller chains",
  events: "daemon event stream",
  config: "settings form, entity CRUD & validation",
  doctor: "environment & integration diagnostics",
};

const VIEW_IDS = Object.keys(VIEW_COMPONENTS) as ViewId[];

function getViewFromHash(): ViewId {
  const hash = window.location.hash.replace(/^#\/?/, "");
  return (VIEW_IDS as string[]).includes(hash) ? (hash as ViewId) : "dashboard";
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
  const [activeView, setActiveView] = useState<ViewId>(getViewFromHash);
  const [clock, setClock] = useState(formatClock);
  const { connected, snapshot, config, pollWarning, doctorReport, selectDisplay, selectedDisplay } = useLiveState();

  useEffect(() => {
    const onHashChange = () => setActiveView(getViewFromHash());
    window.addEventListener("hashchange", onHashChange);
    return () => window.removeEventListener("hashchange", onHashChange);
  }, []);

  useEffect(() => {
    const id = setInterval(() => setClock(formatClock()), 1_000);
    return () => clearInterval(id);
  }, []);

  const navigate = useCallback((key: ViewId) => {
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

  const handleInspect = useCallback((display: string) => {
    selectDisplay(display);
    navigate("displays");
  }, [selectDisplay, navigate]);

  const handleReviewConfig = useCallback(() => {
    navigate("config");
  }, [navigate]);

  const rollbackActive = snapshot?.rollback != null;
  const doctorFailures = doctorReport?.checks.filter((c) => c.status === "fail").length ?? 0;

  const items = navItems({
    displayCount: snapshot ? snapshot.displays.length : 0,
    eventsLive: connected,
    rollbackActive,
    doctorFailures,
  });

  // Generic pending-reload banner only surfaces when there's no rollback
  // banner already explaining the same underlying event (a rollback
  // *is* the outcome of a rejected reload) — showing both would be
  // redundant noise for the same fact.
  const showPendingReloadBanner = Boolean(snapshot?.pending_reload) && !rollbackActive;

  const ActiveComponent = VIEW_COMPONENTS[activeView];

  // Per handoff §3: when the Displays view has drilled into a single
  // display's detail, the topbar swaps to that display's id + a
  // detail-specific subtitle instead of the generic Displays heading.
  const inDisplayDetail = activeView === "displays" && Boolean(selectedDisplay);
  const topbarTitle = inDisplayDetail ? (selectedDisplay as string) : VIEW_LABELS[activeView];
  const topbarSub = inDisplayDetail ? "panel wear & controls" : VIEW_SUBTITLES[activeView];

  return (
    <div className="shell lw-aurora lw-aurora--drift" data-theme="default">
      <aside className="sidebar">
        <div className="sidebar-brand">
          <img src="/mark.svg" alt="" aria-hidden="true" className="brand-mark" />
          <div>
            <div className="brand-wordmark">dormant</div>
            <div className="brand-sub">{`v${__DORMANT_VERSION__}`}</div>
          </div>
        </div>

        <nav className="sidebar-nav">
          {items.map((item) => (
            <a
              key={item.id}
              href={`#/${item.id}`}
              className={`nav-item${item.id === activeView ? " nav-item--active" : ""}`}
              onClick={(event) => {
                event.preventDefault();
                navigate(item.id);
              }}
            >
              <span className="nav-icon" aria-hidden="true">{item.icon}</span>
              <span className="nav-label">{VIEW_LABELS[item.id]}</span>
              {item.badge && (
                <>
                  {" "}
                  <span className={`nav-badge nav-badge--${item.badge.kind}`}>
                    {navBadgeText(item.badge)}
                  </span>
                </>
              )}
            </a>
          ))}
        </nav>

        <SidebarFooter connected={connected} />
      </aside>

      <main className="main">
        <header className="topbar">
          <div className="topbar-left">
            <div className="topbar-heading">
              <h1 className="topbar-title">{topbarTitle}</h1>
              <span className="topbar-sub">{topbarSub}</span>
            </div>
          </div>
          <div className="topbar-right">
            {config && (
              <span className="topbar-pill topbar-config-path" title={config.path}>
                {config.path}
              </span>
            )}
            <span className="topbar-pill topbar-clock">
              <span className="clock-dot" />
              {clock}
            </span>
            <EmergencyWakeControl />
            <button className="topbar-pill topbar-reload" onClick={handleReload}>
              <span aria-hidden="true">↻</span> Reload
            </button>
          </div>
        </header>

        <div className="content">
          <RollbackBanner onReviewConfig={handleReviewConfig} />
          <FailureBanner onInspect={handleInspect} />
          {pollWarning && (
            <div className="global-banner global-banner--poll" role="alert">
              Live refresh delayed; showing the last snapshot — {pollWarning}
            </div>
          )}
          {showPendingReloadBanner && (
            <div className="global-banner global-banner--pending" role="status">
              Config reload pending — {snapshot?.pending_reload}
            </div>
          )}
          <ActiveComponent />
        </div>
      </main>
    </div>
  );
}
