/**
 * Notifications settings section — the `[notifications]` TOML section.
 *
 * W1-5: 230px label column + changed-field markers.
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

const KNOWN_FIELDS: Record<string, { kind: "bool" | "number" | "duration" | "text" }> = {
  enabled: { kind: "bool" },
  wake_attempt_threshold: { kind: "number" },
  cooldown: { kind: "duration" },
  notify_recovery: { kind: "bool" },
};

const FIELD_HELP: Record<string, string> = {
  enabled: "Enable wake-failure desktop notifications. On by default.",
  wake_attempt_threshold: "Consecutive wake-command failures before a notification fires.",
  cooldown: "Minimum time between successive notifications for the same display.",
  notify_recovery: "Emit a recovery notification once a previously-failing display wakes successfully again.",
};

const FIELD_PLACEHOLDER: Record<string, string> = { cooldown: "15m" };

export default function NotificationsSection({ notifications, store, redactedPaths, onDirty, fieldErrors }: NotificationsSectionProps) {
  const inv = notifications ?? {};
  const keys = Object.keys(inv);
  if (keys.length === 0) return null;

  return (
    <FormSection id="notifications" title="Notifications">
      <div className="cf-card">
        {keys.map((key) => {
          const path = ["notifications", key];
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
