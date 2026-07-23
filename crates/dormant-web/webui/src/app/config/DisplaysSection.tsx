/**
 * Display editors section — one card per display.
 *
 * Renders the mode toggle (simple blank vs escalation ladder) and
 * the corresponding editor for each display.  Displays with no
 * configured mode get a read-only warning card.
 *
 * When a mode switch is queued (pending in the store), the toggle
 * reflects the QUEUED mode (active button + "applies on Apply" hint)
 * and the editor renders the queued mode's editor operating on the
 * pending starter array.
 */
import FormSection from "./FormSection";
import LadderEditor from "./LadderEditor";
import ScreensaverEditor from "./ScreensaverEditor";
import { EnumField, NumberField, PANEL_TYPES } from "./fields";
import { useState, useEffect } from "react";
import type { DisplayConfig, LadderStage, RuleConfig } from "../../api/types";
import type { PatchStore } from "./patch";
import CreateEntityForm from "./CreateEntityForm";
import { referencingEntities } from "./entityCrud";
import { useConfirmDialog } from "../components";

const BLANK_MODE_OPTIONS = ["power_off", "screen_off_audio_on", "brightness_zero"] as const;

/**
 * The ladder starter used when switching TO ladder and the display
 * had no previous ladder in the fetched config.
 */
const STARTER_LADDER: LadderStage[] = [
  { kind: "render_black", dwell: "30s" },
  { kind: "power_off" },
];

/** Which mode the display is effectively in, considering pending store state. */
type EffectiveMode = { kind: "blank" } | { kind: "ladder" };

function getEffectiveMode(id: string, cfg: DisplayConfig, store: PatchStore): EffectiveMode {
  const pendingLadder = store.getEdit(["displays", id, "ladder"]);
  const pendingBlank = store.getEdit(["displays", id, "blank_mode"]);

  // If ladder is pending (set), queued mode is ladder
  if (pendingLadder !== undefined) return { kind: "ladder" };
  // If blank_mode is pending (set), queued mode is blank
  if (pendingBlank !== undefined) return { kind: "blank" };

  // Fall back to fetched config
  if (cfg.ladder && cfg.ladder.length > 0) return { kind: "ladder" };
  return { kind: "blank" };
}

interface DisplaysSectionProps {
  displays: Record<string, DisplayConfig>;
  store: PatchStore;
  redactedPaths: string[][];
  onDirty: () => void;
  fieldErrors: Record<string, string | undefined>;
  /** Whether entity create/delete is enabled (`daemon.entity_crud_enabled`, spec §2/§10). Defaults to true for pre-feature callers. */
  entityCrudEnabled?: boolean;
  /** Live rules inventory — used to compute the delete-confirm references warning (spec §7). */
  rules?: Record<string, RuleConfig>;
  /**
   * Pre-fill for the create form from the pairing wizard's post-pair
   * "create display?" hand-off (spec §8.3: `{host, controllers:
   * ["samsung-tizen"]}`). Presence auto-opens the create form.
   */
  createPrefill?: Record<string, unknown> | null;
}

export default function DisplaysSection({
  displays,
  store,
  redactedPaths,
  onDirty,
  fieldErrors,
  entityCrudEnabled = true,
  rules = {},
  createPrefill = null,
}: DisplaysSectionProps) {
  const ids = Object.keys(displays);
  // Re-render tick: incremented on mode switch so the pending indicator
  // and effective mode update immediately even without a parent re-render.
  const [, rerender] = useState(0);
  const [showCreate, setShowCreate] = useState(createPrefill != null);

  useEffect(() => {
    if (createPrefill) setShowCreate(true);
  }, [createPrefill]);

  const { confirm, dialog } = useConfirmDialog();

  if (ids.length === 0 && !entityCrudEnabled) return null;

  async function handleDelete(id: string) {
    const refs = referencingEntities("displays", id, { zones: {}, rules });
    const accepted = await confirm({
      title: `Delete display "${id}"?`,
      description: refs.length > 0
        ? `Referenced by ${refs.join(", ")}. Deleting it may make the pending config invalid.`
        : "Nothing else references displays.",
      confirmLabel: "Delete display",
      tone: "danger",
    });
    if (accepted) {
      store.trackDelete("displays", id);
      onDirty();
    }
  }

  return (
    <>
    <FormSection title="Displays">
      {ids.map((id) => {
        const cfg = displays[id];
              const basePath = ["displays", id];
              const scope = (store.getEdit([...basePath, "scope"]) ?? cfg.scope ?? "private") as "private" | "shared";

        // Fetched state
        const fetchedLadder = cfg.ladder && cfg.ladder.length > 0;
        const fetchedBlank = cfg.blank_mode !== undefined;
        const hasNeither = !fetchedLadder && !fetchedBlank;

        // Effective mode (pending overlay)
        const effective = getEffectiveMode(id, cfg, store);
        const isLadder = effective.kind === "ladder";
        const isBlank = effective.kind === "blank";

        // Is a mode switch queued? (effective ≠ fetched)
        const modeSwitchPending =
          (isLadder && !fetchedLadder) || (isBlank && fetchedLadder) ||
          (hasNeither && (store.getEdit(["displays", id, "ladder"]) !== undefined || store.getEdit(["displays", id, "blank_mode"]) !== undefined));

        return (
          <div key={id} className="cf-card">
            <div className="cf-card__header">
              <span className="cf-card__name">{id}</span>
              {entityCrudEnabled && (
                <button
                  type="button"
                  className="cf-apply__btn cf-apply__btn--danger cf-card__delete"
                  onClick={() => handleDelete(id)}
                >
                  Delete
                </button>
              )}
            </div>

            {/* ── No-mode warning ── */}
            {hasNeither && !modeSwitchPending && (
              <p className="cf-placeholder">
                This display has neither blank_mode nor a ladder — fix the config file.
              </p>
            )}

                  <div className="cf-card__fields">
                    <EnumField
                      path={[...basePath, "scope"]}
                      label="scope"
                      value={scope}
                      locked={store.isLocked([...basePath, "scope"], redactedPaths)}
                      onEdit={(p, v) => { store.trackEdit(p, v); onDirty(); }}
                      options={["private", "shared"]}
                      error={fieldErrors[[...basePath, "scope"].join(".")]}
                    />
                    {scope === "shared" && (
                      <NumberField
                        path={[...basePath, "shared_input_code"]}
                        label="shared_input_code"
                        value={cfg.shared_input_code ?? ""}
                        locked={store.isLocked([...basePath, "shared_input_code"], redactedPaths)}
                        onEdit={(p, v) => { store.trackEdit(p, v); onDirty(); }}
                        error={fieldErrors[[...basePath, "shared_input_code"].join(".")]}
                      />
                    )}
                    {/* ── Mode toggle ── */}
              <div className="cf-field">
                <label className="cf-field__label">Mode</label>
                <div style={{ display: "flex", gap: "8px", alignItems: "center" }}>
                  <button
                    type="button"
                    className={`cf-apply__btn${isBlank ? " cf-apply__btn--apply" : ""}`}
                    onClick={() => { switchToBlank(id, cfg, store, onDirty); rerender((n) => n + 1); }}
                    aria-label="Simple blank mode"
                    aria-pressed={isBlank}
                  >
                    Simple blank
                    {isBlank && modeSwitchPending && (
                      <span style={{ fontSize: "10px", color: "var(--text-faint)", marginLeft: "4px" }}>
                        — applies on Apply
                      </span>
                    )}
                  </button>
                  <button
                    type="button"
                    className={`cf-apply__btn${isLadder ? " cf-apply__btn--apply" : ""}`}
                    onClick={() => { switchToLadder(id, cfg, store, onDirty); rerender((n) => n + 1); }}
                    aria-label="Escalation ladder"
                    aria-pressed={isLadder}
                  >
                    Escalation ladder
                    {isLadder && modeSwitchPending && (
                      <span style={{ fontSize: "10px", color: "var(--text-faint)", marginLeft: "4px" }}>
                        — applies on Apply
                      </span>
                    )}
                  </button>
                </div>
              </div>

              {/* ── Panel technology (independent of blank/ladder mode) ── */}
              <EnumField
                path={[...basePath, "panel_type"]}
                label="panel_type"
                value={cfg.panel_type ?? "unknown"}
                locked={store.isLocked([...basePath, "panel_type"], redactedPaths)}
                onEdit={(p, v) => { store.trackEdit(p, v); onDirty(); }}
                options={PANEL_TYPES}
                error={fieldErrors[[...basePath, "panel_type"].join(".")]}
                help="Panel technology — picks technology-appropriate wear heuristics. unknown is always safe; wear tracking works regardless."
              />

              {/* ── Simple blank mode editor ── */}
              {isBlank && (
                <>
                  <EnumField
                    path={[...basePath, "blank_mode"]}
                    label="blank_mode"
                    value={cfg.blank_mode ?? BLANK_MODE_OPTIONS[0]}
                    locked={store.isLocked([...basePath, "blank_mode"], redactedPaths)}
                    onEdit={(p, v) => { store.trackEdit(p, v); onDirty(); }}
                    options={BLANK_MODE_OPTIONS}
                    error={fieldErrors[[...basePath, "blank_mode"].join(".")]}
                    help="power_off = full display power-off (DDC VCP D6 or DPMS); audio survives only over DDC. screen_off_audio_on = panel off, audio keeps playing (Samsung Picture-Off). brightness_zero = brightness to zero; instant but pixels may stay faintly lit."
                  />
                  <EnumField
                    path={[...basePath, "degraded_mode"]}
                    label="degraded_mode"
                    value={cfg.degraded_mode ?? BLANK_MODE_OPTIONS[0]}
                    locked={store.isLocked([...basePath, "degraded_mode"], redactedPaths)}
                    onEdit={(p, v) => { store.trackEdit(p, v); onDirty(); }}
                    options={BLANK_MODE_OPTIONS}
                    error={fieldErrors[[...basePath, "degraded_mode"].join(".")]}
                    help="Used when the primary mode isn't supported by the display. power_off = full display power-off (DDC VCP D6 or DPMS); audio survives only over DDC. screen_off_audio_on = panel off, audio keeps playing (Samsung Picture-Off). brightness_zero = brightness to zero; instant but pixels may stay faintly lit."
                  />
                </>
              )}

              {/* ── Ladder editor ── */}
              {isLadder && (
                <LadderEditor
                  stages={cfg.ladder ?? STARTER_LADDER}
                  displayId={id}
                  store={store}
                  redactedPaths={redactedPaths}
                  onDirty={onDirty}
                  fieldErrors={fieldErrors}
                />
              )}
            </div>

            {/* ── Screensaver editor — only relevant with ladder ── */}
            {cfg.screensaver && (
              <div style={{ marginTop: "14px" }}>
                <ScreensaverEditor
                  screensaver={cfg.screensaver}
                  displayId={id}
                  store={store}
                  redactedPaths={redactedPaths}
                  onDirty={onDirty}
                  fieldErrors={fieldErrors}
                />
              </div>
            )}
          </div>
        );
      })}

      {entityCrudEnabled && (
        showCreate ? (
          <CreateEntityForm
            collection="displays"
            existingIds={ids}
            initialFields={createPrefill ?? undefined}
            onCreate={(id, value) => {
              store.trackCreate("displays", id, value);
              onDirty();
              setShowCreate(false);
            }}
            onCancel={() => setShowCreate(false)}
          />
        ) : (
          <button type="button" className="cf-apply__btn cf-card__add" onClick={() => setShowCreate(true)}>
            + Add display
          </button>
        )
      )}
    </FormSection>
    {dialog}
    </>
  );
}

/**
 * Switch from ladder (or neither) to blank mode.
 * Emits: set blank_mode (previous or default) + remove ladder.
 */
function switchToBlank(
  id: string,
  cfg: DisplayConfig,
  store: PatchStore,
  onDirty: () => void,
) {
  const prevBlank = cfg.blank_mode ?? "power_off";
  store.trackEdit(["displays", id, "blank_mode"], prevBlank);
  store.trackRemove(["displays", id, "ladder"]);
  onDirty();
}

/**
 * Switch from blank (or neither) to escalation ladder.
 * Emits: set ladder (previous from config or starter) + remove blank_mode + remove degraded_mode.
 */
function switchToLadder(
  id: string,
  cfg: DisplayConfig,
  store: PatchStore,
  onDirty: () => void,
) {
  // Use the display's previous ladder from the fetched config, or the starter
  const prevLadder = cfg.ladder && cfg.ladder.length > 0 ? cfg.ladder : STARTER_LADDER;
  store.trackEdit(["displays", id, "ladder"], prevLadder);
  store.trackRemove(["displays", id, "blank_mode"]);
  store.trackRemove(["displays", id, "degraded_mode"]);
  onDirty();
}
