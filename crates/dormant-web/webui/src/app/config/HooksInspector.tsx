/**
 * Hooks inspector — read-only view of per-display hook slots from
 * `config.inventory.displays[id].hooks`.
 *
 * Renders five slots always (before_release, after_release,
 * before_acquire, after_acquire, on_observed_loss), showing
 * command argv, MQTT actions, timeout, blocking status (with
 * per-slot default applied when absent), and abort_on_failure.
 * Blocking `before_*` actions carry an abort-gate tag.
 * Empty slots render `— none`.
 *
 * No write path — editing requires the config file.
 */
import type { HookAction, HookSlots } from "../../api/types";
import FormSection from "./FormSection";

interface HooksInspectorProps {
  /** Per-display hooks from inventory. */
  hooks?: HookSlots;
  displayId: string;
}

interface SlotDef {
  key: keyof HookSlots;
  label: string;
  /** Default `blocking` value when the key is absent — differs per slot. */
  defaultBlocking: boolean;
}

const SLOTS: SlotDef[] = [
  { key: "before_release", label: "before_release", defaultBlocking: true },
  { key: "after_release", label: "after_release", defaultBlocking: false },
  { key: "before_acquire", label: "before_acquire", defaultBlocking: true },
  { key: "after_acquire", label: "after_acquire", defaultBlocking: false },
  { key: "on_observed_loss", label: "on_observed_loss", defaultBlocking: false },
];

/** Format a single HookAction for display. */
function formatAction(a: HookAction, slot: SlotDef): string {
  const parts: string[] = [];
  if (a.command && a.command.length > 0) {
    parts.push(a.command.join(" "));
  }
  if (a.mqtt) {
    parts.push(`MQTT ${a.mqtt.topic} ← "${a.mqtt.payload}"`);
  }
  if (a.timeout) {
    parts.push(`timeout: ${a.timeout}`);
  }
  const blocking = a.blocking ?? slot.defaultBlocking;
  parts.push(blocking ? "blocking" : "non-blocking");
  if (a.abort_on_failure) {
    parts.push("abort on failure");
  }
  return parts.join(" · ");
}

export default function HooksInspector({ hooks, displayId }: HooksInspectorProps) {
  return (
    <FormSection title={`Hooks — ${displayId}`}>
      <div className="cf-card">
        <div className="cf-card__header">
          <span className="cf-card__name">hooks</span>
          <span className="cf-card__type">per {displayId}</span>
        </div>

        <div className="cf-card__fields">
          {SLOTS.map((slot) => {
            const actions = hooks?.[slot.key];
            const empty = !actions || actions.length === 0;

            return (
              <div key={slot.key} className="cf-field">
                <label className="cf-field__label" style={{ color: empty ? "var(--text-faint)" : undefined }}>
                  {slot.label}
                </label>

                {empty ? (
                  <span className="cf-field__hint" style={{ fontStyle: "italic" }}>
                    — none
                  </span>
                ) : (
                  <div style={{ display: "flex", flexDirection: "column", gap: "4px" }}>
                    {actions.map((a, i) => (
                      <div key={i} style={{ display: "flex", alignItems: "center", gap: "8px" }}>
                        {/* Abort-gate tag on blocking before_* actions */}
                        {(slot.defaultBlocking || a.blocking) &&
                         slot.key.startsWith("before_") && (
                          <span
                            className="cf-field__value-chip"
                            style={{
                              color: "var(--warning)",
                              borderColor: "color-mix(in oklab, var(--warning) 35%, transparent)",
                              flexShrink: 0,
                            }}
                          >
                            abort-gate
                          </span>
                        )}
                        <code
                          style={{
                            fontFamily: "var(--font-mono)",
                            fontSize: "var(--text-2xs)",
                            color: "var(--text-body)",
                            background: "var(--bg-sunken)",
                            padding: "4px 8px",
                            borderRadius: "var(--radius-sm)",
                            wordBreak: "break-all",
                          }}
                        >
                          {formatAction(a, slot)}
                        </code>
                      </div>
                    ))}
                  </div>
                )}
              </div>
            );
          })}
        </div>

        <div
          className="cf-field__hint"
          style={{ marginTop: "12px", paddingTop: "10px", borderTop: "1px solid var(--border)" }}
        >
          Hooks are edited in the config file — the web surface does not write shell commands.
        </div>
      </div>
    </FormSection>
  );
}
