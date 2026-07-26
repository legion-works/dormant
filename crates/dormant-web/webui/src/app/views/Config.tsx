/**
 * Config view — rendered config file + validation + inventory (Raw TOML tab)
 * + editable settings form (five content tabs, one PatchStore).
 *
 * Fetches /api/config + /api/state in parallel on mount. Six sub-tabs
 * (daemon · presence · displays · switching · protection · raw), routed
 * at #/config/{tab}.  One SettingsForm instance owns one PatchStore
 * across all form tabs; the Raw tab is a separate read-only surface.
 */
import { useState, useEffect, useCallback, useRef } from "react";
import { getConfig, getState, postReload } from "../../api/client";
import type { ConfigResponse, KvmStatus } from "../../api/types";
import { Card, stageKindLabel } from "../components";
import { SettingsForm } from "../config/SettingsForm";

/** Global nav guard — Shell reads this before navigating away from Config. */
export const navGuard = { current: null as { dirtyCount: number; discard: () => void; dirtySections: Set<string> } | null };
import "./Config.css";
import "../config/ConfigForm.css";

interface ConfigState {
  loading: boolean;
  error: string | null;
  config: ConfigResponse | null;
  pendingReload: string | null;
  kvm?: KvmStatus | null;
}

interface TomlLine {
  pre: string;
  key: string;
  eq: string;
  val: string;
  comment: string;
  valColor: string;
}

type ConfigTab = "daemon" | "presence" | "displays" | "switching" | "protection" | "raw";

const CONFIG_TABS: ConfigTab[] = ["daemon", "presence", "displays", "switching", "protection", "raw"];

const TAB_LABELS: Record<ConfigTab, string> = {
  daemon: "Daemon",
  presence: "Presence",
  displays: "Displays",
  switching: "Switching",
  protection: "Protection",
  raw: "Raw",
};

/** Map patch prefix → Config tab for per-tab dirty dots. */
const SECTION_TO_TAB: Record<string, ConfigTab> = {
  daemon: "daemon",
  sensors: "presence",
  zones: "presence",
  rules: "presence",
  audio: "presence",
  displays: "displays",
  coordination: "switching",
  keymap: "switching",
  input_filter: "switching",
  wear: "protection",
  watchdog: "protection",
  notifications: "protection",
};

/** Parse #/config/{tab} → tab, defaulting to "daemon". */
function getConfigTabFromHash(): ConfigTab {
  const hash = window.location.hash.replace(/^#\/?/, "");
  const parts = hash.split("/");
  if (parts[0] === "config" && parts[1]) {
    const candidate = parts[1] as ConfigTab;
    if (CONFIG_TABS.includes(candidate)) return candidate;
  }
  return "daemon";
}

/**
 * Line-by-line TOML classifier for syntax highlighting.
 *
 * Mapping per design spec §4:
 *   comments / section headers → --text-faint
 *   keys → --blue-400
 *   equals → --text-muted
 *   string values → --success
 *   numeric values → --accent-warm
 */
function parseTomlLines(raw: string): TomlLine[] {
  return raw.split("\n").map((line) => {
    if (line.trim() === "") {
      return { pre: line, key: "", eq: "", val: "", valColor: "var(--text-faint)", comment: "" };
    }

    if (/^\s*(#|\[)/.test(line)) {
      return { pre: line, key: "", eq: "", val: "", valColor: "var(--text-faint)", comment: "" };
    }

    const kvMatch = line.match(/^(\s*)([\w._-]+)(\s*=\s*)(.*)$/);
    if (kvMatch) {
      const [, pre, key, eq, rest] = kvMatch;

      const commentIdx = rest.indexOf("#");
      let valPart = rest;
      let commentPart = "";
      if (commentIdx >= 0) {
        valPart = rest.slice(0, commentIdx).trimEnd();
        commentPart = rest.slice(commentIdx);
      }

      const valTrimmed = valPart.trim();
      const isString = /^".*"$/.test(valTrimmed) || /^'.*'$/.test(valTrimmed)
        || /^""".*"""$/.test(valTrimmed) || /'''.*'''/.test(valTrimmed);
      const isNumeric = /^-?\d[\d._eE+-]*$/.test(valTrimmed)
        || /^(true|false)$/.test(valTrimmed);
      const valColor = isString
        ? "var(--success)"
        : isNumeric
          ? "var(--accent-warm)"
          : "var(--text-body)";

      return { pre, key, eq, val: valPart, valColor, comment: commentPart };
    }

    return { pre: line, key: "", eq: "", val: "", valColor: "var(--text-body)", comment: "" };
  });
}

/** Overall validation severity class for the file header. */
function validationSeverity(v: ConfigResponse["validation"]): "ok" | "warning" | "danger" {
  if (v.load_error) return "danger";
  if (v.errors.length > 0) return "danger";
  if (v.warnings.length > 0) return "warning";
  return "ok";
}

/** Raw TOML tab content — identical to the original Config view. */
function RawTomlTab({ config: cfg, pendingReload, reloading, onReload }: {
  config: ConfigResponse;
  pendingReload: string | null;
  reloading: boolean;
  onReload: () => void;
}) {
  const tomlLines = parseTomlLines(cfg.raw_toml);
  const vSeverity = validationSeverity(cfg.validation);
  const inv = cfg.inventory;
  const inventoryRows = [
    { k: "Sensors", v: `${Object.keys(inv.sensors ?? {}).length}`, n: Object.keys(inv.sensors ?? {}).join(" · ") || "—" },
    { k: "Zones", v: `${Object.keys(inv.zones ?? {}).length}`, n: Object.keys(inv.zones ?? {}).join(" · ") || "—" },
    { k: "Displays", v: `${Object.keys(inv.displays ?? {}).length}`, n: Object.keys(inv.displays ?? {}).join(" · ") || "—" },
    { k: "Rules", v: `${Object.keys(inv.rules ?? {}).length}`, n: Object.keys(inv.rules ?? {}).join(" · ") || "—" },
  ];

  const sourceMismatch = cfg.source !== "last_applied";
  const hasValidationIssues =
    cfg.validation.load_error != null ||
    cfg.validation.errors.length > 0 ||
    cfg.validation.warnings.length > 0;

  return (
    <>
      {(pendingReload || sourceMismatch) && (
        <div className="config-banner">
          {pendingReload
            ? `Config reload pending — ${pendingReload}`
            : `Config source: ${cfg.source} (not yet applied)`}
        </div>
      )}

      <div className="config-grid">
        <div className="config-file">
          <div className="config-file__header">
            <span className="config-file__icon">{"📄"}</span>
            <span className="config-file__path">{cfg.path}</span>
            <span className={`config-file__status config-file__status--${vSeverity}`}>
              {vSeverity === "ok" ? "✓ valid" : vSeverity === "warning" ? "⚠ warned" : "✕ error"}
              {" · v"}{cfg.config_version}
            </span>
          </div>
          <div className="config-file__body">
            {tomlLines.map((l, i) => (
              <div key={i} className="config-file__line">
                {l.key || l.eq ? (
                  <>
                    <span className="t-pre">{l.pre}</span>
                    <span className="t-key">{l.key}</span>
                    <span className="t-eq">{l.eq}</span>
                    <span className="t-val" style={{ color: l.valColor }}>
                      {l.val}
                    </span>
                    {l.comment && <span className="t-comment">{l.comment}</span>}
                  </>
                ) : (
                  <span
                    className={
                      /^\s*(#|\[)/.test(l.pre) ? "t-comment" : "t-plain"
                    }
                  >
                    {l.pre}
                  </span>
                )}
              </div>
            ))}
          </div>
        </div>

        <div className="config-right">
          <Card>
            <div className="config-inventory">
              <div className="config-inventory__title">Parsed inventory</div>
              {inventoryRows.map((r) => (
                <div key={r.k} className="config-inventory__row">
                  <span className="config-inventory__key">{r.k}</span>
                  <span className="config-inventory__val">{r.v}</span>
                  <span className="config-inventory__names">{r.n}</span>
                </div>
              ))}
            </div>
          </Card>

          <Card>
            <div className="config-inventory">
              <div className="config-inventory__title">Backups</div>
              <div className="config-backups">
                <strong>Last 5 kept</strong>
                <code>backups/config.toml.&lt;timestamp&gt;.&lt;suffix&gt;</code>
                <span>Created before each successful atomic apply; live filenames are not exposed by the Web API.</span>
              </div>
            </div>
          </Card>

          {/* Ladder & Screensaver per-display summary */}
          {Object.values(inv.displays ?? {}).some(
            (dc) => dc.ladder?.length || dc.screensaver,
          ) && (
            <Card>
              <div className="config-inventory">
                <div className="config-inventory__title">Ladder &amp; Screensaver</div>
                {Object.entries(inv.displays ?? {})
                  .filter(([, dc]) => dc.ladder?.length || dc.screensaver)
                  .map(([id, dc]) => (
                    <div key={id} className="config-inventory__row">
                      <span className="config-inventory__key">{id}</span>
                      <span className="config-inventory__val">
                        {dc.ladder?.length
                          ? dc.ladder
                              .map(
                                (s) =>
                                  stageKindLabel(s.kind) +
                                  (s.dwell ? ` (${s.dwell})` : ""),
                              )
                              .join(" → ")
                          : "—"}
                      </span>
                      <span className="config-inventory__names">
                        {dc.screensaver
                          ? `${dc.screensaver.source.length} source${
                              dc.screensaver.source.length !== 1 ? "s" : ""
                            }`
                          : null}
                      </span>
                    </div>
                  ))}
              </div>
            </Card>
          )}

          {hasValidationIssues ? (
            <div className={`config-validation config-validation--${vSeverity}`}>
              <div className="config-validation__header">
                <span className="config-validation__icon">
                  {vSeverity === "danger" ? "✕" : "⚠"}
                </span>
                <span className="config-validation__title">
                  {vSeverity === "danger" ? "Validation errors" : "Validation warnings"}
                </span>
              </div>

              {cfg.validation.load_error && (
                <div className="config-validation__item config-validation__item--danger">
                  {cfg.validation.load_error}
                </div>
              )}

              {cfg.validation.errors.map((e, i) => (
                <div key={i} className="config-validation__item config-validation__item--danger">
                  <span className="config-validation__item-what">{e.what}</span>
                  {e.detail && (
                    <span className="config-validation__item-detail"> — {e.detail}</span>
                  )}
                </div>
              ))}

              {cfg.validation.warnings.map((w, i) => (
                <div key={i} className="config-validation__item config-validation__item--warning">
                  <span className="config-validation__item-path">{w.key_path}</span>
                  {w.message && (
                    <span className="config-validation__item-detail">: {w.message}</span>
                  )}
                </div>
              ))}
            </div>
          ) : (
            <div className={`config-validation config-validation--ok`}>
              <span className="config-validation__icon">✓</span>
              <span className="config-validation__text">
                Configuration parsed with no unknown keys. All zone members resolve to defined sensors; all rule displays are defined. Reload is safe.
              </span>
            </div>
          )}

          <button
            className="config-reload-btn"
            onClick={onReload}
            disabled={reloading}
          >
            {reloading ? "Reloading…" : "↻ Reload config"}
          </button>
        </div>
      </div>
    </>
  );
}

export default function Config() {
  const [state, setState] = useState<ConfigState>({
    loading: true,
    error: null,
    config: null,
    pendingReload: null,
  });
  const [reloading, setReloading] = useState(false);
  const [tab, setTab] = useState<ConfigTab>(getConfigTabFromHash);
  const [dirtyTabs, setDirtyTabs] = useState<Set<ConfigTab>>(new Set());
  const mountedRef = useRef(true);

  // Navigation guard state from SettingsForm — kept for the parent
  // (Shell) to consume in a later wave; Config internal tab switches
  // never prompt.
  const navGuardRef = useRef<{ dirtyCount: number; discard: () => void } | null>(null);

  const fetchData = useCallback(async () => {
    setState((prev) => ({ ...prev, error: null }));
    try {
      const [cfg, snap] = await Promise.all([getConfig(), getState()]);
      if (!mountedRef.current) return;
      setState({
        loading: false,
        error: null,
        config: cfg,
        pendingReload: snap.pending_reload,
        kvm: snap.kvm ?? null,
      });
    } catch (err: unknown) {
      if (!mountedRef.current) return;
      setState({
        loading: false,
        error: err instanceof Error ? err.message : "Unknown error",
        config: null,
        pendingReload: null,
      });
    }
  }, []);

  useEffect(() => {
    mountedRef.current = true;
    void fetchData();
    return () => { mountedRef.current = false; };
  }, [fetchData]);

  // Sync tab from hash on hashchange (sidebar nav, back/forward).
  useEffect(() => {
    const handler = () => {
      const nextTab = getConfigTabFromHash();
      setTab(nextTab);
    };
    window.addEventListener("hashchange", handler);
    return () => window.removeEventListener("hashchange", handler);
  }, []);

  const handleReload = useCallback(async () => {
    setReloading(true);
    try {
      await postReload();
    } catch {
      // Reload may fail; re-fetch to show current state.
    }
    await fetchData();
    setReloading(false);
  }, [fetchData]);

  /** Tab click — switches sub-tab unconditionally; never prompts. */
  const handleTabClick = useCallback(
    (targetTab: ConfigTab) => {
      setTab(targetTab);
      window.location.hash = `#/config/${targetTab}`;
    },
    [],
  );

  // Provide nav-guard callback to SettingsForm; Config does not prompt on
  // internal tab switches but surfaces the guard to the Shell via this ref
  // so a future Shell-level guard can use it (beforeunload is the immediate
  // guard that still fires).
  const handleNavGuard = useCallback(
    (guard: { dirtyCount: number; discard: () => void; dirtySections: Set<string> } | null) => {
      navGuardRef.current = guard;
      navGuard.current = guard;
      if (guard) {
        const tabs = new Set<ConfigTab>();
        for (const s of guard.dirtySections) {
          const t = SECTION_TO_TAB[s];
          if (t) tabs.add(t);
        }
        setDirtyTabs(tabs);
      } else {
        setDirtyTabs(new Set());
      }
    },
    [],
  );

  if (state.loading) {
    return <div className="config-loading">Loading configuration…</div>;
  }

  if (state.error) {
    return <div className="config-error">Daemon unreachable: {state.error}</div>;
  }

  const cfg = state.config!;

  return (
    <div className="config">
      <div className="config-tabs">
        {CONFIG_TABS.map((t) => (
          <button
            key={t}
            type="button"
            className={`config-tab${tab === t ? " config-tab--active" : ""}`}
            onClick={() => handleTabClick(t)}
          >
            {TAB_LABELS[t]}
            {dirtyTabs.has(t) && (
              <span style={{
                display: "inline-block",
                width: "6px",
                height: "6px",
                borderRadius: "50%",
                background: "var(--accent-warm)",
                marginLeft: "6px",
                verticalAlign: "middle",
              }} />
            )}
          </button>
        ))}
      </div>

      {/* Raw tab content — always mounted but hidden when not active. */}
      <div style={{ display: tab === "raw" ? undefined : "none" }}>
        <RawTomlTab
          config={cfg}
          pendingReload={state.pendingReload}
          reloading={reloading}
          onReload={handleReload}
        />
      </div>

      {/* SettingsForm — always mounted so the PatchStore survives tab switches. */}
      <div style={{ display: tab === "raw" ? "none" : undefined }}>
        <SettingsForm config={cfg} onNavigationGuard={handleNavGuard} tab={tab} kvm={state.kvm} />
      </div>
    </div>
  );
}
