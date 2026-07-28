/**
 * Daemon settings section — scalar fields from inventory.daemon.
 *
 * Renders each known key with the appropriate widget, falling back
 * to TextField for unknown scalar values.
 *
 * W1-5: 230px label column, changed-field markers, Advanced collapse
 * (macos_idle_*, generation_barrier_ack_timeout, doctor_wake_settle).
 */
import { useState, useCallback } from "react";
import FormSection from "./FormSection";
import { DurationField, EnumField, NumberField, TextField, LOG_LEVELS, IDLE_TIME_UNITS, IDLE_SOURCES } from "./fields";
import type { FieldProps } from "./fields";
import type { PatchStore } from "./patch";
import { readSectionAdvanced, writeSectionAdvanced } from "./density";

interface DaemonSectionProps {
  daemon: Record<string, unknown>;
  store: PatchStore;
  redactedPaths: string[][];
  onDirty: () => void;
  fieldErrors: Record<string, string | undefined>;
}

const KNOWN_FIELDS: Record<string, { kind: "enum" | "number" | "duration" | "text"; options?: readonly string[] }> = {
  log_level: { kind: "enum", options: LOG_LEVELS },
  web_port: { kind: "number" },
  startup_holdoff: { kind: "duration" },
  reload_debounce: { kind: "duration" },
  generation_barrier_ack_timeout: { kind: "duration" },
  idle_time_unit: { kind: "enum", options: IDLE_TIME_UNITS },
  idle_source: { kind: "enum", options: IDLE_SOURCES },
  stale_sensor_timeout: { kind: "duration" },
  doctor_wake_settle: { kind: "duration" },
  macos_idle_frozen_polls: { kind: "number" },
  macos_idle_sanity_cap: { kind: "duration" },
  macos_idle_startup_grace: { kind: "duration" },
};

const FIELD_HELP: Record<string, string> = {
  log_level: "Verbosity: trace < debug < info < warn < error.",
  web_port: "1024–65535; empty disables the web UI.",
  startup_holdoff: "Delay before any blank/wake actions after startup, allowing sensors to stabilise.",
  reload_debounce: "Coalesce rapid config reloads.",
  generation_barrier_ack_timeout: "Maximum wait for the old generation to drain during a reload.",
  idle_time_unit: "How to read the compositor's idle-time reply. auto detects the unit; override only if detection is wrong.",
  idle_source: "Idle-detection backend for the user-activity inhibitor.",
  stale_sensor_timeout: "A sensor silent this long becomes unavailable.",
  doctor_wake_settle: "Doctor exercise: settle time before retrying the post-wake panel readback (100ms–30s).",
  macos_idle_frozen_polls: "Consecutive unchanged macOS idle readings before treating the source as frozen (minimum 2).",
  macos_idle_sanity_cap: "Maximum plausible macOS idle reading; larger values are rejected as bogus.",
  macos_idle_startup_grace: "Startup window before macOS idle readings are trusted.",
};

const FIELD_PLACEHOLDER: Record<string, string> = {
  startup_holdoff: "30s",
  reload_debounce: "500ms",
  generation_barrier_ack_timeout: "2s",
  stale_sensor_timeout: "300s",
  doctor_wake_settle: "3s",
  macos_idle_frozen_polls: "3",
  macos_idle_sanity_cap: "24h",
  macos_idle_startup_grace: "30s",
};

/** Keys placed behind the ▸ Advanced toggle. */
const ADVANCED_KEYS = new Set([
  "generation_barrier_ack_timeout",
  "doctor_wake_settle",
  "macos_idle_frozen_polls",
  "macos_idle_sanity_cap",
  "macos_idle_startup_grace",
]);

export default function DaemonSection({ daemon, store, redactedPaths, onDirty, fieldErrors }: DaemonSectionProps) {
  const keys = Object.keys(daemon);
  const visibleKeys = keys.filter((k) => !ADVANCED_KEYS.has(k));
  const advancedKeys = keys.filter((k) => ADVANCED_KEYS.has(k));

  const [showAdvanced, setShowAdvanced] = useState(
    // Auto-expand if any advanced key has a non-default value, otherwise restore from localStorage
    () => advancedKeys.some((k) => {
      const v = daemon[k];
      return v !== undefined && v !== "" && v !== 0;
    }) || readSectionAdvanced("daemon")
  );

  const toggleAdvanced = useCallback(() => {
    setShowAdvanced((prev) => { writeSectionAdvanced("daemon", !prev); return !prev; });
  }, []);

  if (keys.length === 0) return null;

  function renderField(key: string) {
    const path = ["daemon", key];
    const value = daemon[key];
    const locked = store.isLocked(path, redactedPaths);
    const known = KNOWN_FIELDS[key];
    const error = fieldErrors[path.join(".")];
    const pending = store.getEdit(path);
    const changed = pending !== undefined && pending !== value;

    const shared: FieldProps = {
      path, label: key, value, locked,
      lockedReason: locked ? "contains credentials — edit in the config file" : undefined,
      error, help: FIELD_HELP[key], placeholder: FIELD_PLACEHOLDER[key],
      onEdit: (p, v) => { store.trackEdit(p, v); onDirty(); },
    };

    let widget: React.ReactNode;
    if (locked) widget = <TextField key={key} {...shared} />;
    else if (!known) {
      if (typeof value === "number") widget = <NumberField key={key} {...shared} />;
      else if (typeof value === "boolean") widget = <EnumField key={key} {...shared} options={["true", "false"]} />;
      else if (typeof value === "string") widget = <TextField key={key} {...shared} />;
      else return null;
    } else switch (known.kind) {
      case "enum": widget = <EnumField key={key} {...shared} options={known.options ?? []} />; break;
      case "number": widget = <NumberField key={key} {...shared} />; break;
      case "duration": widget = <DurationField key={key} {...shared} />; break;
      case "text": widget = <TextField key={key} {...shared} />; break;
      default: return null;
    }

    const cls = `cf-field cf-field--row${changed ? " cf-field--changed" : ""}`;
    return (
      <div key={key} className={cls}>
        {widget}
        {changed && <span className="cf-field__was">changed · was {String(value ?? "")}</span>}
      </div>
    );
  }

  // W5-1: posture card — derived from web_bind / web_allow_nonloopback.
  // Truth table: LAN posture requires BOTH a non-loopback bind AND the
  // explicit web_allow_nonloopback opt-in.  Loopback bind + flag-true is
  // still loopback-only (the flag doesn't override the actual listener).
  const webBind = typeof daemon.web_bind === "string" ? daemon.web_bind : "";
  const isLoopbackBind = webBind === "127.0.0.1" || webBind === "::1" || webBind.startsWith("127.");
  const isNonloopback = !isLoopbackBind && Boolean(daemon.web_allow_nonloopback);
  const postureCard = (
    <div
      className={`cf-posture-card${isNonloopback ? " cf-posture-card--lan" : " cf-posture-card--loopback"}`}
      style={{
        padding: "10px 14px",
        borderRadius: "var(--radius-md)",
        marginBottom: "14px",
        fontSize: "11.5px",
        lineHeight: 1.55,
        display: "flex",
        alignItems: "flex-start",
        gap: "10px",
      }}
    >
      <span
        style={{
          display: "inline-flex",
          alignItems: "center",
          padding: "1px 8px",
          borderRadius: "var(--radius-full)",
          fontFamily: "var(--font-mono)",
          fontSize: "10px",
          fontWeight: 600,
          flexShrink: 0,
          marginTop: "1px",
          color: isNonloopback ? "var(--accent-warm)" : "var(--accent)",
          border: `1px solid ${isNonloopback ? "color-mix(in oklab, var(--accent-warm) 35%, transparent)" : "color-mix(in oklab, var(--accent) 35%, transparent)"}`,
          background: isNonloopback
            ? "color-mix(in oklab, var(--accent-warm) 10%, transparent)"
            : "color-mix(in oklab, var(--accent) 10%, transparent)",
        }}
      >
        {isNonloopback ? "LAN" : "loopback"}
      </span>
      <span style={{ color: "var(--text-body)" }}>
        {isNonloopback
          ? `The web surface is bound to ${daemon.web_bind || "non-loopback"} and reachable from the network without authentication. Exposed write routes: config apply, display blank/wake/switch/push, pause/resume, reload, doctor, emergency wake, pairing, doctor exercise.`
          : "Loopback only — the web surface is not reachable from the LAN. All write routes require local access."}
      </span>
    </div>
  );

  return (
    <FormSection id="daemon" title="Daemon">
      <div className="cf-card">
        {postureCard}

        {/* Groups: Web surface, Timing, Platform, Feature flags */}
        <div className="cf-card__summary-type" style={{ marginBottom: "6px" }}>
          Web surface
        </div>
        {visibleKeys.filter((k) => ["web_port", "web_bind"].includes(k)).map(renderField)}

        <div className="cf-card__summary-type" style={{ marginTop: "10px", marginBottom: "6px" }}>
          Timing
        </div>
        {visibleKeys.filter((k) => ["startup_holdoff", "reload_debounce", "stale_sensor_timeout", "log_level"].includes(k)).map(renderField)}

        <div className="cf-card__summary-type" style={{ marginTop: "10px", marginBottom: "6px" }}>
          Platform
        </div>
        {visibleKeys.filter((k) => ["idle_time_unit", "idle_source"].includes(k)).map(renderField)}

        <div className="cf-card__summary-type" style={{ marginTop: "10px", marginBottom: "6px" }}>
          Feature flags
        </div>
        {visibleKeys.filter((k) => /_enabled$/.test(k) || k === "web_allow_nonloopback" || k === "hook_edit_enabled").map(renderField)}

        {/* Remaining visible keys not in any group */}
        {visibleKeys.filter((k) => !["web_port", "web_bind", "startup_holdoff", "reload_debounce", "stale_sensor_timeout", "log_level", "idle_time_unit", "idle_source"].includes(k) && !(/_enabled$/.test(k) || k === "web_allow_nonloopback" || k === "hook_edit_enabled")).map(renderField)}

        {/* ▸ Advanced */}
        {advancedKeys.length > 0 && (
          <>
            <button
              type="button"
              className="cf-section__toggle"
              style={{ marginTop: "10px" }}
              onClick={toggleAdvanced}
            >
              <span className={`cf-section__chevron${showAdvanced ? " cf-section__chevron--open" : ""}`}>
                {"▸"}
              </span>
              <span style={{ fontSize: "11px", color: "var(--text-muted)" }}>
                Advanced
              </span>
            </button>
            {showAdvanced && advancedKeys.map(renderField)}
          </>
        )}

        {/* Other — any key not in KNOWN_FIELDS */}
        {keys.filter((k) => !KNOWN_FIELDS[k]).length > 0 && (
          <>
            <div className="cf-card__summary-type" style={{ marginTop: "10px", marginBottom: "6px" }}>
              Other
            </div>
            {keys.filter((k) => !KNOWN_FIELDS[k]).map(renderField)}
          </>
        )}
      </div>
    </FormSection>
  );
}
