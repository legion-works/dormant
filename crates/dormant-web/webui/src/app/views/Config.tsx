/**
 * Config view — rendered config file + validation + inventory.
 *
 * Fetches /api/config for the full ConfigResponse and /api/state for
 * the pending_reload field.  Renders a two-column layout: left is the
 * syntax-highlighted TOML file viewer, right is the parsed inventory
 * summary and validation card.  A reload button triggers postReload.
 *
 * Data: /api/config + /api/state  |  Visual authority: design README §4 /
 * Dormant Dashboard.dc.html lines 272-302.
 */
import { useState, useEffect, useCallback, useRef } from "react";
import { getConfig, getState, postReload } from "../../api/client";
import type { ConfigResponse, ConfigValidation } from "../../api/types";
import { Card } from "../components";
import "./Config.css";

interface ConfigState {
  loading: boolean;
  error: string | null;
  config: ConfigResponse | null;
  pendingReload: string | null;
}

interface TomlLine {
  pre: string;
  key: string;
  eq: string;
  val: string;
  comment: string;
  /** CSS var() color token for the value span. */
  valColor: string;
}

/**
 * Crude TOML line classifier for syntax highlighting.
 *
 * Strategy matches the design README §4 spec:
 *   comments / section headers → --text-faint
 *   keys → --blue-400
 *   equals → --text-muted
 *   string values → --success
 *   numeric values → --accent-warm
 */
function parseTomlLines(raw: string): TomlLine[] {
  return raw.split("\n").map((line) => {
    // Blank line
    if (line.trim() === "") {
      return { pre: line, key: "", eq: "", val: "", valColor: "var(--text-faint)", comment: "" };
    }

    // Full-line comment or section header
    if (/^\s*(#|\[)/.test(line)) {
      return { pre: line, key: "", eq: "", val: "", valColor: "var(--text-faint)", comment: "" };
    }

    // key = value  (with optional trailing comment)
    const kvMatch = line.match(/^(\s*)([\w._-]+)(\s*=\s*)(.*)$/);
    if (kvMatch) {
      const [, pre, key, eq, rest] = kvMatch;

      // Split trailing comment from value
      const commentIdx = rest.indexOf("#");
      let valPart = rest;
      let commentPart = "";
      if (commentIdx >= 0) {
        valPart = rest.slice(0, commentIdx).trimEnd();
        commentPart = rest.slice(commentIdx);
      }

      // Strip surrounding quotes for classification
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

    // Everything else: render as a plain line
    return { pre: line, key: "", eq: "", val: "", valColor: "var(--text-body)", comment: "" };
  });
}

/** Build a short validation summary sentence. */
function validationSummary(v: ConfigValidation): { cls: string; text: string } {
  if (v.load_error) {
    return { cls: "danger", text: `Load error: ${v.load_error}` };
  }
  if (v.errors.length > 0) {
    const first = v.errors[0];
    return { cls: "danger", text: `Validation failed: ${first.what} — ${first.detail}` };
  }
  if (v.warnings.length > 0) {
    return { cls: "warning", text: `${v.warnings.length} warning${v.warnings.length > 1 ? "s" : ""} — config loaded with caveats` };
  }
  return { cls: "ok", text: "Configuration parsed with no unknown keys. All zone members resolve to defined sensors; all rule displays are defined. Reload is safe." };
}

export default function Config() {
  const [state, setState] = useState<ConfigState>({
    loading: true,
    error: null,
    config: null,
    pendingReload: null,
  });
  const [reloading, setReloading] = useState(false);
  const mountedRef = useRef(true);

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

  const handleReload = useCallback(async () => {
    setReloading(true);
    try {
      await postReload();
    } catch {
      // Reload may fail; re-fetch config to show current state.
    }
    void fetchData();
    setReloading(false);
  }, [fetchData]);

  if (state.loading) {
    return <div className="config-loading">Loading configuration…</div>;
  }

  if (state.error) {
    return <div className="config-error">Daemon unreachable: {state.error}</div>;
  }

  const cfg = state.config!;
  const tomlLines = parseTomlLines(cfg.raw_toml);
  const vSum = validationSummary(cfg.validation);
  const inv = cfg.inventory;
  const inventoryRows = [
    { k: "Sensors", v: `${Object.keys(inv.sensors ?? {}).length}`, n: Object.keys(inv.sensors ?? {}).join(" · ") || "—" },
    { k: "Zones", v: `${Object.keys(inv.zones ?? {}).length}`, n: Object.keys(inv.zones ?? {}).join(" · ") || "—" },
    { k: "Displays", v: `${Object.keys(inv.displays ?? {}).length}`, n: Object.keys(inv.displays ?? {}).join(" · ") || "—" },
    { k: "Rules", v: `${Object.keys(inv.rules ?? {}).length}`, n: Object.keys(inv.rules ?? {}).join(" · ") || "—" },
  ];

  const sourceMismatch = cfg.source !== "last_applied";

  return (
    <div className="config">
      {/* Pending reload banner */}
      {(state.pendingReload || sourceMismatch) && (
        <div className="config-banner">
          {state.pendingReload
            ? `Config reload pending — ${state.pendingReload}`
            : `Config source: ${cfg.source} (not yet applied)`}
        </div>
      )}

      {/* Two-column grid */}
      <div className="config-grid">
        {/* Left: file viewer */}
        <div className="config-file">
          <div className="config-file__header">
            <span className="config-file__icon">{"📄"}</span>
            <span className="config-file__path">{cfg.path}</span>
            <span className={`config-file__status config-file__status--${vSum.cls}`}>
              {vSum.cls === "ok" ? "✓ valid" : vSum.cls === "warning" ? "⚠ warned" : "✕ error"}
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

        {/* Right column */}
        <div className="config-right">
          {/* Inventory card */}
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

          {/* Validation card */}
          <div className={`config-validation config-validation--${vSum.cls}`}>
            <span className="config-validation__icon">
              {vSum.cls === "ok" ? "✓" : vSum.cls === "warning" ? "⚠" : "✕"}
            </span>
            <span className="config-validation__text">{vSum.text}</span>
          </div>

          {/* Reload button */}
          <button
            className="config-reload-btn"
            onClick={handleReload}
            disabled={reloading}
          >
            {reloading ? "Reloading…" : "↻ Reload config"}
          </button>
        </div>
      </div>
    </div>
  );
}
