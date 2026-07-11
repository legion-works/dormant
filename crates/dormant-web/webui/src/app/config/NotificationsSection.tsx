/**
 * Notifications settings section — the `[notifications]` TOML section.
 *
 * Renders each known `notifications.*` key with the appropriate widget,
 * mirroring DaemonSection/WearSection's known-field guidance pattern.
 * All four keys have defaults in the Rust schema
 * (`NotificationsConfig::default()`), so the section is only hidden when
 * `notifications` is entirely absent from the inventory (older
 * fixture/payload shape).
 */
import FormSection from "./FormSection";
import { BoolField, DurationField, NumberField, TextField } from "./fields";
import type { FieldProps } from "./fields";
import type { PatchStore } from "./patch";

interface NotificationsSectionProps {
  notifications: Record<string, unknown> | undefined;
  store: PatchStore;
  redactedPaths: string[][];
  onDirty: () => void;
  fieldErrors: Record<string, string | undefined>;
}

/** Known notifications keys with explicit widget choices. */
const KNOWN_FIELDS: Record<string, { kind: "bool" | "number" | "duration" | "text" }> = {
  enabled: { kind: "bool" },
  wake_attempt_threshold: { kind: "number" },
  cooldown: { kind: "duration" },
  notify_recovery: { kind: "bool" },
};

/** Per-field help text — accurate to the real config semantics. */
const FIELD_HELP: Record<string, string> = {
  enabled: "Enable wake-failure desktop notifications. On by default.",
  wake_attempt_threshold: "Consecutive wake-command failures before a notification fires.",
  cooldown: "Minimum time between successive notifications for the same display.",
  notify_recovery: "Emit a recovery notification once a previously-failing display wakes successfully again.",
};

/** Placeholder text for empty inputs — the real default value. */
const FIELD_PLACEHOLDER: Record<string, string> = {
  cooldown: "15m",
};

export default function NotificationsSection({ notifications, store, redactedPaths, onDirty, fieldErrors }: NotificationsSectionProps) {
  const inv = notifications ?? {};
  const keys = Object.keys(inv);
  if (keys.length === 0) return null;

  return (
    <FormSection title="Notifications">
      <div className="cf-card">
        {keys.map((key) => {
          const path = ["notifications", key];
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
