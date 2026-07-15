/**
 * Navigation metadata — pure derivation of the five sidebar nav items
 * from live-state facts (display count, event-stream connectivity,
 * rollback status, doctor failures). `Shell` renders the `NavItem[]`
 * this produces; later tasks (Displays, Events, Config, Doctor) import
 * `NavItem`/`NavMeta` rather than re-deriving the sidebar's array shape.
 *
 * Also exports `useNavigate` — the minimal hash-based navigation hook
 * views use to switch tabs without depending on Shell-internal state.
 * Navigation is hash-based (matching Shell's hashchange listener); no
 * router dependency is required.
 */
import { useCallback } from "react";

export type ViewId = "dashboard" | "displays" | "events" | "config" | "doctor";

export interface NavBadge {
  kind: "rollback" | "live" | "count";
  value?: string;
}

export interface NavItem {
  id: ViewId;
  label: string;
  icon: string;
  badge?: NavBadge;
}

export interface NavMeta {
  displayCount: number;
  eventsLive: boolean;
  rollbackActive: boolean;
  doctorFailures: number;
}

/** Derive the five sidebar nav items (stable ids/order) from live facts. */
export function navItems(meta: NavMeta): NavItem[] {
  return [
    { id: "dashboard", label: "Dashboard", icon: "▦" },
    {
      id: "displays",
      label: "Displays",
      icon: "▣",
      badge: { kind: "count", value: String(meta.displayCount) },
    },
    {
      id: "events",
      label: "Events",
      icon: "⌁",
      badge: meta.eventsLive ? { kind: "live" } : undefined,
    },
    {
      id: "config",
      label: "Config",
      icon: "≡",
      badge: meta.rollbackActive ? { kind: "rollback" } : undefined,
    },
    {
      id: "doctor",
      label: "Doctor",
      icon: "◇",
      badge: meta.doctorFailures > 0 ? { kind: "count", value: String(meta.doctorFailures) } : undefined,
    },
  ];
}

/** Render text for a `NavBadge` — `count` uses its own value, `live`/`rollback` are fixed labels. */
export function navBadgeText(badge: NavBadge): string {
  if (badge.kind === "count") return badge.value ?? "";
  if (badge.kind === "live") return "live";
  return "rollback";
}

export function useNavigate() {
  return useCallback((key: ViewId) => {
    window.location.hash = `#/${key}`;
  }, []);
}
