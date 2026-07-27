/**
 * Watchdog settings section — the `[watchdog]` TOML section.
 *
 * W1-5: 230px label column + changed-field markers.
 */
import FormSection from "./FormSection";
import { BoolField, DurationField, NumberField, TextField } from "./fields";
import type { FieldProps } from "./fields";
import type { PatchStore } from "./patch";

interface WatchdogSectionProps {
  watchdog: Record<string, unknown> | undefined;
  store: PatchStore;
  redactedPaths: string[][];
  onDirty: () => void;
  fieldErrors: Record<string, string | undefined>;
}

const KNOWN_FIELDS: Record<string, { kind: "bool" | "number" | "duration" | "text" }> = {
  lkg_enabled: { kind: "bool" },
  lkg_rollback_enabled: { kind: "bool" },
  stability_window: { kind: "duration" },
};

const FIELD_HELP: Record<string, string> = {
  lkg_enabled: "Track a last-known-good (LKG) config generation snapshot. On by default.",
  lkg_rollback_enabled:
    "Allow a detected crash loop to trigger an automatic COUNTED rollback to the LKG generation. " +
    "Disabling only suppresses the counted-rollback path (a debugging escape hatch) — " +
    "immediate rollback and sticky substitution stay active regardless.",
  stability_window: "How long a boot must stay up before it counts as stable for LKG purposes. 30s floor.",
};

const FIELD_PLACEHOLDER: Record<string, string> = { stability_window: "300s" };

export default function WatchdogSection({ watchdog, store, redactedPaths, onDirty, fieldErrors }: WatchdogSectionProps) {
  const inv = watchdog ?? {};
  const keys = Object.keys(inv);
  if (keys.length === 0) return null;

  return (
    <FormSection title="Watchdog">
      <div className="cf-card">
        {keys.map((key) => {
          const path = ["watchdog", key];
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
