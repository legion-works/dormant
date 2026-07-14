/**
 * Ladder editor — stage rows with kind dropdown, optional dwell,
 * remove button, and add/reorder controls.
 *
 * Every mutation emits a whole-array set on the ladder path.
 * The terminal stage (last in array) shows "(terminal — no dwell)"
 * when its dwell is absent — the server treats no-dwell as an
 * immediate power-off/sleep signal.
 *
 * The working array is `store.getEdit(ladderPath) ?? fetchedStages`,
 * so sequential edits on different fields of the same stage
 * accumulate instead of overwriting each other.
 */
import { DurationField, EnumField } from "./fields";
import { STAGE_KINDS } from "../../api/types";
import type { LadderStage } from "../../api/types";
import type { PatchStore } from "./patch";

interface LadderEditorProps {
  stages: LadderStage[];
  displayId: string;
  store: PatchStore;
  redactedPaths: string[][];
  onDirty: () => void;
  fieldErrors: Record<string, string | undefined>;
}

const LADDER_PATH = (id: string): string[] => ["displays", id, "ladder"];

/** Default stage appended when "Add Stage" is clicked. */
const DEFAULT_STAGE: LadderStage = { kind: "render_black", dwell: "30s" };

/**
 * Read the effective ladder array: pending state wins over fetched prop.
 * Sequential edits on different fields of the same stage accumulate
 * because every mutation rebuilds from the EFFECTIVE array, not the
 * fetched prop.
 */
function getEffectiveStages(displayId: string, fetched: LadderStage[], store: PatchStore): LadderStage[] {
  const pending = store.getEdit(LADDER_PATH(displayId));
  return (pending as LadderStage[] | undefined) ?? fetched;
}

/**
 * Whether a value represents "no input" — an empty/whitespace string
 * from a cleared input field, or an explicit `null`.  The server omits
 * absent `Option` fields entirely rather than serialising them as JSON
 * `null` (see `skip_serializing_if` on `LadderStage::dwell` in
 * `dormant-core/src/types.rs`), so `null` here is a defensive-compat
 * case: older daemons or manually-crafted patch objects may still send it.
 */
function isAbsentInput(v: unknown): boolean {
  return v === null || (typeof v === "string" && v.trim() === "");
}

/**
 * Strip absent values from optional fields.  The server (TOML) rejects
 * null and empty strings for optional fields like dwell.
 */
function cleanStage(s: LadderStage): LadderStage {
  if (isAbsentInput(s.dwell)) {
    const { dwell: _, ...rest } = s;
    return rest as LadderStage;
  }
  return s;
}

/** Clone stages, strip null optional fields, and emit the new array. */
function emitStages(id: string, stages: LadderStage[], store: PatchStore, onDirty: () => void) {
  store.trackEdit(LADDER_PATH(id), stages.map(cleanStage));
  onDirty();
}

export default function LadderEditor({ stages: fetchedStages, displayId, store, redactedPaths, onDirty, fieldErrors }: LadderEditorProps) {
  const ladderPath = LADDER_PATH(displayId);
  const ladderLocked = store.isLocked(ladderPath, redactedPaths);

  // Working array: pending overlay wins over fetched prop.
  const stages = getEffectiveStages(displayId, fetchedStages, store);

  const isTerminal = (idx: number) => idx === stages.length - 1;

  return (
    <div className="cf-card" style={{ borderStyle: "dashed" }}>
      <div className="cf-card__header">
        <span className="cf-card__name">Escalation ladder</span>
        {ladderLocked && (
          <span className="cf-field__lock" title="locked — redacted path ancestor" aria-label="locked">{"🔒"}</span>
        )}
      </div>

      {stages.map((stage, idx) => {
        const stagePath = [...ladderPath, String(idx)];
        const kindPath = [...stagePath, "kind"];
        const dwellPath = [...stagePath, "dwell"];
        const terminal = isTerminal(idx);

        return (
          <div key={idx} style={{ display: "flex", alignItems: "center", gap: "8px", marginBottom: "8px" }}>
            <span style={{
              fontFamily: "var(--font-mono)", fontSize: "var(--text-2xs)",
              color: "var(--text-faint)", minWidth: "20px",
            }}>
              {idx + 1}
            </span>

            {/* Kind dropdown */}
            <div style={{ flex: 1 }}>
              <EnumField
                path={kindPath}
                label={`kind`}
                value={stage.kind}
                locked={ladderLocked || store.isLocked(kindPath, redactedPaths)}
                onEdit={(_p, v) => {
                  const effective = getEffectiveStages(displayId, fetchedStages, store);
                  const next = [...effective];
                  next[idx] = { ...next[idx], kind: v as LadderStage["kind"] };
                  emitStages(displayId, next, store, onDirty);
                }}
                options={STAGE_KINDS}
                error={fieldErrors[kindPath.join(".")]}
              />
            </div>

            {/* Dwell — optional, terminal stage shows marker when absent */}
            <div style={{ flex: 1 }}>
              {terminal && stage.dwell == null ? (
                <div className="cf-field">
                  <label className="cf-field__label">dwell</label>
                  <span className="cf-field__value-text" style={{ borderStyle: "dashed", opacity: 0.6 }}>
                    (terminal — no dwell)
                  </span>
                </div>
              ) : (
                <DurationField
                  path={dwellPath}
                  label="dwell"
                  value={stage.dwell ?? ""}
                  locked={ladderLocked || store.isLocked(dwellPath, redactedPaths)}
                  onEdit={(_p, v) => {
                    const effective = getEffectiveStages(displayId, fetchedStages, store);
                    const next = [...effective];
                    next[idx] = { ...next[idx], dwell: v as string };
                    emitStages(displayId, next, store, onDirty);
                  }}
                  error={fieldErrors[dwellPath.join(".")]}
                />
              )}
            </div>

            {/* Reorder + remove buttons */}
            {!ladderLocked && (
              <div style={{ display: "flex", gap: "4px", flexShrink: 0 }}>
                <button
                  type="button"
                  className="cf-apply__btn"
                  style={{ padding: "4px 8px", fontSize: "10px" }}
                  disabled={idx === 0}
                  onClick={() => {
                    const effective = getEffectiveStages(displayId, fetchedStages, store);
                    const next = [...effective];
                    [next[idx - 1], next[idx]] = [next[idx], next[idx - 1]];
                    emitStages(displayId, next, store, onDirty);
                  }}
                  aria-label="Move stage up"
                  title="Move up"
                >
                  ↑
                </button>
                <button
                  type="button"
                  className="cf-apply__btn"
                  style={{ padding: "4px 8px", fontSize: "10px" }}
                  disabled={idx === stages.length - 1}
                  onClick={() => {
                    const effective = getEffectiveStages(displayId, fetchedStages, store);
                    const next = [...effective];
                    [next[idx], next[idx + 1]] = [next[idx + 1], next[idx]];
                    emitStages(displayId, next, store, onDirty);
                  }}
                  aria-label="Move stage down"
                  title="Move down"
                >
                  ↓
                </button>
                <button
                  type="button"
                  className="cf-apply__btn cf-apply__btn--discard"
                  style={{ padding: "4px 8px", fontSize: "10px" }}
                  disabled={stages.length <= 1}
                  onClick={() => {
                    const effective = getEffectiveStages(displayId, fetchedStages, store);
                    const next = effective.filter((_, i) => i !== idx);
                    emitStages(displayId, next, store, onDirty);
                  }}
                  aria-label="Remove stage"
                  title="Remove"
                >
                  ✕
                </button>
              </div>
            )}
          </div>
        );
      })}

      {/* Add stage */}
      {!ladderLocked && (
        <button
          type="button"
          className="cf-apply__btn"
          style={{ marginTop: "6px" }}
          onClick={() => {
            const effective = getEffectiveStages(displayId, fetchedStages, store);
            const next = [...effective, { ...DEFAULT_STAGE }];
            emitStages(displayId, next, store, onDirty);
          }}
          aria-label="Add stage"
        >
          + Add stage
        </button>
      )}
    </div>
  );
}
