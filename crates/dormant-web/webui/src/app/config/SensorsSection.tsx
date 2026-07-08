/**
 * Sensors section — one card per sensor from inventory.sensors.
 *
 * Renders scalar fields per sensor type.  The `type` discriminator
 * is rendered as a read-only label (not editable in v1).
 * broker_url / url fields are locked when the path is redacted.
 */
import FormSection from "./FormSection";
import { DurationField, EnumField, NumberField, TextField } from "./fields";
import type { FieldProps } from "./fields";
import type { PatchStore } from "./patch";
import type { SensorConfig } from "../../api/types";

interface SensorsSectionProps {
  sensors: Record<string, SensorConfig>;
  store: PatchStore;
  redactedPaths: string[][];
  onDirty: () => void;
  fieldErrors: Record<string, string | undefined>;
}

/** Fields rendered per sensor type — only keys that make sense to edit. */
const SENSOR_SCALAR_KEYS: string[] = [
  "topic", "broker_url", "url", "entity", "port", "baud",
  "payload_on", "payload_off", "field", "kind",
  "hold_time", "stale_timeout",
];

/** Per-field help and placeholder — accurate to the real config semantics. */
const HELP: Record<string, string> = {
  kind: "presence = continuous occupancy; motion = pulse, stretched by hold_time.",
  hold_time: "How long a motion pulse is treated as present.",
  stale_timeout: "A sensor silent this long becomes unavailable.",
};

const PLACEHOLDER: Record<string, string> = {
  broker_url: "mqtt://host:1883",
  url: "ws://ha.local:8123/api/websocket",
  port: "/dev/ttyUSB0",
  hold_time: "2s",
  stale_timeout: "300s",
};

export default function SensorsSection({ sensors, store, redactedPaths, onDirty, fieldErrors }: SensorsSectionProps) {
  const ids = Object.keys(sensors);
  if (ids.length === 0) return null;

  return (
    <FormSection title="Sensors">
      {ids.map((id) => {
        const cfg = sensors[id];
        const basePath = ["sensors", id];

        return (
          <div key={id} className="cf-card">
            <div className="cf-card__header">
              <span className="cf-card__name">{id}</span>
              <span className="cf-card__type">
                type: {cfg.type}
                <span className="cf-field__lock" title="not editable in v1" aria-label="not editable in v1">{"🔒"}</span>
              </span>
            </div>

            <div className="cf-card__fields">
              {SENSOR_SCALAR_KEYS.filter((k) => k in cfg).map((key) => {
                const path = [...basePath, key];
                const value = (cfg as unknown as Record<string, unknown>)[key];
                const redactedLocked = store.isLocked(path, redactedPaths);
                const locked = redactedLocked || key === "type";
                const lockedReason = redactedLocked
                  ? "contains credentials — edit in the config file"
                  : key === "type"
                    ? "not editable in v1"
                    : undefined;
                const error = fieldErrors[path.join(".")];

                const shared: FieldProps = {
                  path,
                  label: key,
                  value,
                  locked,
                  lockedReason,
                  error,
                  help: HELP[key],
                  placeholder: PLACEHOLDER[key],
                  onEdit: (p, v) => {
                    store.trackEdit(p, v);
                    onDirty();
                  },
                };

                // Widget selection by key
                if (key === "kind") {
                  return <EnumField key={key} {...shared} options={["presence", "motion"]} />;
                }
                if (key === "baud" || (typeof value === "number")) {
                  return <NumberField key={key} {...shared} />;
                }
                if (key === "hold_time" || key === "stale_timeout") {
                  return <DurationField key={key} {...shared} />;
                }
                return <TextField key={key} {...shared} />;
              })}

              {/* Show keys present in config but not in our known list as text-only */}
              {Object.keys(cfg)
                .filter((k) => k !== "type" && !SENSOR_SCALAR_KEYS.includes(k))
                .map((key) => {
                  const path = [...basePath, key];
                  const value = (cfg as unknown as Record<string, unknown>)[key];
                  return (
                    <TextField
                      key={key}
                      path={path}
                      label={key}
                      value={value}
                      locked={false}
                      onEdit={(p, v) => { store.trackEdit(p, v); onDirty(); }}
                    />
                  );
                })}
            </div>
          </div>
        );
      })}
    </FormSection>
  );
}
