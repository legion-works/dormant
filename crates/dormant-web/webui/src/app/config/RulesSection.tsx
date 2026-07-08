/**
 * Rules section — one card per rule from inventory.rules.
 *
 * Scalar fields: zone (read-only string), displays (read-only in T7),
 * grace_period, wake_retry_interval (durations), wake_retries (number),
 * inhibitors (read-only in T7).
 */
import FormSection from "./FormSection";
import { DurationField, NumberField, TextField } from "./fields";
import type { FieldProps } from "./fields";
import type { PatchStore } from "./patch";
import type { RuleConfig } from "../../api/types";

/** Per-key help for rule scalar fields — accurate to the real config semantics. */
const RULE_HELP: Record<string, string> = {
  min_blank_time: "Minimum time a display must stay blanked before it can be woken.",
  min_wake_time: "Minimum time a display must stay awake before it can be blanked again.",
  activity_idle_threshold: "No keyboard/mouse events for this long means the user is inactive.",
  activity_poll_interval: "How often to poll user-activity state while an activity inhibitor is active.",
  wake_retry_backoff: "Backoff between the immediate wake attempt and the first retry.",
};

interface RulesSectionProps {
  rules: Record<string, RuleConfig>;
  store: PatchStore;
  redactedPaths: string[][];
  onDirty: () => void;
  fieldErrors: Record<string, string | undefined>;
}

export default function RulesSection({ rules, store, redactedPaths, onDirty, fieldErrors }: RulesSectionProps) {
  const ids = Object.keys(rules);
  if (ids.length === 0) return null;

  return (
    <FormSection title="Rules">
      {ids.map((id) => {
        const cfg = rules[id];
        const basePath = ["rules", id];

        const makeShared = (key: string, value: unknown, extra?: Partial<FieldProps>): FieldProps => ({
          path: [...basePath, key],
          label: key,
          value,
          locked: store.isLocked([...basePath, key], redactedPaths),
          lockedReason: undefined,
          error: fieldErrors[[...basePath, key].join(".")],
          onEdit: (p, v) => {
            store.trackEdit(p, v);
            onDirty();
          },
          ...extra,
        });

        return (
          <div key={id} className="cf-card">
            <div className="cf-card__header">
              <span className="cf-card__name">{id}</span>
            </div>

            <div className="cf-card__fields">
              {/* zone — display name linking to which zone this rule governs */}
              <div className="cf-field cf-field--locked">
                <label className="cf-field__label">zone</label>
                <div className="cf-field__value-row">
                  <span className="cf-field__value-text">{cfg.zone}</span>
                  <span className="cf-field__lock" title="not editable in v1" aria-label="not editable in v1">{"🔒"}</span>
                </div>
              </div>

              {/* displays — read-only in T7 */}
              <div className="cf-field cf-field--locked">
                <label className="cf-field__label">displays</label>
                <div className="cf-field__value-list">
                  {cfg.displays.map((d) => (
                    <span key={d} className="cf-field__value-chip">{d}</span>
                  ))}
                  <span className="cf-field__lock" title="not editable in v1" aria-label="not editable in v1">{"🔒"}</span>
                </div>
              </div>

              {/* grace_period — duration */}
              {cfg.grace_period !== undefined && (
                <DurationField {...makeShared("grace_period", cfg.grace_period, { help: "Zone must stay present or absent this long before a rule acts (debounce).", placeholder: "60s" })} />
              )}

              {/* wake_retry_interval — duration */}
              {cfg.wake_retry_interval !== undefined && (
                <DurationField {...makeShared("wake_retry_interval", cfg.wake_retry_interval, { help: "Interval between successive wake retries after the initial backoff.", placeholder: "60s" })} />
              )}

              {/* wake_retries — number */}
              {cfg.wake_retries !== undefined && (
                <NumberField {...makeShared("wake_retries", cfg.wake_retries, { help: "Number of wake retries before escalating to the next controller or failing.", placeholder: "3" })} />
              )}

              {/* inhibitors — read-only in T7 */}
              {cfg.inhibitors && cfg.inhibitors.length > 0 && (
                <div className="cf-field cf-field--locked">
                  <label className="cf-field__label">inhibitors</label>
                  <div className="cf-field__value-list">
                    {cfg.inhibitors.map((inhibitor) => (
                      <span key={inhibitor} className="cf-field__value-chip">{inhibitor}</span>
                    ))}
                    <span className="cf-field__lock" title="not editable in v1" aria-label="not editable in v1">{"🔒"}</span>
                  </div>
                </div>
              )}

              {/* Render any remaining scalar keys not handled above */}
              {Object.keys(cfg)
                .filter((k) => !["zone", "displays", "grace_period", "wake_retry_interval", "wake_retries", "inhibitors"].includes(k))
                .map((key) => {
                  const value = (cfg as unknown as Record<string, unknown>)[key];
                  const extra = RULE_HELP[key] ? { help: RULE_HELP[key] } : {};
                  if (typeof value === "number") {
                    return <NumberField key={key} {...makeShared(key, value, extra)} />;
                  }
                  if (typeof value === "string") {
                    // Heuristic: keys ending in _time, _period, _interval, _backoff are durations
                    if (/_time$|_period$|_interval$|_backoff$/.test(key)) {
                      return <DurationField key={key} {...makeShared(key, value, extra)} />;
                    }
                    return <TextField key={key} {...makeShared(key, value, extra)} />;
                  }
                  return null;
                })}
            </div>
          </div>
        );
      })}
    </FormSection>
  );
}
