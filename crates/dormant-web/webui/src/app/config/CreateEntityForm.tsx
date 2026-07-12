/**
 * Shared entity-create form for the four CRUD collections (spec §4/§5/§7,
 * config-crud-wizard T6). Field set per collection mirrors
 * `CREATABLE_FIELDS` (entityCrud.ts, itself a hand-verified mirror of
 * `crates/dormant-web/src/config_patch.rs:488-545`) — sensors get a
 * `type` discriminator with per-type conditional fields (mqtt/ha/
 * usb-ld2410), zones/displays/rules have no discriminator.
 *
 * The id field gets LIVE hygiene feedback via `validateEntityId`
 * (entityCrud.ts) — a client-side mirror of the server's
 * `validate_entity_id`. The server is still the real boundary: this
 * only prevents submitting an id the server would reject outright.
 */
import { useState } from "react";
import { DurationField, EnumField, NumberField, BoolField, TextField, MultiSelectField } from "./fields";
import {
  validateEntityId,
  VALID_INHIBITORS,
  DISPLAY_CONTROLLER_OPTIONS,
} from "./entityCrud";
import type { CrudCollection } from "./entityCrud";

const SENSOR_TYPES = ["mqtt", "ha", "usb-ld2410"] as const;
type SensorType = (typeof SENSOR_TYPES)[number];

const ZONE_MODES = ["any", "all", "quorum", "weighted"] as const;
type ZoneMode = (typeof ZONE_MODES)[number];

const BLANK_MODE_OPTIONS = ["power_off", "screen_off_audio_on", "brightness_zero"] as const;

interface CreateEntityFormProps {
  collection: CrudCollection;
  /** Ids already present in this collection — a client-side collision pre-check. */
  existingIds: string[];
  /** Zone ids from the live inventory — populates rules' `zone` select. */
  zoneIds?: string[];
  /** Display ids from the live inventory — populates rules' `displays` multi-select. */
  displayIds?: string[];
  /** Sensor ids from the live inventory — populates zones' `members` multi-select. */
  sensorIds?: string[];
  /**
   * Seed values for the non-id fields (e.g. the pairing wizard's
   * post-pair "create display?" hand-off, spec §8.3: `{host,
   * controllers: ["samsung-tizen"]}`). Only read once, at mount — this
   * component is always freshly mounted when a caller flips it into
   * view (a ternary swap, not a persistent instance), so there's no
   * stale-prop concern.
   */
  initialFields?: Record<string, unknown>;
  onCreate: (id: string, value: Record<string, unknown>) => void;
  onCancel: () => void;
}

/** A dummy path prefix for widgets rendered by this form — no real config
 * path exists yet (the entity isn't created), the `[fields.tsx]` widgets
 * only use `path` to derive an input `id`/`htmlFor` and hand it back to
 * `onEdit`, which this form ignores in favor of its own `setField`. */
const NEW_PATH_PREFIX = "__new__";

export default function CreateEntityForm({
  collection,
  existingIds,
  zoneIds = [],
  displayIds = [],
  sensorIds = [],
  initialFields,
  onCreate,
  onCancel,
}: CreateEntityFormProps) {
  const [id, setId] = useState("");
  const [sensorType, setSensorType] = useState<SensorType>("mqtt");
  const [zoneMode, setZoneMode] = useState<ZoneMode>("any");
  const [fields, setFields] = useState<Record<string, unknown>>(() => initialFields ?? {});

  const idHygiene = validateEntityId(id);
  const idTaken = id.length > 0 && existingIds.includes(id);
  const idError =
    id.length === 0
      ? undefined
      : !idHygiene.ok
        ? idHygiene.reason
        : idTaken
          ? `entity id '${id}' already exists`
          : undefined;
  const idValid = id.length > 0 && idHygiene.ok && !idTaken;

  function setField(key: string, value: unknown) {
    setFields((f) => ({ ...f, [key]: value }));
  }

  function buildValue(): Record<string, unknown> {
    const value: Record<string, unknown> = { ...fields };
    if (collection === "sensors") value.type = sensorType;
    if (collection === "zones") value.mode = zoneMode;
    for (const key of Object.keys(value)) {
      const v = value[key];
      if (v === "" || v === undefined) delete value[key];
      if (Array.isArray(v) && v.length === 0) delete value[key];
    }
    return value;
  }

  function submit() {
    if (!idValid) return;
    onCreate(id, buildValue());
  }

  const p = (key: string) => [NEW_PATH_PREFIX, key];

  return (
    <div className="cf-card cf-card--create" data-testid={`create-${collection}-form`}>
      <div className="cf-field">
        <label className="cf-field__label" htmlFor={`new-${collection}-id`}>id</label>
        <div className="cf-field__input-row">
          <input
            id={`new-${collection}-id`}
            className="cf-field__input"
            value={id}
            onChange={(e) => setId(e.target.value)}
            placeholder="lowercase-id"
          />
        </div>
        {idError && <span className="cf-field__error">{idError}</span>}
      </div>

      {collection === "sensors" && (
        <>
          <EnumField
            path={p("type")}
            label="type"
            value={sensorType}
            locked={false}
            onEdit={(_p, v) => setSensorType(v as SensorType)}
            options={SENSOR_TYPES}
          />
          {sensorType === "mqtt" && (
            <>
              <TextField path={p("broker_url")} label="broker_url" value={fields.broker_url ?? ""} locked={false} onEdit={(_p, v) => setField("broker_url", v)} placeholder="mqtt://host:1883" />
              <TextField path={p("topic")} label="topic" value={fields.topic ?? ""} locked={false} onEdit={(_p, v) => setField("topic", v)} />
              <TextField path={p("field")} label="field" value={fields.field ?? ""} locked={false} onEdit={(_p, v) => setField("field", v)} />
              <TextField path={p("payload_on")} label="payload_on" value={fields.payload_on ?? ""} locked={false} onEdit={(_p, v) => setField("payload_on", v)} />
              <TextField path={p("payload_off")} label="payload_off" value={fields.payload_off ?? ""} locked={false} onEdit={(_p, v) => setField("payload_off", v)} />
            </>
          )}
          {sensorType === "ha" && (
            <>
              <TextField path={p("url")} label="url" value={fields.url ?? ""} locked={false} onEdit={(_p, v) => setField("url", v)} placeholder="ws://ha.local:8123/api/websocket" />
              <TextField path={p("entity")} label="entity" value={fields.entity ?? ""} locked={false} onEdit={(_p, v) => setField("entity", v)} />
            </>
          )}
          {sensorType === "usb-ld2410" && (
            <>
              <TextField path={p("port")} label="port" value={fields.port ?? ""} locked={false} onEdit={(_p, v) => setField("port", v)} placeholder="/dev/ttyUSB0" />
              <NumberField path={p("baud")} label="baud" value={fields.baud ?? ""} locked={false} onEdit={(_p, v) => setField("baud", v)} />
            </>
          )}
          <EnumField path={p("kind")} label="kind" value={fields.kind ?? "presence"} locked={false} onEdit={(_p, v) => setField("kind", v)} options={["presence", "motion"]} />
          <DurationField path={p("hold_time")} label="hold_time" value={fields.hold_time ?? ""} locked={false} onEdit={(_p, v) => setField("hold_time", v)} placeholder="2s" />
          <DurationField path={p("stale_timeout")} label="stale_timeout" value={fields.stale_timeout ?? ""} locked={false} onEdit={(_p, v) => setField("stale_timeout", v)} placeholder="300s" />
        </>
      )}

      {collection === "zones" && (
        <>
          <EnumField
            path={p("mode")}
            label="mode"
            value={zoneMode}
            locked={false}
            onEdit={(_p, v) => setZoneMode(v as ZoneMode)}
            options={ZONE_MODES}
          />
          <MultiSelectField
            path={p("members")}
            label="members"
            value={(fields.members as string[]) ?? []}
            locked={false}
            onEdit={(_p, v) => setField("members", v)}
            options={sensorIds}
          />
          <EnumField path={p("unavailable_policy")} label="unavailable_policy" value={fields.unavailable_policy ?? "present"} locked={false} onEdit={(_p, v) => setField("unavailable_policy", v)} options={["present", "absent"]} />
        </>
      )}

      {collection === "displays" && (
        <>
          <MultiSelectField
            path={p("controllers")}
            label="controllers"
            value={(fields.controllers as string[]) ?? []}
            locked={false}
            onEdit={(_p, v) => setField("controllers", v)}
            options={DISPLAY_CONTROLLER_OPTIONS}
          />
          <TextField path={p("host")} label="host" value={fields.host ?? ""} locked={false} onEdit={(_p, v) => setField("host", v)} />
          <EnumField path={p("blank_mode")} label="blank_mode" value={fields.blank_mode ?? "power_off"} locked={false} onEdit={(_p, v) => setField("blank_mode", v)} options={BLANK_MODE_OPTIONS} />
          <TextField path={p("output")} label="output" value={fields.output ?? ""} locked={false} onEdit={(_p, v) => setField("output", v)} />
          <TextField path={p("ddc_display")} label="ddc_display" value={fields.ddc_display ?? ""} locked={false} onEdit={(_p, v) => setField("ddc_display", v)} />
          <TextField path={p("wol_mac")} label="wol_mac" value={fields.wol_mac ?? ""} locked={false} onEdit={(_p, v) => setField("wol_mac", v)} />
          <BoolField path={p("samsung_restore_backlight")} label="samsung_restore_backlight" value={fields.samsung_restore_backlight ?? false} locked={false} onEdit={(_p, v) => setField("samsung_restore_backlight", v)} />
          <NumberField path={p("restore_brightness")} label="restore_brightness" value={fields.restore_brightness ?? ""} locked={false} onEdit={(_p, v) => setField("restore_brightness", v)} />
          <BoolField path={p("treat_unreachable_as_blanked")} label="treat_unreachable_as_blanked" value={fields.treat_unreachable_as_blanked ?? false} locked={false} onEdit={(_p, v) => setField("treat_unreachable_as_blanked", v)} />
          <DurationField path={p("command_timeout")} label="command_timeout" value={fields.command_timeout ?? ""} locked={false} onEdit={(_p, v) => setField("command_timeout", v)} />
        </>
      )}

      {collection === "rules" && (
        <>
          <EnumField path={p("zone")} label="zone" value={fields.zone ?? (zoneIds[0] ?? "")} locked={false} onEdit={(_p, v) => setField("zone", v)} options={zoneIds} />
          <MultiSelectField path={p("displays")} label="displays" value={(fields.displays as string[]) ?? []} locked={false} onEdit={(_p, v) => setField("displays", v)} options={displayIds} />
          <MultiSelectField path={p("inhibitors")} label="inhibitors" value={(fields.inhibitors as string[]) ?? []} locked={false} onEdit={(_p, v) => setField("inhibitors", v)} options={VALID_INHIBITORS} />
          <DurationField path={p("grace_period")} label="grace_period" value={fields.grace_period ?? ""} locked={false} onEdit={(_p, v) => setField("grace_period", v)} placeholder="60s" />
          <DurationField path={p("min_blank_time")} label="min_blank_time" value={fields.min_blank_time ?? ""} locked={false} onEdit={(_p, v) => setField("min_blank_time", v)} />
          <DurationField path={p("min_wake_time")} label="min_wake_time" value={fields.min_wake_time ?? ""} locked={false} onEdit={(_p, v) => setField("min_wake_time", v)} />
          <DurationField path={p("activity_idle_threshold")} label="activity_idle_threshold" value={fields.activity_idle_threshold ?? ""} locked={false} onEdit={(_p, v) => setField("activity_idle_threshold", v)} />
          <DurationField path={p("activity_poll_interval")} label="activity_poll_interval" value={fields.activity_poll_interval ?? ""} locked={false} onEdit={(_p, v) => setField("activity_poll_interval", v)} />
          <NumberField path={p("wake_retries")} label="wake_retries" value={fields.wake_retries ?? ""} locked={false} onEdit={(_p, v) => setField("wake_retries", v)} placeholder="3" />
          <DurationField path={p("wake_retry_backoff")} label="wake_retry_backoff" value={fields.wake_retry_backoff ?? ""} locked={false} onEdit={(_p, v) => setField("wake_retry_backoff", v)} />
          <DurationField path={p("wake_retry_interval")} label="wake_retry_interval" value={fields.wake_retry_interval ?? ""} locked={false} onEdit={(_p, v) => setField("wake_retry_interval", v)} />
        </>
      )}

      <div className="cf-card__actions">
        <button type="button" className="cf-apply__btn cf-apply__btn--apply" onClick={submit} disabled={!idValid}>
          Create
        </button>
        <button type="button" className="cf-apply__btn" onClick={onCancel}>
          Cancel
        </button>
      </div>
    </div>
  );
}
