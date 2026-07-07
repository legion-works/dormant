/**
 * Widget set for the settings form — one component per value type.
 *
 * Every field takes `(path, label, value, locked, onEdit)` plus an
 * optional `lockedReason` (shown in a tooltip on the lock icon) and
 * `error` (inline 422 validation detail rendered below the input).
 */

/** Log levels matching Rust's tracing crate + dormant's custom levels. */
export const LOG_LEVELS = ["trace", "debug", "info", "warn", "error"] as const;

/** Zone fusion modes. */
export const FUSION_MODES = ["any", "all", "quorum", "weighted"] as const;

/** Unavailable-policy options. */
export const UNAVAILABLE_POLICIES = ["present", "absent"] as const;

export interface FieldProps {
  path: string[];
  label: string;
  value: unknown;
  locked: boolean;
  lockedReason?: string;
  onEdit: (path: string[], value: unknown) => void;
  /** Inline 422 validation error detail for this field. */
  error?: string;
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
export function DurationField({ path, label, value, locked, lockedReason, onEdit, error }: FieldProps) {
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
          onChange={(e) => onEdit(path, e.target.value)}
        />
        {locked && <LockIcon reason={lockedReason} />}
      </div>
      {!locked && raw.length > 0 && !durRx.test(raw) && (
        <span className="cf-field__hint">expected: 5s, 1m 30s, 2h</span>
      )}
      {error && <span className="cf-field__error">{error}</span>}
    </div>
  );
}

/** Enum field — <select> with a fixed set of options. */
export function EnumField({ path, label, value, locked, lockedReason, onEdit, error, options }: FieldProps & { options: readonly string[] }) {
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
      {error && <span className="cf-field__error">{error}</span>}
    </div>
  );
}

/** Boolean field — checkbox toggle. */
export function BoolField({ path, label, value, locked, lockedReason, onEdit, error }: FieldProps) {
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
      {error && <span className="cf-field__error">{error}</span>}
    </div>
  );
}

/** Number field — numeric input. */
export function NumberField({ path, label, value, locked, lockedReason, onEdit, error }: FieldProps) {
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
          onChange={(e) => {
            const n = Number(e.target.value);
            if (!Number.isNaN(n)) onEdit(path, n);
          }}
        />
        {locked && <LockIcon reason={lockedReason} />}
      </div>
      {error && <span className="cf-field__error">{error}</span>}
    </div>
  );
}

/** Plain text field. */
export function TextField({ path, label, value, locked, lockedReason, onEdit, error }: FieldProps) {
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
          onChange={(e) => onEdit(path, e.target.value)}
        />
        {locked && <LockIcon reason={lockedReason} />}
      </div>
      {error && <span className="cf-field__error">{error}</span>}
    </div>
  );
}
