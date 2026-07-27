/**
 * HooksInspector — read-only inventory of the five KVM hand-off hook slots
 * for a shared display.
 *
 * When `hookEditEnabled` is true (§BG-6), each slot renders an inline
 * array editor reusing the ScreensaverEditor pattern: per-action cards
 * with command/mqtt/timeout/blocking/abort_on_failure fields, plus
 * add/remove buttons.  When false (the default), hooks stay read-only and
 * the footer states they are edited in the file.
 */
import type { HookAction, HookSlots } from "../../api/types";
import FormSection from "./FormSection";
import { TextField, BoolField, DurationField } from "./fields";
import type { PatchStore } from "./patch";

export interface HooksInspectorProps {
  displayId: string;
  hooks: HookSlots | undefined;
  /** When true, show the edit affordance — gated on `daemon.hook_edit_enabled`. */
  hookEditEnabled?: boolean;
  store?: PatchStore;
  onDirty?: () => void;
  fieldErrors?: Record<string, string | undefined>;
}

interface SlotDef {
  key: keyof HookSlots;
  label: string;
  /** Default for `blocking` when absent from the TOML — per-slot fallback. */
  defaultBlocking: boolean;
}

const SLOTS: SlotDef[] = [
  { key: "before_release", label: "before_release", defaultBlocking: true },
  { key: "after_release", label: "after_release", defaultBlocking: false },
  { key: "before_acquire", label: "before_acquire", defaultBlocking: true },
  { key: "after_acquire", label: "after_acquire", defaultBlocking: false },
  { key: "on_observed_loss", label: "on_observed_loss", defaultBlocking: false },
];

/** Format a HookAction as a human-readable summary line. */
function formatAction(a: HookAction, slot: SlotDef): string {
  const parts: string[] = [];
  if (a.command && a.command.length > 0) {
    parts.push(a.command.join(" "));
  }
  if (a.mqtt) {
    parts.push(`MQTT ${a.mqtt.topic} \u2190 "${a.mqtt.payload}"`);
  }
  if (a.timeout) {
    parts.push(`timeout: ${a.timeout}`);
  }
  const blocking = a.blocking ?? slot.defaultBlocking;
  if (blocking) parts.push("blocking");
  if (a.abort_on_failure) parts.push("abort on failure");
  if (parts.length === 0) return "(empty)";
  return parts.join(" · ");
}

const DEFAULT_ACTION: HookAction = {};

export default function HooksInspector({ hooks, displayId, hookEditEnabled, store, onDirty, fieldErrors }: HooksInspectorProps) {
  const editing = Boolean(hookEditEnabled && store && onDirty);

  /** Emit a whole slot's action array as a single Set patch. */
  function emitSlot(slotKey: keyof HookSlots, actions: HookAction[]) {
    if (!store || !onDirty) return;
    const path = ["displays", displayId, "hooks", slotKey];
    store.trackEdit(path, actions.length === 0 ? [] : actions);
    onDirty();
  }

  return (
    <FormSection title={`Hooks — ${displayId}`}>
      <div className="cf-card">
        <div className="cf-card__header">
          <span className="cf-card__name">hooks</span>
          <span className="cf-card__type">per {displayId}</span>
        </div>

        <div className="cf-card__fields">
          {SLOTS.map((slot) => {
            const actions: HookAction[] = hooks?.[slot.key] ?? [];
            const empty = actions.length === 0;

            return (
              <div key={slot.key} className="cf-field">
                <label className="cf-field__label" style={{ color: empty ? "var(--text-faint)" : undefined }}>
                  {slot.label}
                </label>

                {empty && !editing ? (
                  <span className="cf-field__hint" style={{ fontStyle: "italic" }}>
                    — none
                  </span>
                ) : (
                  <div style={{ display: "flex", flexDirection: "column", gap: "4px" }}>
                    {actions.map((a, i) => {
                      if (editing) {
                        const actPath = ["displays", displayId, "hooks", slot.key, String(i)];
                        return (
                          <div key={i} className="hooks-editor__action" style={{
                            background: "var(--bg-sunken)",
                            borderRadius: "var(--radius-sm)",
                            padding: "8px 10px",
                            marginBottom: "6px",
                            border: "1px solid var(--border)",
                          }}>
                            <div style={{
                              display: "flex",
                              alignItems: "center",
                              justifyContent: "space-between",
                              marginBottom: "6px",
                            }}>
                              <span style={{ fontFamily: "var(--font-mono)", fontSize: "10px", color: "var(--text-muted)", textTransform: "uppercase", letterSpacing: "var(--tracking-caps)" }}>
                                Action {i + 1}
                              </span>
                              <button
                                type="button"
                                className="cf-apply__btn cf-apply__btn--discard"
                                style={{ padding: "2px 8px", fontSize: "10px" }}
                                onClick={() => {
                                  const next = actions.filter((_, j) => j !== i);
                                  emitSlot(slot.key, next);
                                }}
                                aria-label="Remove action"
                                title="Remove action"
                              >
                                ✕
                              </button>
                            </div>
                            <div style={{ display: "flex", flexDirection: "column", gap: "6px" }}>
                              <TextField
                                path={[...actPath, "command"]}
                                label="command"
                                value={(a.command ?? []).join(" ")}
                                locked={false}
                                onEdit={(_p, v) => {
                                  const next = [...actions];
                                  next[i] = { ...next[i], command: String(v).split(/\s+/) };
                                  emitSlot(slot.key, next);
                                }}
                                error={fieldErrors?.[[...actPath, "command"].join(".")]}
                                placeholder="argv words"
                              help="Shell command and arguments, space-separated."
                            />
                            <TextField
                              path={[...actPath, "mqtt", "topic"]}
                              label="mqtt topic"
                              value={a.mqtt?.topic ?? ""}
                              locked={false}
                              onEdit={(_p, v) => {
                                const next = [...actions];
                                const topic = String(v);
                                next[i] = { ...next[i], mqtt: topic ? { topic, payload: a.mqtt?.payload ?? "" } : undefined };
                                emitSlot(slot.key, next);
                              }}
                              error={fieldErrors?.[[...actPath, "mqtt", "topic"].join(".")]}
                              placeholder="dormant/status"
                              help="MQTT topic to publish.  Leave empty to use command instead."
                            />
                            <TextField
                              path={[...actPath, "mqtt", "payload"]}
                              label="mqtt payload"
                              value={a.mqtt?.payload ?? ""}
                              locked={false}
                              onEdit={(_p, v) => {
                                const next = [...actions];
                                const payload = String(v);
                                if (a.mqtt) {
                                  next[i] = { ...next[i], mqtt: { ...a.mqtt, payload } };
                                }
                                emitSlot(slot.key, next);
                              }}
                              error={fieldErrors?.[[...actPath, "mqtt", "payload"].join(".")]}
                              placeholder='{"status":"releasing"}'
                              help="Payload sent to the MQTT topic."
                            />
                            <BoolField
                                path={[...actPath, "blocking"]}
                                label="blocking"
                                value={a.blocking ?? slot.defaultBlocking}
                                locked={false}
                                onEdit={(_p, v) => {
                                  const next = [...actions];
                                  next[i] = { ...next[i], blocking: v as boolean };
                                  emitSlot(slot.key, next);
                                }}
                                help={`Wait for hook completion before proceeding. Default for ${slot.label}: ${slot.defaultBlocking}.`}
                              />
                              <BoolField
                                path={[...actPath, "abort_on_failure"]}
                                label="abort_on_failure"
                                value={a.abort_on_failure ?? false}
                                locked={false}
                                onEdit={(_p, v) => {
                                  const next = [...actions];
                                  next[i] = { ...next[i], abort_on_failure: v as boolean };
                                  emitSlot(slot.key, next);
                                }}
                                help="Cancel the switch when this hook fails."
                              />
                              <DurationField
                                path={[...actPath, "timeout"]}
                                label="timeout"
                                value={a.timeout ?? ""}
                                locked={false}
                                onEdit={(_p, v) => {
                                  const next = [...actions];
                                  next[i] = { ...next[i], timeout: v as string };
                                  emitSlot(slot.key, next);
                                }}
                                error={fieldErrors?.[[...actPath, "timeout"].join(".")]}
                                help="Max runtime. Left empty to use the daemon default."
                              />
                            </div>
                          </div>
                        );
                      }

                      // Read-only rendering
                      return (
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
                      );
                    })}

                    {/* Add action button — only when editing */}
                    {editing && (
                      <button
                        type="button"
                        className="cf-apply__btn"
                        style={{ marginTop: "4px" }}
                        onClick={() => {
                          const next = [...actions, { ...DEFAULT_ACTION }];
                          emitSlot(slot.key, next);
                        }}
                        aria-label={`Add ${slot.label} action`}
                      >
                        + Add action
                      </button>
                    )}
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
          {editing
            ? "Hook commands run with the daemon's privileges — only enable editing when you control the loopback surface."
            : "Hooks are edited in the config file — the web surface does not write shell commands."}
        </div>
      </div>
    </FormSection>
  );
}
