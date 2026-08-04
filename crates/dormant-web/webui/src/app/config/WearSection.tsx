/**
 * Wear (panel-exposure) settings section — the `[wear]` TOML section.
 *
 * Renders each known `wear.*` key with the appropriate widget.
 * W1-5: 230px label column + changed-field markers.
 */
import FormSection from "./FormSection";
import { BoolField, DurationField, EnumField, MultiSelectField, NumberField, TextField } from "./fields";
import type { FieldProps } from "./fields";
import type { PatchStore } from "./patch";
import type { DisplayConfig, WearConfig } from "../../api/types";

interface WearSectionProps {
  wear: WearConfig | undefined;
  displays?: Record<string, DisplayConfig>;
  store: PatchStore;
  redactedPaths: string[][];
  onDirty: () => void;
  fieldErrors: Record<string, string | undefined>;
}

const KNOWN_FIELDS: Record<string, { kind: "bool" | "number" | "duration" | "text" }> = {
  enabled: { kind: "bool" },
  sample_interval: { kind: "duration" },
  persist_interval: { kind: "duration" },
  read_timeout: { kind: "duration" },
  grid_rows: { kind: "number" },
  grid_cols: { kind: "number" },
  fallback_brightness: { kind: "number" },
  screensaver_factor: { kind: "number" },
  short_cycle_dwell: { kind: "duration" },
  advisory_after: { kind: "duration" },
};

const FIELD_HELP: Record<string, string> = {
  enabled: "Enable panel-wear tracking. On by default.",
  sample_interval: "How often to sample panel state for wear attribution.",
  persist_interval: "How often to persist the wear ledger to disk.",
  read_timeout: "Timeout for a single panel-state read during sampling.",
  grid_rows: "Number of rows in the wear-attribution grid.",
  grid_cols: "Number of columns in the wear-attribution grid.",
  fallback_brightness: "Brightness fraction (0.0-1.0) assumed when the real brightness can't be read.",
  screensaver_factor: "Brightness fraction (0.0-1.0) attributed while the screensaver is active.",
  short_cycle_dwell: "Minimum dwell before a blank/wake cycle counts as a full cycle.",
  advisory_after: "Panel age (accumulated on-hours) after which wear advisories start surfacing.",
};

const FIELD_PLACEHOLDER: Record<string, string> = {
  sample_interval: "60s",
  persist_interval: "300s",
  read_timeout: "2s",
  short_cycle_dwell: "600s",
  advisory_after: "96h",
};

/**
 * Render-eligibility predicate — mirrors `DisplayConfig::is_render_eligible`
 * in `crates/dormant-core/src/config/schema.rs`. A display is render-eligible
 * when at least one local controller (`kwin-dpms` / `ddcci` / `command`) is
 * present AND the controller list is not composed solely of remote
 * controllers (`samsung-tizen` / `ha-passthrough`). The two predicates
 * must remain disjoint — the composite `is_sampling_eligible` widens this
 * with a compositor_output branch so the wear-sampling selector can flip
 * independently once source-gating lands.
 */
function isRenderEligible(display: DisplayConfig): boolean {
  const LOCAL = new Set(["kwin-dpms", "ddcci", "command"]);
  const REMOTE = new Set(["samsung-tizen", "ha-passthrough"]);
  const hasLocal = display.controllers.some((c) => LOCAL.has(c));
  const onlyRemote = display.controllers.every((c) => REMOTE.has(c));
  return hasLocal && !onlyRemote;
}

/**
 * Sampling-eligibility predicate — mirrors `DisplayConfig::is_sampling_eligible`
 * in `crates/dormant-core/src/config/schema.rs`. A display is sampling-eligible
 * when it is render-eligible OR it carries an explicit non-empty
 * `compositor_output` declaration. The compositor_output branch is what
 * makes a remote-only TV show up in the wear sampled-displays list — the
 * render path can never target it (no local controller), but the active
 * sampler can observe its panel via a declared compositor output.
 */
function isSamplingEligible(display: DisplayConfig): boolean {
  if (isRenderEligible(display)) return true;
  const out = display.compositor_output;
  return typeof out === "string" && out.trim().length > 0;
}

function ActiveSamplingFields({ value, displays, store, redactedPaths, onDirty, fieldErrors }: {
  value: Record<string, unknown>;
  displays: Record<string, DisplayConfig>;
  store: PatchStore;
  redactedPaths: string[][];
  onDirty: () => void;
  fieldErrors: Record<string, string | undefined>;
}) {
  const displayOptions = Object.entries(displays)
    .filter(([, display]) => isSamplingEligible(display))
    .map(([id]) => id);
  // Canonical multi-display field renders the plural selector when the
  // config carries a list; the legacy singular row is hidden so the
  // operator cannot mix keys (the server rejects both keys present).
  const plural = Array.isArray(value["sampled_displays"]);
  const fields: Array<{ key: string; kind: "bool" | "duration" | "number" | "enum" | "multiselect"; options?: readonly string[] }> = [
    { key: "enabled", kind: "bool" },
    ...(plural
      ? [{ key: "sampled_displays", kind: "multiselect" as const, options: displayOptions }]
      : [{ key: "sampled_display", kind: "enum" as const, options: displayOptions }]),
    { key: "stream_mode", kind: "enum", options: ["warm", "per-tick"] },
    { key: "capture_timeout", kind: "duration" },
    { key: "failure_threshold", kind: "number" },
    { key: "circuit_reset_after", kind: "duration" },
  ];

  return (
    <div className="cf-card" data-testid="active-sampling-fields">
      <h3>Active sampling</h3>
      {fields.map(({ key, kind, options }) => {
        const path = ["wear", "active_sampling", key];
        const locked = store.isLocked(path, redactedPaths);
        const shared: FieldProps = {
          path, label: `active_sampling.${key}`, value: value[key], locked,
          error: fieldErrors[path.join(".")],
          onEdit: (editPath, next) => { store.trackEdit(editPath, next); onDirty(); },
        };
        if (kind === "bool") return <BoolField key={key} {...shared} />;
        if (kind === "duration") return <DurationField key={key} {...shared} />;
        if (kind === "number") return <NumberField key={key} {...shared} />;
        if (kind === "multiselect") return <MultiSelectField key={key} {...shared} options={options ?? []} />;
        return <EnumField key={key} {...shared} options={options ?? []} />;
      })}
    </div>
  );
}

export default function WearSection({ wear, displays = {}, store, redactedPaths, onDirty, fieldErrors }: WearSectionProps) {
  const inv = wear ?? {};
  const keys = Object.keys(inv);
  if (keys.length === 0) return null;

  return (
    <FormSection id="wear" title="Wear">
      <div className="cf-card">
          {keys.filter((key) => key !== "active_sampling").map((key) => {
          const path = ["wear", key];
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
        {typeof inv.active_sampling === "object" && inv.active_sampling !== null && !Array.isArray(inv.active_sampling) && (
          <ActiveSamplingFields
            value={inv.active_sampling as unknown as Record<string, unknown>}
            displays={displays}
            store={store}
            redactedPaths={redactedPaths}
            onDirty={onDirty}
            fieldErrors={fieldErrors}
          />
        )}
      </FormSection>
  );
}
