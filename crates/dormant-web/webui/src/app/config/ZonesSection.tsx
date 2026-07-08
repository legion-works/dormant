/**
 * Zones section — one card per zone from inventory.zones.
 *
 * Scalar fields: mode (enum), unavailable_policy (enum), quorum/threshold (number).
 * members / weights are read-only in T7 (array editors are T8).
 */
import FormSection from "./FormSection";
import { EnumField, NumberField } from "./fields";
import type { FieldProps } from "./fields";
import type { PatchStore } from "./patch";
import type { ZoneConfig } from "../../api/types";
import { FUSION_MODES, UNAVAILABLE_POLICIES } from "./fields";

interface ZonesSectionProps {
  zones: Record<string, ZoneConfig>;
  store: PatchStore;
  redactedPaths: string[][];
  onDirty: () => void;
  fieldErrors: Record<string, string | undefined>;
}

export default function ZonesSection({ zones, store, redactedPaths, onDirty, fieldErrors }: ZonesSectionProps) {
  const ids = Object.keys(zones);
  if (ids.length === 0) return null;

  return (
    <FormSection title="Zones">
      {ids.map((id) => {
        const cfg = zones[id];
        const basePath = ["zones", id];

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
              <EnumField {...makeShared("mode", cfg.mode, { help: "How members combine into one presence result. any = present if any member is; all = only if every member is; quorum = at least N members; weighted = present members' weight fraction meets the threshold." })} options={FUSION_MODES} />

              <EnumField {...makeShared("unavailable_policy", cfg.unavailable_policy, { help: "How an offline/stale sensor is treated. present (default) is fail-safe — never blanks a room it can't see. absent will blank when sensors drop out; use with care." })} options={UNAVAILABLE_POLICIES} />

              {/* Members — read-only in T7 */}
              <div className="cf-field cf-field--locked">
                <label className="cf-field__label">members</label>
                <div className="cf-field__value-list">
                  {cfg.members.map((m) => (
                    <span key={m} className="cf-field__value-chip">{m}</span>
                  ))}
                  <span className="cf-field__lock" title="array editors land in T8" aria-label="not editable in v1">{"🔒"}</span>
                </div>
              </div>

              {cfg.quorum !== undefined && (
                <NumberField {...makeShared("quorum", cfg.quorum, { help: "Minimum number of members that must report present." })} />
              )}
              {cfg.threshold !== undefined && (
                <NumberField {...makeShared("threshold", cfg.threshold, { help: "Present-weight fraction required, 0.0–1.0." })} />
              )}

              {/* Weights — read-only in T7 */}
              {Object.keys(cfg.weights).length > 0 && (
                <div className="cf-field cf-field--locked">
                  <label className="cf-field__label">weights</label>
                  <div className="cf-field__value-list">
                    {Object.entries(cfg.weights).map(([k, v]) => (
                      <span key={k} className="cf-field__value-chip">{k}: {v}</span>
                    ))}
                    <span className="cf-field__lock" title="array editors land in T8" aria-label="not editable in v1">{"🔒"}</span>
                  </div>
                </div>
              )}
            </div>
          </div>
        );
      })}
    </FormSection>
  );
}
