/**
 * Sensors section — one card per sensor from inventory.sensors.
 *
 * Renders scalar fields per sensor type.  The `type` discriminator
 * is rendered as a read-only label (not editable in v1).
 * broker_url / url fields are locked when the path is redacted.
 *
 * W1-5: per-entity collapse with localStorage persistence.
 */
import { useState, useCallback } from "react";
import FormSection from "./FormSection";
import { DurationField, EnumField, NumberField, TextField } from "./fields";
import type { FieldProps } from "./fields";
import type { PatchStore } from "./patch";
import type { SensorConfig, ZoneConfig } from "../../api/types";
import CreateEntityForm from "./CreateEntityForm";
import { referencingEntities } from "./entityCrud";
import { useConfirmDialog } from "../components";
import { readEntityExpanded, writeEntityExpanded, sensorSummary } from "./density";

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

/** Sensor summary: type + port/path — imported from density.ts. */

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
  const { confirm, dialog } = useConfirmDialog();

  // Per-entity expanded state
  const [expanded, setExpanded] = useState<Record<string, boolean>>(() => {
    const out: Record<string, boolean> = {};
    for (const id of ids) out[id] = readEntityExpanded("sensors", id);
    return out;
  });

  const toggleExpanded = useCallback((id: string) => {
    setExpanded((prev) => {
      const next = !prev[id];
      writeEntityExpanded("sensors", id, next);
      return { ...prev, [id]: next };
    });
  }, []);

  if (ids.length === 0 && !entityCrudEnabled) return null;

  async function handleDelete(id: string) {
    const refs = referencingEntities("sensors", id, { zones, rules: {} });
    const accepted = await confirm({
      title: `Delete sensor "${id}"?`,
      description: refs.length > 0
        ? `Referenced by ${refs.join(", ")}. Deleting it may make the pending config invalid.`
        : "Nothing else references sensors.",
      confirmLabel: "Delete sensor",
      tone: "danger",
    });
    if (!accepted) return;
    store.trackDelete("sensors", id);
    onDirty();
  }

  return (
    <>
    <FormSection id="sensors" title="Sensors">
      {ids.map((id) => {
        const cfg = sensors[id];
        const basePath = ["sensors", id];
        const open = expanded[id] !== false; // default true

        return (
          <div key={id} className="cf-card">
            <div className="cf-card__header">
              <button
                type="button"
                className="cf-section__toggle"
                onClick={() => toggleExpanded(id)}
                aria-expanded={open}
                style={{ minWidth: 0, gap: "4px" }}
              >
                <span className={`cf-section__chevron${open ? " cf-section__chevron--open" : ""}`}>
                  {"▶"}
                </span>
              </button>
              <span className="cf-card__name">{id}</span>
              {!open && (
                <span className="cf-card__summary-type">{sensorSummary(cfg)}</span>
              )}
              {open && (
                <span className="cf-card__type">
                  type: {cfg.type}
                  <span className="cf-field__lock" title="not editable in v1" aria-label="not editable in v1">{"🔒"}</span>
                </span>
              )}
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

            {open && (
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
                const pending = store.getEdit(path);
                const changed = pending !== undefined && pending !== value;

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
                let widget: React.ReactNode;
                if (key === "kind") {
                  widget = <EnumField key={key} {...shared} options={["presence", "motion"]} />;
                } else if (key === "baud" || (typeof value === "number")) {
                  widget = <NumberField key={key} {...shared} />;
                } else if (key === "hold_time" || key === "stale_timeout") {
                  widget = <DurationField key={key} {...shared} />;
                } else {
                  widget = <TextField key={key} {...shared} />;
                }

                if (changed) {
                  const was = String(value ?? "");
                  return (
                    <div key={key} className={`cf-field cf-field--changed cf-field--row`}>
                      {widget}
                      <span className="cf-field__was">changed · was {was}</span>
                    </div>
                  );
                }
                return <div key={key} className="cf-field cf-field--row">{widget}</div>;
              })}

              {/* Show keys present in config but not in our known list as text-only */}
              {Object.keys(cfg)
                .filter((k) => k !== "type" && !SENSOR_SCALAR_KEYS.includes(k))
                .map((key) => {
                  const path = [...basePath, key];
                  const value = (cfg as unknown as Record<string, unknown>)[key];
                  return (
                    <div key={key} className="cf-field cf-field--row">
                      <TextField
                        path={path}
                        label={key}
                        value={value}
                        locked={false}
                        onEdit={(p, v) => { store.trackEdit(p, v); onDirty(); }}
                      />
                    </div>
                  );
                })}
            </div>
            )}
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
    {dialog}
    </>
  );
}
