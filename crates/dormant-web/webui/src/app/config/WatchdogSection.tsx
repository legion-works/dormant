/**
 * Watchdog settings section — the `[watchdog]` TOML section.
 *
 * Renders each known `watchdog.*` key with the appropriate widget,
 * mirroring WearSection/NotificationsSection's known-field guidance
 * pattern. All three keys have defaults in the Rust schema
 * (`WatchdogConfig::default()`), so the section is only hidden when
 * `watchdog` is entirely absent from the inventory (older
 * fixture/payload shape).
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

/** Known watchdog keys with explicit widget choices. */
const KNOWN_FIELDS: Record<string, { kind: "bool" | "number" | "duration" | "text" }> = {
  lkg_enabled: { kind: "bool" },
  lkg_rollback_enabled: { kind: "bool" },
  stability_window: { kind: "duration" },
};

/** Per-field help text — accurate to the real config semantics. */
const FIELD_HELP: Record<string, string> = {
  lkg_enabled: "Track a last-known-good (LKG) config generation snapshot. On by default.",
  lkg_rollback_enabled:
    "Allow a detected crash loop to trigger an automatic COUNTED rollback to the last-known-good generation. " +
    "Disabling this only suppresses the counted-rollback path (a debugging escape hatch) — immediate rollback " +
    "and sticky substitution stay active regardless of this setting.",
  stability_window:
    "How long a boot must stay up before it counts as stable for LKG purposes. 30s floor.",
};

/** Placeholder text for empty inputs — the real default value. */
const FIELD_PLACEHOLDER: Record<string, string> = {
  stability_window: "300s",
};

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
            return <TextField key={key} {...shared} />;
          }

          if (!known) {
            if (typeof value === "number") return <NumberField key={key} {...shared} />;
            if (typeof value === "boolean") return <BoolField key={key} {...shared} />;
            if (typeof value === "string") return <TextField key={key} {...shared} />;
            return null;
          }

          switch (known.kind) {
            case "bool":
              return <BoolField key={key} {...shared} />;
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
