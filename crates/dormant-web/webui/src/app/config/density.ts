/**
 * Density helpers for W1-5 — localStorage-backed expand/collapse state
 * per entity card, changelog markers, and summary labels.
 *
 * Used by the four collection sections (sensors, zones, rules, displays).
 */

import type { SensorConfig, ZoneConfig, DisplayConfig, RuleConfig } from "../../api/types";

/** localStorage key scoped to a section + entity. */
export function entityStorageKey(section: string, id: string): string {
  return `dormant-config-collapse:${section}:${id}`;
}

/** Read expanded state; defaults to true (expanded). */
export function readEntityExpanded(section: string, id: string): boolean {
  try {
    const v = localStorage.getItem(entityStorageKey(section, id));
    if (v === null) return true;
    return v === "1";
  } catch { return true; }
}

/** Persist expanded state. */
export function writeEntityExpanded(section: string, id: string, expanded: boolean) {
  try {
    localStorage.setItem(entityStorageKey(section, id), expanded ? "1" : "0");
  } catch { /* ignore */ }
}

/** One-line summary for collapsed sensor cards. */
export function sensorSummary(cfg: SensorConfig): string {
  const base = cfg.type;
  if (cfg.type === "usb-ld2410" && cfg.port) return `${base} · ${cfg.port}`;
  if (cfg.type === "mqtt" && cfg.topic) return `${base} · ${cfg.topic}`;
  if (cfg.type === "ha" && cfg.entity) return `${base} · ${cfg.entity}`;
  return base;
}

/** One-line summary for collapsed zone cards. */
export function zoneSummary(cfg: ZoneConfig): string {
  const parts: string[] = [cfg.mode];
  if (cfg.members && cfg.members.length > 0) {
    parts.push(cfg.members.join(", "));
  }
  return parts.join(" · ");
}

/** One-line summary for collapsed rule cards. */
export function ruleSummary(cfg: RuleConfig): string {
  const parts: string[] = [];
  if (cfg.zone) parts.push(`zone: ${cfg.zone}`);
  if (cfg.displays) parts.push(`${cfg.displays.length} display(s)`);
  return parts.join(" · ");
}

/** One-line summary for collapsed display cards. */
export function displaySummary(cfg: DisplayConfig): string {
  const parts: string[] = [];
  if (cfg.scope) parts.push(cfg.scope);
  if (cfg.controllers) parts.push(cfg.controllers.join(", "));
  if (cfg.blank_mode) parts.push(cfg.blank_mode);
  else if (cfg.ladder) parts.push("ladder");
  return parts.join(" · ");
}
