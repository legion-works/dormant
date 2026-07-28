/**
 * Wear (panel-exposure) settings section — the `[wear]` TOML section.
 *
 * Renders each known `wear.*` key with the appropriate widget.
 * W1-5: 230px label column + changed-field markers.
 */
import FormSection from "./FormSection";
import { BoolField, DurationField, NumberField, TextField } from "./fields";
import type { FieldProps } from "./fields";
import type { PatchStore } from "./patch";

interface WearSectionProps {
  wear: Record<string, unknown> | undefined;
  store: PatchStore;
  redactedPaths: string[][];
  onDirty: () => void;
  fieldErrors: Record<string, string | undefined>;
}

const KNOWN_FIELDS: Record<string, { kind: "bool" | "number" | "duration" | "text" }> = {
  enabled: { kind: "bool" },
  sample_interval: { kind: "duration" },
  persist_interval: { kind: "duration" },
  read_timeout: { kind: "duration" },
  grid_rows: { kind: "number" },
  grid_cols: { kind: "number" },
  fallback_brightness: { kind: "number" },
  screensaver_factor: { kind: "number" },
  short_cycle_dwell: { kind: "duration" },
  advisory_after: { kind: "duration" },
};

const FIELD_HELP: Record<string, string> = {
  enabled: "Enable panel-wear tracking. On by default.",
  sample_interval: "How often to sample panel state for wear attribution.",
  persist_interval: "How often to persist the wear ledger to disk.",
  read_timeout: "Timeout for a single panel-state read during sampling.",
  grid_rows: "Number of rows in the wear-attribution grid.",
  grid_cols: "Number of columns in the wear-attribution grid.",
  fallback_brightness: "Brightness fraction (0.0-1.0) assumed when the real brightness can't be read.",
  screensaver_factor: "Brightness fraction (0.0-1.0) attributed while the screensaver is active.",
  short_cycle_dwell: "Minimum dwell before a blank/wake cycle counts as a full cycle.",
  advisory_after: "Panel age (accumulated on-hours) after which wear advisories start surfacing.",
};

const FIELD_PLACEHOLDER: Record<string, string> = {
  sample_interval: "60s",
  persist_interval: "300s",
  read_timeout: "2s",
  short_cycle_dwell: "600s",
  advisory_after: "96h",
};

export default function WearSection({ wear, store, redactedPaths, onDirty, fieldErrors }: WearSectionProps) {
  const inv = wear ?? {};
  const keys = Object.keys(inv);
  if (keys.length === 0) return null;

  return (
    <FormSection id="wear" title="Wear">
      <div className="cf-card">
        {keys.map((key) => {
          const path = ["wear", key];
          const value = inv[key];
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
            else if (typeof value === "boolean") widget = <BoolField key={key} {...shared} />;
            else if (typeof value === "string") widget = <TextField key={key} {...shared} />;
            else return null;
          } else switch (known.kind) {
            case "bool": widget = <BoolField key={key} {...shared} />; break;
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
        })}
      </div>
    </FormSection>
  );
}
