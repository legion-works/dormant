/**
 * Zones section — one card per zone from inventory.zones.
 *
 * W1-5: per-entity collapse with localStorage, 230px label column,
 * changed-field markers.
 */
import { useState, useCallback } from "react";
import FormSection from "./FormSection";
import { EnumField, NumberField, MultiSelectField } from "./fields";
import type { FieldProps } from "./fields";
import type { PatchStore } from "./patch";
import type { ZoneConfig, RuleConfig } from "../../api/types";
import { FUSION_MODES, UNAVAILABLE_POLICIES } from "./fields";
import CreateEntityForm from "./CreateEntityForm";
import { referencingEntities } from "./entityCrud";
import { useConfirmDialog } from "../components";
import { readEntityExpanded, writeEntityExpanded, zoneSummary } from "./density";

interface ZonesSectionProps {
  zones: Record<string, ZoneConfig>;
  store: PatchStore;
  redactedPaths: string[][];
  onDirty: () => void;
  fieldErrors: Record<string, string | undefined>;
  entityCrudEnabled?: boolean;
  sensorIds?: string[];
  rules?: Record<string, RuleConfig>;
}

export default function ZonesSection({
  zones, store, redactedPaths, onDirty, fieldErrors,
  entityCrudEnabled = true, sensorIds = [], rules = {},
}: ZonesSectionProps) {
  const ids = Object.keys(zones);
  const [showCreate, setShowCreate] = useState(false);
  const { confirm, dialog } = useConfirmDialog();

  const [expanded, setExpanded] = useState<Record<string, boolean>>(() => {
    const out: Record<string, boolean> = {};
    for (const id of ids) out[id] = readEntityExpanded("zones", id);
    return out;
  });
  const toggleExpanded = useCallback((id: string) => {
    setExpanded((prev) => {
      const next = !prev[id];
      writeEntityExpanded("zones", id, next);
      return { ...prev, [id]: next };
    });
  }, []);

  if (ids.length === 0 && !entityCrudEnabled) return null;

  async function handleDelete(id: string) {
    const refs = referencingEntities("zones", id, { zones: {}, rules });
    const accepted = await confirm({
      title: `Delete zone "${id}"?`,
      description: refs.length > 0 ? `Referenced by ${refs.join(", ")}. Deleting it may make the pending config invalid.` : "Nothing else references zones.",
      confirmLabel: "Delete zone", tone: "danger",
    });
    if (accepted) { store.trackDelete("zones", id); onDirty(); }
  }

  return (
    <>
    <FormSection title="Zones">
      {ids.map((id) => {
        const cfg = zones[id];
        const basePath = ["zones", id];
        const open = expanded[id] !== false;

        const makeShared = (key: string, value: unknown, extra?: Partial<FieldProps>): FieldProps => ({
          path: [...basePath, key], label: key, value,
          locked: store.isLocked([...basePath, key], redactedPaths),
          error: fieldErrors[[...basePath, key].join(".")],
          onEdit: (p, v) => { store.trackEdit(p, v); onDirty(); },
          ...extra,
        });

        return (
          <div key={id} className="cf-card">
            <div className="cf-card__header">
              <button type="button" className="cf-section__toggle"
                onClick={() => toggleExpanded(id)} aria-expanded={open}
                style={{ minWidth: 0, gap: "4px" }}>
                <span className={`cf-section__chevron${open ? " cf-section__chevron--open" : ""}`}>{"▶"}</span>
              </button>
              <span className="cf-card__name">{id}</span>
              {!open && <span className="cf-card__summary-type">{zoneSummary(cfg)}</span>}
              {entityCrudEnabled && (
                <button type="button" className="cf-apply__btn cf-apply__btn--danger cf-card__delete"
                  onClick={() => handleDelete(id)}>Delete</button>
              )}
            </div>

            {open && (
            <div className="cf-card__fields">
              <div className="cf-field cf-field--row">
                <EnumField {...makeShared("mode", cfg.mode, { help: "How members combine into one presence result. any = present if any member is; all = only if every member is; quorum = at least N members; weighted = present members' weight fraction meets the threshold." })} options={FUSION_MODES} />
              </div>

              <div className="cf-field cf-field--row">
                <EnumField {...makeShared("unavailable_policy", cfg.unavailable_policy ?? "present", { help: "How an offline/stale sensor is treated. present (default) is fail-safe — never blanks a room it can't see. absent will blank when sensors drop out; use with care." })} options={UNAVAILABLE_POLICIES} />
              </div>

              {entityCrudEnabled ? (
                <div className="cf-field cf-field--row">
                  <MultiSelectField {...makeShared("members", cfg.members, { help: "Sensors that fuse into this zone's presence." })} options={sensorIds} />
                </div>
              ) : (
                <div className="cf-field cf-field--row cf-field--locked">
                  <label className="cf-field__label">members</label>
                  <div className="cf-field__value-list">
                    {cfg.members.map((m) => <span key={m} className="cf-field__value-chip">{m}</span>)}
                    <span className="cf-field__lock" title="array editors land in T8">{"🔒"}</span>
                  </div>
                </div>
              )}

              {cfg.quorum !== undefined && (
                <div className="cf-field cf-field--row">
                  <NumberField {...makeShared("quorum", cfg.quorum, { help: "Minimum number of members that must report present." })} />
                </div>
              )}
              {cfg.threshold !== undefined && (
                <div className="cf-field cf-field--row">
                  <NumberField {...makeShared("threshold", cfg.threshold, { help: "Present-weight fraction required, 0.0–1.0." })} />
                </div>
              )}

              {cfg.weights && Object.keys(cfg.weights).length > 0 && (
                <div className="cf-field cf-field--row cf-field--locked">
                  <label className="cf-field__label">weights</label>
                  <div className="cf-field__value-list">
                    {Object.entries(cfg.weights).map(([k, v]) => <span key={k} className="cf-field__value-chip">{k}: {v}</span>)}
                    <span className="cf-field__lock" title="array editors land in T8">{"🔒"}</span>
                  </div>
                </div>
              )}
            </div>
            )}
          </div>
        );
      })}

      {entityCrudEnabled && (
        showCreate ? (
          <CreateEntityForm collection="zones" existingIds={ids} sensorIds={sensorIds}
            onCreate={(id, value) => { store.trackCreate("zones", id, value); onDirty(); setShowCreate(false); }}
            onCancel={() => setShowCreate(false)} />
        ) : (
          <button type="button" className="cf-apply__btn cf-card__add" onClick={() => setShowCreate(true)}>+ Add zone</button>
        )
      )}
    </FormSection>
    {dialog}
    </>
  );
}
