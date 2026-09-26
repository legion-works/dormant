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
import { DurationField, EnumField, NumberField, HexCodeField, BoolField, TextField, MultiSelectField } from "./fields";
import {
  validateEntityId,
  VALID_INHIBITORS,
  DISPLAY_CONTROLLER_OPTIONS,
  FIELD_HELP,
  FIELD_EXAMPLE,
} from "./entityCrud";
import type { CrudCollection } from "./entityCrud";
import type { ConfigPatch } from "../../api/types";

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
   * Existing zone id -> current member list, used by the sensor form's
   * "Add to zone" affordance to compute a whole-array `members` set
   * patch (the server's `config_patch.rs` accepts it: `zones.<id>.members`
   * is a known path and the value is an array, not a container).
   */
  zoneMembers?: Record<string, string[]>;
  /**
   * Seed values for the non-id fields (e.g. the pairing wizard's
   * post-pair "create display?" hand-off, spec §8.3: `{host,
   * controllers: ["samsung-tizen"]}`). Only read once, at mount — this
   * component is always freshly mounted when a caller flips it into
   * view (a ternary swap, not a persistent instance), so there's no
   * stale-prop concern.
   */
  initialFields?: Record<string, unknown>;
  /**
   * Called with the new id, the create-entity payload, and an optional
   * list of extra patches (currently only used for the sensor
   * "Add to zone" affordance that appends the new sensor id to an
   * existing zone's `members`).
   */
  onCreate: (id: string, value: Record<string, unknown>, extra?: ConfigPatch[]) => void;
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
  zoneMembers,
  initialFields,
  onCreate,
  onCancel,
}: CreateEntityFormProps) {
  const [id, setId] = useState("");
  const [sensorType, setSensorType] = useState<SensorType>("mqtt");
  const [zoneMode, setZoneMode] = useState<ZoneMode>("any");
  const [displayScope, setDisplayScope] = useState<"private" | "shared">("private");
  const [addToZone, setAddToZone] = useState("");
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
    if (collection === "displays") value.scope = displayScope;
    for (const key of Object.keys(value)) {
      const v = value[key];
      if (v === "" || v === undefined) delete value[key];
      if (Array.isArray(v) && v.length === 0) delete value[key];
    }
    return value;
  }

  function submit() {
    if (!idValid) return;
    const value = buildValue();
    const extra: ConfigPatch[] = [];
    if (collection === "sensors" && addToZone && zoneMembers) {
      // Sensor create → also append the new sensor id to the chosen zone's members.
      // The patch store's last-write-wins semantics make this safe even when
      // the user already had a pending `zones.<id>.members` edit.
      const current = zoneMembers[addToZone] ?? [];
      if (!current.includes(id)) {
        extra.push({
          op: "set",
          path: ["zones", addToZone, "members"],
          value: [...current, id],
        });
      }
    }
    onCreate(id, value, extra.length > 0 ? extra : undefined);
  }

  function fieldHelp(key: string): string | undefined {
    return FIELD_HELP[collection]?.[key];
  }

  function fieldExample(key: string): string | undefined {
    return FIELD_EXAMPLE[collection]?.[key];
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
            help={fieldHelp("type")}
          />
          {sensorType === "mqtt" && (
            <>
              <TextField path={p("broker_url")} label="broker_url" value={fields.broker_url ?? ""} locked={false} onEdit={(_p, v) => setField("broker_url", v)} placeholder={fieldExample("broker_url") ?? "mqtt://host:1883"} help={fieldHelp("broker_url")} />
              <TextField path={p("topic")} label="topic" value={fields.topic ?? ""} locked={false} onEdit={(_p, v) => setField("topic", v)} placeholder="zigbee2mqtt/desk-sensor" help={fieldHelp("topic")} />
              <TextField path={p("field")} label="field" value={fields.field ?? ""} locked={false} onEdit={(_p, v) => setField("field", v)} placeholder="/occupancy" help={fieldHelp("field")} />
              <TextField path={p("payload_on")} label="payload_on" value={fields.payload_on ?? ""} locked={false} onEdit={(_p, v) => setField("payload_on", v)} placeholder="ON" help={fieldHelp("payload_on")} />
              <TextField path={p("payload_off")} label="payload_off" value={fields.payload_off ?? ""} locked={false} onEdit={(_p, v) => setField("payload_off", v)} placeholder="OFF" help={fieldHelp("payload_off")} />
            </>
          )}
          {sensorType === "ha" && (
            <>
              <TextField path={p("url")} label="url" value={fields.url ?? ""} locked={false} onEdit={(_p, v) => setField("url", v)} placeholder={fieldExample("url") ?? "ws://ha.local:8123/api/websocket"} help={fieldHelp("url")} />
              <TextField path={p("entity")} label="entity" value={fields.entity ?? ""} locked={false} onEdit={(_p, v) => setField("entity", v)} placeholder="binary_sensor.couch_presence" help={fieldHelp("entity")} />
            </>
          )}
          {sensorType === "usb-ld2410" && (
            <>
              <TextField path={p("port")} label="port" value={fields.port ?? ""} locked={false} onEdit={(_p, v) => setField("port", v)} placeholder={fieldExample("port") ?? "/dev/ttyUSB0"} help={fieldHelp("port")} />
              <NumberField path={p("baud")} label="baud" value={fields.baud ?? ""} locked={false} onEdit={(_p, v) => setField("baud", v)} placeholder="256000" help={fieldHelp("baud")} />
            </>
          )}
          <EnumField path={p("kind")} label="kind" value={fields.kind ?? "presence"} locked={false} onEdit={(_p, v) => setField("kind", v)} options={["presence", "motion"]} help={fieldHelp("kind")} />
          <DurationField path={p("hold_time")} label="hold_time" value={fields.hold_time ?? ""} locked={false} onEdit={(_p, v) => setField("hold_time", v)} placeholder={fieldExample("hold_time") ?? "2s"} help={fieldHelp("hold_time")} />
          <DurationField path={p("stale_timeout")} label="stale_timeout" value={fields.stale_timeout ?? ""} locked={false} onEdit={(_p, v) => setField("stale_timeout", v)} placeholder={fieldExample("stale_timeout") ?? "300s"} help={fieldHelp("stale_timeout")} />
          {zoneIds.length > 0 && zoneMembers && (
            <EnumField
              path={p("add_to_zone")}
              label="add to zone (optional)"
              value={addToZone}
              locked={false}
              onEdit={(_p, v) => setAddToZone(typeof v === "string" ? v : "")}
              options={["", ...zoneIds]}
              help="If set, the new sensor is appended to this zone's members. The create patch and the members patch are tracked together; safe with concurrent edits (last-write-wins)."
            />
          )}
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
            help={fieldHelp("mode")}
          />
          <MultiSelectField
            path={p("members")}
            label="members"
            value={(fields.members as string[]) ?? []}
            locked={false}
            onEdit={(_p, v) => setField("members", v)}
            options={sensorIds}
            help={fieldHelp("members")}
          />
          <EnumField path={p("unavailable_policy")} label="unavailable_policy" value={fields.unavailable_policy ?? "present"} locked={false} onEdit={(_p, v) => setField("unavailable_policy", v)} options={["present", "absent"]} help={fieldHelp("unavailable_policy")} />
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
            help={fieldHelp("controllers")}
          />
          <EnumField path={p("scope")} label="scope" value={displayScope} locked={false} onEdit={(_p, v) => setDisplayScope(v as "private" | "shared")} options={["private", "shared"]} help="Private displays are owned by this machine only; shared displays participate in multi-machine KVM switching." />
          {displayScope === "shared" && (
            <>
              <HexCodeField path={p("shared_input_code")} label="shared_input_code" value={fields.shared_input_code ?? ""} locked={false} onEdit={(_p, v) => setField("shared_input_code", v)} help={fieldHelp("shared_input_code")} />
              <HexCodeField path={p("shared_input_write_code")} label="shared_input_write_code (optional)" value={fields.shared_input_write_code ?? ""} locked={false} onEdit={(_p, v) => setField("shared_input_write_code", v)} help={fieldHelp("shared_input_write_code")} />
              <HexCodeField path={p("shared_peer_input_code")} label="shared_peer_input_code (optional peer read code)" value={fields.shared_peer_input_code ?? ""} locked={false} onEdit={(_p, v) => setField("shared_peer_input_code", v)} help={fieldHelp("shared_peer_input_code")} />
              <HexCodeField path={p("shared_peer_input_write_code")} label="shared_peer_input_write_code (optional peer write code)" value={fields.shared_peer_input_write_code ?? ""} locked={false} onEdit={(_p, v) => setField("shared_peer_input_write_code", v)} help={fieldHelp("shared_peer_input_write_code")} />
            </>
          )}
          <TextField path={p("host")} label="host" value={fields.host ?? ""} locked={false} onEdit={(_p, v) => setField("host", v)} placeholder={fieldExample("host")} help={fieldHelp("host")} />
          <EnumField path={p("blank_mode")} label="blank_mode" value={fields.blank_mode ?? "power_off"} locked={false} onEdit={(_p, v) => setField("blank_mode", v)} options={BLANK_MODE_OPTIONS} help={fieldHelp("blank_mode")} />
          <TextField path={p("output")} label="output" value={fields.output ?? ""} locked={false} onEdit={(_p, v) => setField("output", v)} placeholder={fieldExample("output")} help={fieldHelp("output")} />
          <TextField path={p("ddc_display")} label="ddc_display" value={fields.ddc_display ?? ""} locked={false} onEdit={(_p, v) => setField("ddc_display", v)} placeholder={fieldExample("ddc_display")} help={fieldHelp("ddc_display")} />
          <TextField path={p("wol_mac")} label="wol_mac" value={fields.wol_mac ?? ""} locked={false} onEdit={(_p, v) => setField("wol_mac", v)} placeholder={fieldExample("wol_mac")} help={fieldHelp("wol_mac")} />
          <BoolField path={p("samsung_restore_backlight")} label="samsung_restore_backlight" value={fields.samsung_restore_backlight ?? false} locked={false} onEdit={(_p, v) => setField("samsung_restore_backlight", v)} help={fieldHelp("samsung_restore_backlight")} />
          <NumberField path={p("restore_brightness")} label="restore_brightness" value={fields.restore_brightness ?? ""} locked={false} onEdit={(_p, v) => setField("restore_brightness", v)} placeholder={fieldExample("restore_brightness")} help={fieldHelp("restore_brightness")} />
          <BoolField path={p("treat_unreachable_as_blanked")} label="treat_unreachable_as_blanked" value={fields.treat_unreachable_as_blanked ?? false} locked={false} onEdit={(_p, v) => setField("treat_unreachable_as_blanked", v)} help={fieldHelp("treat_unreachable_as_blanked")} />
          <DurationField path={p("command_timeout")} label="command_timeout" value={fields.command_timeout ?? ""} locked={false} onEdit={(_p, v) => setField("command_timeout", v)} placeholder={fieldExample("command_timeout")} help={fieldHelp("command_timeout")} />
        </>
      )}

      {collection === "rules" && (
        <>
          <EnumField path={p("zone")} label="zone" value={fields.zone ?? (zoneIds[0] ?? "")} locked={false} onEdit={(_p, v) => setField("zone", v)} options={zoneIds} help={fieldHelp("zone")} />
          <MultiSelectField path={p("displays")} label="displays" value={(fields.displays as string[]) ?? []} locked={false} onEdit={(_p, v) => setField("displays", v)} options={displayIds} help={fieldHelp("displays")} />
          <MultiSelectField path={p("inhibitors")} label="inhibitors" value={(fields.inhibitors as string[]) ?? []} locked={false} onEdit={(_p, v) => setField("inhibitors", v)} options={VALID_INHIBITORS} help={fieldHelp("inhibitors")} />
          <DurationField path={p("grace_period")} label="grace_period" value={fields.grace_period ?? ""} locked={false} onEdit={(_p, v) => setField("grace_period", v)} placeholder={fieldExample("grace_period") ?? "60s"} help={fieldHelp("grace_period")} />
          <DurationField path={p("min_blank_time")} label="min_blank_time" value={fields.min_blank_time ?? ""} locked={false} onEdit={(_p, v) => setField("min_blank_time", v)} placeholder={fieldExample("min_blank_time")} help={fieldHelp("min_blank_time")} />
          <DurationField path={p("min_wake_time")} label="min_wake_time" value={fields.min_wake_time ?? ""} locked={false} onEdit={(_p, v) => setField("min_wake_time", v)} placeholder={fieldExample("min_wake_time")} help={fieldHelp("min_wake_time")} />
          <DurationField path={p("activity_idle_threshold")} label="activity_idle_threshold" value={fields.activity_idle_threshold ?? ""} locked={false} onEdit={(_p, v) => setField("activity_idle_threshold", v)} placeholder={fieldExample("activity_idle_threshold")} help={fieldHelp("activity_idle_threshold")} />
          <DurationField path={p("activity_poll_interval")} label="activity_poll_interval" value={fields.activity_poll_interval ?? ""} locked={false} onEdit={(_p, v) => setField("activity_poll_interval", v)} placeholder={fieldExample("activity_poll_interval")} help={fieldHelp("activity_poll_interval")} />
          <NumberField path={p("wake_retries")} label="wake_retries" value={fields.wake_retries ?? ""} locked={false} onEdit={(_p, v) => setField("wake_retries", v)} placeholder={fieldExample("wake_retries") ?? "3"} help={fieldHelp("wake_retries")} />
          <DurationField path={p("wake_retry_backoff")} label="wake_retry_backoff" value={fields.wake_retry_backoff ?? ""} locked={false} onEdit={(_p, v) => setField("wake_retry_backoff", v)} placeholder={fieldExample("wake_retry_backoff")} help={fieldHelp("wake_retry_backoff")} />
          <DurationField path={p("wake_retry_interval")} label="wake_retry_interval" value={fields.wake_retry_interval ?? ""} locked={false} onEdit={(_p, v) => setField("wake_retry_interval", v)} placeholder={fieldExample("wake_retry_interval")} help={fieldHelp("wake_retry_interval")} />
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
