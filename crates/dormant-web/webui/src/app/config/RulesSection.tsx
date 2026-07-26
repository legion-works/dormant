/**
 * Rules section — one card per rule from inventory.rules.
 *
 * W1-5: per-entity collapse with localStorage, 230px label column,
 * changed-field markers.
 */
import { useState, useCallback } from "react";
import FormSection from "./FormSection";
import { DurationField, NumberField, TextField, EnumField, MultiSelectField } from "./fields";
import type { FieldProps } from "./fields";
import type { PatchStore } from "./patch";
import type { RuleConfig } from "../../api/types";
import CreateEntityForm from "./CreateEntityForm";
import { VALID_INHIBITORS } from "./entityCrud";
import { useConfirmDialog } from "../components";
import { readEntityExpanded, writeEntityExpanded, ruleSummary } from "./density";

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
  entityCrudEnabled?: boolean;
  zoneIds?: string[];
  displayIds?: string[];
}

export default function RulesSection({
  rules, store, redactedPaths, onDirty, fieldErrors,
  entityCrudEnabled = true, zoneIds = [], displayIds = [],
}: RulesSectionProps) {
  const ids = Object.keys(rules);
  const [showCreate, setShowCreate] = useState(false);
  const { confirm, dialog } = useConfirmDialog();

  const [expanded, setExpanded] = useState<Record<string, boolean>>(() => {
    const out: Record<string, boolean> = {};
    for (const id of ids) out[id] = readEntityExpanded("rules", id);
    return out;
  });
  const toggleExpanded = useCallback((id: string) => {
    setExpanded((prev) => {
      const next = !prev[id];
      writeEntityExpanded("rules", id, next);
      return { ...prev, [id]: next };
    });
  }, []);

  if (ids.length === 0 && !entityCrudEnabled) return null;

  async function handleDelete(id: string) {
    const accepted = await confirm({
      title: `Delete rule "${id}"?`, description: "Nothing else references rules.",
      confirmLabel: "Delete rule", tone: "danger",
    });
    if (accepted) { store.trackDelete("rules", id); onDirty(); }
  }

  return (
    <>
    <FormSection title="Rules">
      {ids.map((id) => {
        const cfg = rules[id];
        const basePath = ["rules", id];
        const open = expanded[id] !== false;

        const makeShared = (key: string, value: unknown, extra?: Partial<FieldProps>): FieldProps => ({
          path: [...basePath, key], label: key, value,
          locked: store.isLocked([...basePath, key], redactedPaths),
          error: fieldErrors[[...basePath, key].join(".")],
          onEdit: (p, v) => { store.trackEdit(p, v); onDirty(); },
          ...extra,
        });

        function rowCls(key: string, value: unknown) {
          const pending = store.getEdit([...basePath, key]);
          const changed = pending !== undefined && pending !== value;
          return `cf-field cf-field--row${changed ? " cf-field--changed" : ""}`;
        }

        return (
          <div key={id} className="cf-card">
            <div className="cf-card__header">
              <button type="button" className="cf-section__toggle"
                onClick={() => toggleExpanded(id)} aria-expanded={open}
                style={{ minWidth: 0, gap: "4px" }}>
                <span className={`cf-section__chevron${open ? " cf-section__chevron--open" : ""}`}>{"▶"}</span>
              </button>
              <span className="cf-card__name">{id}</span>
              {!open && <span className="cf-card__summary-type">{ruleSummary(cfg)}</span>}
              {entityCrudEnabled && (
                <button type="button" className="cf-apply__btn cf-apply__btn--danger cf-card__delete"
                  onClick={() => handleDelete(id)}>Delete</button>
              )}
            </div>

            {open && (
            <div className="cf-card__fields">
              {entityCrudEnabled ? (
                <div className={rowCls("zone", cfg.zone)}>
                  <EnumField {...makeShared("zone", cfg.zone)} options={zoneIds} />
                </div>
              ) : (
                <div className="cf-field cf-field--row cf-field--locked">
                  <label className="cf-field__label">zone</label>
                  <div className="cf-field__value-row">
                    <span className="cf-field__value-text">{cfg.zone}</span>
                    <span className="cf-field__lock" title="not editable in v1" aria-label="not editable in v1">{"🔒"}</span>
                  </div>
                </div>
              )}

              {entityCrudEnabled ? (
                <div className={rowCls("displays", cfg.displays)}>
                  <MultiSelectField {...makeShared("displays", cfg.displays)} options={displayIds} />
                </div>
              ) : (
                <div className="cf-field cf-field--row cf-field--locked">
                  <label className="cf-field__label">displays</label>
                  <div className="cf-field__value-list">
                    {cfg.displays.map((d) => <span key={d} className="cf-field__value-chip">{d}</span>)}
                    <span className="cf-field__lock" title="not editable in v1" aria-label="not editable in v1">{"🔒"}</span>
                  </div>
                </div>
              )}

              {cfg.grace_period !== undefined && (
                <div className={rowCls("grace_period", cfg.grace_period)}>
                  <DurationField {...makeShared("grace_period", cfg.grace_period, { help: "Zone must stay present or absent this long before a rule acts (debounce).", placeholder: "60s" })} />
                  {store.getEdit([...basePath, "grace_period"]) !== undefined && store.getEdit([...basePath, "grace_period"]) !== cfg.grace_period && (
                    <span className="cf-field__was">changed · was {String(cfg.grace_period)}</span>
                  )}
                </div>
              )}

              {cfg.wake_retry_interval !== undefined && (
                <div className={rowCls("wake_retry_interval", cfg.wake_retry_interval)}>
                  <DurationField {...makeShared("wake_retry_interval", cfg.wake_retry_interval, { help: "Interval between successive wake retries after the initial backoff.", placeholder: "60s" })} />
                  {store.getEdit([...basePath, "wake_retry_interval"]) !== undefined && store.getEdit([...basePath, "wake_retry_interval"]) !== cfg.wake_retry_interval && (
                    <span className="cf-field__was">changed · was {String(cfg.wake_retry_interval)}</span>
                  )}
                </div>
              )}

              {cfg.wake_retries !== undefined && (
                <div className={rowCls("wake_retries", cfg.wake_retries)}>
                  <NumberField {...makeShared("wake_retries", cfg.wake_retries, { help: "Number of wake retries before escalating to the next controller or failing.", placeholder: "3" })} />
                  {store.getEdit([...basePath, "wake_retries"]) !== undefined && store.getEdit([...basePath, "wake_retries"]) !== cfg.wake_retries && (
                    <span className="cf-field__was">changed · was {String(cfg.wake_retries)}</span>
                  )}
                </div>
              )}

              {entityCrudEnabled ? (
                <div className={rowCls("inhibitors", cfg.inhibitors)}>
                  <MultiSelectField {...makeShared("inhibitors", cfg.inhibitors ?? [])} options={VALID_INHIBITORS} />
                </div>
              ) : (
                cfg.inhibitors && cfg.inhibitors.length > 0 && (
                  <div className="cf-field cf-field--row cf-field--locked">
                    <label className="cf-field__label">inhibitors</label>
                    <div className="cf-field__value-list">
                      {cfg.inhibitors.map((inhib) => <span key={inhib} className="cf-field__value-chip">{inhib}</span>)}
                      <span className="cf-field__lock" title="not editable in v1" aria-label="not editable in v1">{"🔒"}</span>
                    </div>
                  </div>
                )
              )}

              {Object.keys(cfg)
                .filter((k) => !["zone", "displays", "grace_period", "wake_retry_interval", "wake_retries", "inhibitors"].includes(k))
                .map((key) => {
                  const value = (cfg as unknown as Record<string, unknown>)[key];
                  const extra = RULE_HELP[key] ? { help: RULE_HELP[key] } : {};
                  const pending = store.getEdit([...basePath, key]);
                  const changed = pending !== undefined && pending !== value;
                  let widget: React.ReactNode;
                  if (typeof value === "number") widget = <NumberField key={key} {...makeShared(key, value, extra)} />;
                  else if (typeof value === "string") {
                    if (/_time$|_period$|_interval$|_backoff$/.test(key)) widget = <DurationField key={key} {...makeShared(key, value, extra)} />;
                    else widget = <TextField key={key} {...makeShared(key, value, extra)} />;
                  } else return null;
                  return (
                    <div key={key} className={`cf-field cf-field--row${changed ? " cf-field--changed" : ""}`}>
                      {widget}
                      {changed && <span className="cf-field__was">changed · was {String(value ?? "")}</span>}
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
          <CreateEntityForm collection="rules" existingIds={ids} zoneIds={zoneIds} displayIds={displayIds}
            onCreate={(id, value) => { store.trackCreate("rules", id, value); onDirty(); setShowCreate(false); }}
            onCancel={() => setShowCreate(false)} />
        ) : (
          <button type="button" className="cf-apply__btn cf-card__add" onClick={() => setShowCreate(true)}>+ Add rule</button>
        )
      )}
    </FormSection>
    {dialog}
    </>
  );
}
