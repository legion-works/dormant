/**
 * Widget set for the settings form — one component per value type.
 *
 * Every field takes `(path, label, value, locked, onEdit)` plus an
 * optional `lockedReason` (shown in a tooltip on the lock icon) and
 * `error` (inline 422 validation detail rendered below the input).
 */
import { useState, useEffect } from "react";

export { LOG_LEVELS, FUSION_MODES, UNAVAILABLE_POLICIES, IDLE_TIME_UNITS, IDLE_SOURCES, PANEL_TYPES } from "./constants";

export interface FieldProps {
  path: string[];
  label: string;
  value: unknown;
  locked: boolean;
  lockedReason?: string;
  onEdit: (path: string[], value: unknown) => void;
  /** Inline 422 validation error detail for this field. */
  error?: string;
  /** Guidance text shown below the control. */
  help?: string;
  /** Placeholder attribute for text/number/duration inputs. */
  placeholder?: string;
}

/* ── shared locked-input shell ── */

function LockIcon({ reason }: { reason?: string }) {
  return (
    <span className="cf-field__lock" title={reason ?? ""} aria-label={reason}>
      {"🔒"}
    </span>
  );
}

function fieldClassName(locked: boolean, error: boolean): string {
  let cls = "cf-field";
  if (locked) cls += " cf-field--locked";
  if (error) cls += " cf-field--error";
  return cls;
}

/* ── individual field components ── */

/**
 * Duration field — text input with client-side humantime validation.
 *
 * Accepts patterns like `5s`, `2m 30s`, `1h`.  The server is the
 * ultimate authority; the client only shows a hint for obviously
 * invalid input.
 */
export function DurationField({ path, label, value, locked, lockedReason, onEdit, error, help, placeholder }: FieldProps) {
  const raw = typeof value === "string" ? value : String(value ?? "");
  const durRx = /^\d+(ms|s|m|h)( \d+(ms|s|m|h))*$/;

  return (
    <div className={fieldClassName(locked, !!error)}>
      <label className="cf-field__label" htmlFor={path.join(".")}>{label}</label>
      <div className="cf-field__input-row">
        <input
          id={path.join(".")}
          type="text"
          className="cf-field__input"
          value={raw}
          disabled={locked}
          placeholder={placeholder}
          onChange={(e) => onEdit(path, e.target.value)}
        />
        {locked && <LockIcon reason={lockedReason} />}
      </div>
      {!locked && raw.length > 0 && !durRx.test(raw) && (
        <span className="cf-field__hint">expected: 5s, 1m 30s, 2h</span>
      )}
      {help && <span className="cf-field__hint">{help}</span>}
      {error && <span className="cf-field__error">{error}</span>}
    </div>
  );
}

/** Enum field — <select> with a fixed set of options. */
export function EnumField({ path, label, value, locked, lockedReason, onEdit, error, help, options }: FieldProps & { options: readonly string[] }) {
  const current = typeof value === "string" ? value : String(value ?? "");

  return (
    <div className={fieldClassName(locked, !!error)}>
      <label className="cf-field__label" htmlFor={path.join(".")}>{label}</label>
      <div className="cf-field__input-row">
        <select
          id={path.join(".")}
          className="cf-field__select"
          value={current}
          disabled={locked}
          onChange={(e) => onEdit(path, e.target.value)}
        >
          {options.map((opt) => (
            <option key={opt} value={opt}>{opt}</option>
          ))}
        </select>
        {locked && <LockIcon reason={lockedReason} />}
      </div>
      {help && <span className="cf-field__hint">{help}</span>}
      {error && <span className="cf-field__error">{error}</span>}
    </div>
  );
}

/** Boolean field — checkbox toggle. */
export function BoolField({ path, label, value, locked, lockedReason, onEdit, error, help }: FieldProps) {
  const checked = Boolean(value);

  return (
    <div className={fieldClassName(locked, !!error)}>
      <label className="cf-field__label cf-field__label--checkbox">
        <input
          type="checkbox"
          className="cf-field__checkbox"
          checked={checked}
          disabled={locked}
          onChange={(e) => onEdit(path, e.target.checked)}
        />
        <span>{label}</span>
      </label>
      {locked && <LockIcon reason={lockedReason} />}
      {help && <span className="cf-field__hint">{help}</span>}
      {error && <span className="cf-field__error">{error}</span>}
    </div>
  );
}

/** Number field — numeric input. */
export function NumberField({ path, label, value, locked, lockedReason, onEdit, error, help, placeholder }: FieldProps) {
  const raw = typeof value === "number" ? String(value) : String(value ?? "");

  return (
    <div className={fieldClassName(locked, !!error)}>
      <label className="cf-field__label" htmlFor={path.join(".")}>{label}</label>
      <div className="cf-field__input-row">
        <input
          id={path.join(".")}
          type="number"
          className="cf-field__input"
          value={raw}
          disabled={locked}
          placeholder={placeholder}
          onChange={(e) => {
            const n = Number(e.target.value);
            if (!Number.isNaN(n)) onEdit(path, n);
          }}
        />
        {locked && <LockIcon reason={lockedReason} />}
      </div>
      {help && <span className="cf-field__hint">{help}</span>}
      {error && <span className="cf-field__error">{error}</span>}
    </div>
  );
}

/** Plain text field. */
export function TextField({ path, label, value, locked, lockedReason, onEdit, error, help, placeholder }: FieldProps) {
  const raw = typeof value === "string" ? value : String(value ?? "");

  return (
    <div className={fieldClassName(locked, !!error)}>
      <label className="cf-field__label" htmlFor={path.join(".")}>{label}</label>
      <div className="cf-field__input-row">
        <input
          id={path.join(".")}
          type="text"
          className="cf-field__input"
          value={raw}
          disabled={locked}
          placeholder={placeholder}
          onChange={(e) => onEdit(path, e.target.value)}
        />
        {locked && <LockIcon reason={lockedReason} />}
      </div>
      {help && <span className="cf-field__hint">{help}</span>}
      {error && <span className="cf-field__error">{error}</span>}
    </div>
  );
}

/**
 * String-list field — add/remove/edit a `Vec<String>`-shaped value
 * (e.g. `audio.call_roles`, and the inner list of an optional
 * string-list field like `audio.playback_roles`).
 *
 * Owns its working array as internal state, seeded from `value` and
 * re-synced only when the `value` *reference* itself changes (a fresh
 * fetch/discard/reload passes a new array). This means repeated
 * add/remove/edit interactions accumulate correctly against the same
 * mount even when the host re-renders with the same (stale) `value`
 * prop between keystrokes — the existing scalar fields in this file
 * rely on the host re-supplying `value` from pending store state,
 * which doesn't hold for a widget that must support several
 * micro-edits (add, then edit, then remove) before Apply.
 */
export function StringListField({ path, label, value, locked, lockedReason, onEdit, error, help, placeholder }: FieldProps) {
  const [items, setItems] = useState<string[]>(() => (Array.isArray(value) ? [...(value as string[])] : []));
  const [draft, setDraft] = useState("");

  // Re-sync when the host hands us a genuinely new array (fresh fetch/
  // discard/reload) — not on every render, since `value` is typically
  // the same reference across re-renders triggered by our own edits.
  useEffect(() => {
    setItems(Array.isArray(value) ? [...(value as string[])] : []);
  }, [value]);

  function commit(next: string[]) {
    setItems(next);
    onEdit(path, next);
  }

  function addEntry() {
    const trimmed = draft.trim();
    if (!trimmed) return;
    commit([...items, trimmed]);
    setDraft("");
  }

  function removeAt(idx: number) {
    commit(items.filter((_, i) => i !== idx));
  }

  function editAt(idx: number, next: string) {
    const copy = [...items];
    copy[idx] = next;
    commit(copy);
  }

  const addInputId = `${path.join(".")}-add`;

  return (
    <div className={fieldClassName(locked, !!error)}>
      <label className="cf-field__label" htmlFor={addInputId}>{label}</label>
      <div className="cf-field__value-list">
        {items.map((item, idx) =>
          locked ? (
            <span key={idx} className="cf-field__value-chip">{item}</span>
          ) : (
            <span key={idx} className="cf-field__value-chip" style={{ display: "flex", alignItems: "center", gap: "4px" }}>
              <input
                type="text"
                className="cf-field__input"
                style={{ width: "auto", maxWidth: "160px", padding: "3px 6px" }}
                value={item}
                aria-label={`${label} item ${idx + 1}`}
                onChange={(e) => editAt(idx, e.target.value)}
              />
              <button
                type="button"
                className="cf-apply__btn cf-apply__btn--discard"
                style={{ padding: "2px 6px", fontSize: "10px" }}
                onClick={() => removeAt(idx)}
                aria-label={`Remove ${label} item ${idx + 1}`}
                title={`Remove ${label} item ${idx + 1}`}
              >
                {"✕"}
              </button>
            </span>
          ),
        )}
        {locked && <LockIcon reason={lockedReason} />}
      </div>
      {!locked && (
        <div className="cf-field__input-row">
          <input
            id={addInputId}
            type="text"
            className="cf-field__input"
            value={draft}
            placeholder={placeholder}
            onChange={(e) => setDraft(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") {
                e.preventDefault();
                addEntry();
              }
            }}
          />
          <button
            type="button"
            className="cf-apply__btn"
            onClick={addEntry}
            aria-label={`Add ${label}`}
          >
            {"+ Add"}
          </button>
        </div>
      )}
      {help && <span className="cf-field__hint">{help}</span>}
      {error && <span className="cf-field__error">{error}</span>}
    </div>
  );
}
