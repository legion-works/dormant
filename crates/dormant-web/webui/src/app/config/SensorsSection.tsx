/**
 * Sensors section — one card per sensor from inventory.sensors.
 *
 * Renders scalar fields per sensor type.  The `type` discriminator
 * is rendered as a read-only label (not editable in v1).
 * broker_url / url fields are locked when the path is redacted.
 */
import { useState } from "react";
import FormSection from "./FormSection";
import { DurationField, EnumField, NumberField, TextField } from "./fields";
import type { FieldProps } from "./fields";
import type { PatchStore } from "./patch";
import type { SensorConfig, ZoneConfig } from "../../api/types";
import CreateEntityForm from "./CreateEntityForm";
import { referencingEntities } from "./entityCrud";

interface SensorsSectionProps {
  sensors: Record<string, SensorConfig>;
  store: PatchStore;
  redactedPaths: string[][];
  onDirty: () => void;
  fieldErrors: Record<string, string | undefined>;
  /** Whether entity create/delete is enabled (`daemon.entity_crud_enabled`, spec §2/§10). Defaults to true when omitted (pre-feature callers). */
  entityCrudEnabled?: boolean;
  /** Live zones inventory — used to compute the delete-confirm references warning (spec §7). */
  zones?: Record<string, ZoneConfig>;
}

/** Fields rendered per sensor type — only keys that make sense to edit. */
const SENSOR_SCALAR_KEYS: string[] = [
  "topic", "broker_url", "url", "entity", "port", "baud",
  "payload_on", "payload_off", "field", "kind",
  "hold_time", "stale_timeout",
  "availability_topic", "availability_payload_online", "availability_payload_offline",
];

/** Per-field help and placeholder — accurate to the real config semantics. */
const HELP: Record<string, string> = {
  kind: "presence = continuous occupancy; motion = pulse, stretched by hold_time.",
  hold_time: "How long a motion pulse is treated as present.",
  stale_timeout: "A sensor silent this long becomes unavailable.",
  availability_topic: "Optional LWT/availability topic override — defaults to <topic>/availability if unset.",
  availability_payload_online: "Payload marking the sensor online. Informational only — no event is emitted.",
  availability_payload_offline: "Payload marking the sensor offline — emits Unavailable for this sensor.",
};

const PLACEHOLDER: Record<string, string> = {
  broker_url: "mqtt://host:1883",
  url: "ws://ha.local:8123/api/websocket",
  port: "/dev/ttyUSB0",
  hold_time: "2s",
  stale_timeout: "300s",
  availability_topic: "tele/desk/LWT",
  availability_payload_online: "online",
  availability_payload_offline: "offline",
};

export default function SensorsSection({
  sensors,
  store,
  redactedPaths,
  onDirty,
  fieldErrors,
  entityCrudEnabled = true,
  zones = {},
}: SensorsSectionProps) {
  const ids = Object.keys(sensors);
  const [showCreate, setShowCreate] = useState(false);

  if (ids.length === 0 && !entityCrudEnabled) return null;

  function handleDelete(id: string) {
    const refs = referencingEntities("sensors", id, { zones, rules: {} });
    const msg = refs.length > 0
      ? `Delete sensor "${id}"? It is referenced by ${refs.join(", ")} — deleting it may make those entities invalid.`
      : `Delete sensor "${id}"?`;
    if (window.confirm(msg)) {
      store.trackDelete("sensors", id);
      onDirty();
    }
  }

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
              {entityCrudEnabled && (
                <button
                  type="button"
                  className="cf-apply__btn cf-apply__btn--danger cf-card__delete"
                  onClick={() => handleDelete(id)}
                >
                  Delete
                </button>
              )}
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

      {entityCrudEnabled && (
        showCreate ? (
          <CreateEntityForm
            collection="sensors"
            existingIds={ids}
            onCreate={(id, value) => {
              store.trackCreate("sensors", id, value);
              onDirty();
              setShowCreate(false);
            }}
            onCancel={() => setShowCreate(false)}
          />
        ) : (
          <button type="button" className="cf-apply__btn cf-card__add" onClick={() => setShowCreate(true)}>
            + Add sensor
          </button>
        )
      )}
    </FormSection>
  );
}
