/**
 * Daemon settings section — scalar fields from inventory.daemon.
 *
 * Renders each known key with the appropriate widget, falling back
 * to TextField for unknown scalar values.  Keys are rendered only
 * when present in the config (or have known defaults).
 */
import FormSection from "./FormSection";
import { DurationField, EnumField, NumberField, TextField, LOG_LEVELS, IDLE_TIME_UNITS, IDLE_SOURCES } from "./fields";
import type { FieldProps } from "./fields";
import type { PatchStore } from "./patch";

interface DaemonSectionProps {
  daemon: Record<string, unknown>;
  store: PatchStore;
  redactedPaths: string[][];
  onDirty: () => void;
  fieldErrors: Record<string, string | undefined>;
}

/** Known daemon keys with explicit widget choices. */
const KNOWN_FIELDS: Record<string, { kind: "enum" | "number" | "duration" | "text"; options?: readonly string[] }> = {
  log_level: { kind: "enum", options: LOG_LEVELS },
  web_port: { kind: "number" },
  startup_holdoff: { kind: "duration" },
  reload_debounce: { kind: "duration" },
  idle_time_unit: { kind: "enum", options: IDLE_TIME_UNITS },
  idle_source: { kind: "enum", options: IDLE_SOURCES },
  stale_sensor_timeout: { kind: "duration" },
};

/** Per-field help text — accurate to the real config semantics. */
const FIELD_HELP: Record<string, string> = {
  log_level: "Verbosity: trace < debug < info < warn < error.",
  web_port: "1024–65535; empty disables the web UI.",
  startup_holdoff: "Delay before any blank/wake actions after startup, allowing sensors to stabilise.",
  reload_debounce: "Coalesce rapid config reloads.",
  idle_time_unit: "How to read the compositor's idle-time reply. auto detects the unit; override only if detection is wrong.",
  idle_source: "Idle-detection backend for the user-activity inhibitor.",
  stale_sensor_timeout: "A sensor silent this long becomes unavailable.",
};

/** Placeholder text for empty inputs — the real default value. */
const FIELD_PLACEHOLDER: Record<string, string> = {
  startup_holdoff: "30s",
  reload_debounce: "500ms",
  stale_sensor_timeout: "300s",
};

export default function DaemonSection({ daemon, store, redactedPaths, onDirty, fieldErrors }: DaemonSectionProps) {
  const keys = Object.keys(daemon);
  if (keys.length === 0) return null;

  return (
    <FormSection title="Daemon">
      <div className="cf-card">
        {keys.map((key) => {
          const path = ["daemon", key];
          const value = daemon[key];
          const locked = store.isLocked(path, redactedPaths);
          const lockedReason = locked ? "contains credentials — edit in the config file" : undefined;
          const known = KNOWN_FIELDS[key];
          const error = fieldErrors[path.join(".")];

          const shared: FieldProps = {
            path,
            label: key,
            value,
            locked,
            lockedReason,
            error,
            help: FIELD_HELP[key],
            placeholder: FIELD_PLACEHOLDER[key],
            onEdit: (p, v) => {
              store.trackEdit(p, v);
              onDirty();
            },
          };

          if (locked) {
            // Render as text — the value is needed for context even when locked.
            return <TextField key={key} {...shared} />;
          }

          if (!known) {
            // Fall back to type inference
            if (typeof value === "number") return <NumberField key={key} {...shared} />;
            if (typeof value === "boolean") return <EnumField key={key} {...shared} options={["true", "false"]} />;
            if (typeof value === "string") return <TextField key={key} {...shared} />;
            return null;
          }

          switch (known.kind) {
            case "enum":
              return <EnumField key={key} {...shared} options={known.options ?? []} />;
            case "number":
              return <NumberField key={key} {...shared} />;
            case "duration":
              return <DurationField key={key} {...shared} />;
            case "text":
              return <TextField key={key} {...shared} />;
            default:
              return null;
          }
        })}
      </div>
    </FormSection>
  );
}
