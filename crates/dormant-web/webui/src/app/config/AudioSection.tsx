/**
 * Audio (PipeWire inhibitor) settings section — the `[audio]` TOML
 * section, mirroring NotificationsSection/WearSection's known-field
 * guidance pattern.
 *
 * Six keys total (`crates/dormant-core/src/config/schema.rs`
 * `AudioConfig`):
 *   - poll_interval, min_active   — duration scalars.
 *   - call_roles                  — `Vec<String>`, always present.
 *   - playback_roles              — `Option<Vec<String>>`. `None` means
 *     "any non-call running output stream inhibits" (the permissive
 *     default) and is distinct from `Some([])`, which server-side
 *     validation rejects outright. Rendered as an enable-toggle + a
 *     StringListField: unchecked = unset (no patch until touched),
 *     checked = a tracked list edit; unchecking again after a list was
 *     set emits a `remove` patch (mirrors the store's existing
 *     edit-then-remove collapse — see DisplaysSection's blank/ladder
 *     mode-switch precedent for the same "derive effective state from
 *     store.getEdit, fall back to fetched" shape).
 *   - capture_is_call             — bool.
 *   - pw_dump_command             — SECURITY (spec §6#10): config-driven
 *     command execution. Rendered permanently locked/read-only — mirrors
 *     RulesSection's read-only `inhibitors` treatment (`.cf-field--locked`,
 *     `.cf-field__value-chip`... here a single value chip, lock icon) —
 *     and is EXCLUDED from patch emission entirely: no onEdit handler is
 *     ever wired to it, so it can never appear in buildPatches().
 */
import { useState, useEffect } from "react";
import FormSection from "./FormSection";
import { BoolField, DurationField, StringListField } from "./fields";
import type { FieldProps } from "./fields";
import type { PatchStore } from "./patch";

interface AudioSectionProps {
  audio: Record<string, unknown> | undefined;
  store: PatchStore;
  redactedPaths: string[][];
  onDirty: () => void;
  fieldErrors: Record<string, string | undefined>;
}

/** Known audio keys with explicit widget choices (all six — no fallback branch needed). */
const KNOWN_FIELDS: Record<string, { kind: "bool" | "duration" | "string_list" }> = {
  poll_interval: { kind: "duration" },
  min_active: { kind: "duration" },
  call_roles: { kind: "string_list" },
  capture_is_call: { kind: "bool" },
};

/** Per-field help text — accurate to the real config semantics. */
const FIELD_HELP: Record<string, string> = {
  poll_interval: "How often to poll pw_dump_command for the current PipeWire graph.",
  min_active: "Minimum continuous stream activity before the audio inhibitor asserts (debounces transient blips). Deassertion is immediate.",
  call_roles: "media.role values that mean \"this running stream is a call\".",
  capture_is_call: "Whether a running input stream (an open microphone) counts as a call. Off by default — many setups idle with a mic node running.",
};

const FIELD_PLACEHOLDER: Record<string, string> = {
  poll_interval: "5s",
  min_active: "3s",
};

/** Bespoke field: playback_roles is `Option<Vec<String>>` — see module docstring. */
function PlaybackRolesField({
  audio,
  store,
  redactedPaths,
  onDirty,
  fieldErrors,
}: {
  audio: Record<string, unknown>;
  store: PatchStore;
  redactedPaths: string[][];
  onDirty: () => void;
  fieldErrors: Record<string, string | undefined>;
}) {
  const path = ["audio", "playback_roles"];
  const fetched = audio.playback_roles;
  const fetchedList = Array.isArray(fetched) ? (fetched as string[]) : null;

  // Render state lives locally (not derived from store.getEdit()): the
  // store can't distinguish "never touched" from "explicitly removed"
  // (both read back undefined from getEdit), and driving `isSet` off
  // React state guarantees the checkbox re-renders on every toggle even
  // when the host doesn't force a re-render synchronously (onDirty is a
  // fire-and-forget notification, not something this component can rely
  // on for its own reactivity). Re-synced whenever the fetched `audio`
  // object identity changes (fresh fetch/discard/reload).
  const [isSet, setIsSet] = useState(fetchedList !== null);
  const [list, setList] = useState<string[]>(fetchedList ?? []);
  useEffect(() => {
    setIsSet(fetchedList !== null);
    setList(fetchedList ?? []);
  }, [audio, fetchedList]);

  const locked = store.isLocked(path, redactedPaths);

  function handleToggle(checked: boolean) {
    setIsSet(checked);
    if (checked) {
      // F16: an empty list is rejected server-side. Track nothing until
      // the first role is added via handleListChange below.
      if (list.length > 0) {
        store.trackEdit(path, list);
      }
    } else if (list.length > 0 || fetchedList !== null) {
      // Only emit a remove when there is something to remove: a
      // previously-set (fetched or in-progress) list. Checking the box
      // and unchecking it again with no roles ever added tracked
      // nothing above, so there's nothing pending to clear here either.
      store.trackRemove(path);
    }
    onDirty();
  }

  function handleListChange(next: string[]) {
    setList(next);
    if (next.length > 0) {
      store.trackEdit(path, next);
    } else if (fetchedList !== null) {
      // Cleared back to empty after a committed value existed — remove
      // rather than emit F16's `playback_roles = []`.
      store.trackRemove(path);
    }
    onDirty();
  }

  const error = fieldErrors[path.join(".")];

  return (
    <div className={`cf-field${locked ? " cf-field--locked" : ""}`}>
      <label className="cf-field__label cf-field__label--checkbox">
        <input
          type="checkbox"
          className="cf-field__checkbox"
          checked={isSet}
          disabled={locked}
          onChange={(e) => handleToggle(e.target.checked)}
        />
        <span>Restrict playback_roles by role</span>
      </label>
      {!isSet && (
        <span className="cf-field__hint">
          unset — any role (every non-call running output stream inhibits)
        </span>
      )}
      {isSet && (
        <StringListField
          path={path}
          label="playback_roles"
          value={list}
          locked={locked}
          onEdit={(_p, v) => handleListChange(v as string[])}
          error={error}
          help="media.role values that inhibit playback blanking. An empty list is rejected by validation — add at least one role or uncheck to allow any role."
        />
      )}
    </div>
  );
}

export default function AudioSection({ audio, store, redactedPaths, onDirty, fieldErrors }: AudioSectionProps) {
  const inv = audio ?? {};
  const keys = Object.keys(inv);
  if (keys.length === 0) return null;

  return (
    <FormSection title="Audio">
      <div className="cf-card">
        {keys.map((key) => {
          if (key === "playback_roles") {
            return (
              <PlaybackRolesField
                key={key}
                audio={inv}
                store={store}
                redactedPaths={redactedPaths}
                onDirty={onDirty}
                fieldErrors={fieldErrors}
              />
            );
          }

          if (key === "pw_dump_command") {
            const value = inv[key];
            return (
              <div key={key} className="cf-field cf-field--locked">
                <label className="cf-field__label">pw_dump_command</label>
                <div className="cf-field__value-row">
                  <span className="cf-field__value-text">{String(value ?? "")}</span>
                  <span
                    className="cf-field__lock"
                    title="not editable in v1 — feature 05 will gate this"
                    aria-label="not editable in v1 — feature 05 will gate this"
                  >
                    {"🔒"}
                  </span>
                </div>
                <span className="cf-field__hint">
                  Config-driven command execution — locked pending the feature-05 gate.
                </span>
              </div>
            );
          }

          const path = ["audio", key];
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

          if (!known) return null;

          switch (known.kind) {
            case "bool":
              return <BoolField key={key} {...shared} />;
            case "duration":
              return <DurationField key={key} {...shared} />;
            case "string_list":
              return <StringListField key={key} {...shared} />;
            default:
              return null;
          }
        })}
      </div>
    </FormSection>
  );
}
