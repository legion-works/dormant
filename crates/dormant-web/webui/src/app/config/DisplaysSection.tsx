/**
 * Display editors section — one card per display.
 *
 * Renders the mode toggle (simple blank vs escalation ladder) and
 * the corresponding editor for each display.  Displays with no
 * configured mode get a read-only warning card.
 */
import FormSection from "./FormSection";
import LadderEditor from "./LadderEditor";
import ScreensaverEditor from "./ScreensaverEditor";
import { EnumField } from "./fields";
import type { DisplayConfig, LadderStage } from "../../api/types";
import type { PatchStore } from "./patch";

const BLANK_MODE_OPTIONS = ["power_off", "screen_off_audio_on", "brightness_zero"] as const;

/**
 * The ladder starter used when switching TO ladder and the display
 * had no previous ladder in the fetched config.
 */
const STARTER_LADDER: LadderStage[] = [
  { kind: "render_black", dwell: "30s" },
  { kind: "power_off" },
];

interface DisplaysSectionProps {
  displays: Record<string, DisplayConfig>;
  store: PatchStore;
  redactedPaths: string[][];
  onDirty: () => void;
  fieldErrors: Record<string, string | undefined>;
}

export default function DisplaysSection({ displays, store, redactedPaths, onDirty, fieldErrors }: DisplaysSectionProps) {
  const ids = Object.keys(displays);
  if (ids.length === 0) return null;

  return (
    <FormSection title="Displays">
      {ids.map((id) => {
        const cfg = displays[id];
        const basePath = ["displays", id];

        // Determine current mode
        const hasLadder = cfg.ladder && cfg.ladder.length > 0;
        const hasBlankMode = cfg.blank_mode !== undefined;
        const hasNeither = !hasLadder && !hasBlankMode;

        return (
          <div key={id} className="cf-card">
            <div className="cf-card__header">
              <span className="cf-card__name">{id}</span>
            </div>

            {/* ── No-mode warning ── */}
            {hasNeither && (
              <p className="cf-placeholder">
                This display has no blank_mode or ladder configured.
                Choose a mode below — the config file must be edited directly until this editor ships.
              </p>
            )}

            <div className="cf-card__fields">
              {/* ── Mode toggle ── */}
              <div className="cf-field">
                <label className="cf-field__label">Mode</label>
                <div style={{ display: "flex", gap: "8px" }}>
                  <button
                    type="button"
                    className={`cf-apply__btn${!hasLadder ? " cf-apply__btn--apply" : ""}`}
                    onClick={() => switchToBlank(id, cfg, store, onDirty)}
                    aria-label="Simple blank mode"
                    aria-pressed={!hasLadder}
                  >
                    Simple blank
                  </button>
                  <button
                    type="button"
                    className={`cf-apply__btn${hasLadder ? " cf-apply__btn--apply" : ""}`}
                    onClick={() => switchToLadder(id, cfg, store, onDirty)}
                    aria-label="Escalation ladder"
                    aria-pressed={hasLadder}
                  >
                    Escalation ladder
                  </button>
                </div>
              </div>

              {/* ── Simple blank mode editor ── */}
              {!hasLadder && (
                <>
                  <EnumField
                    path={[...basePath, "blank_mode"]}
                    label="blank_mode"
                    value={cfg.blank_mode ?? BLANK_MODE_OPTIONS[0]}
                    locked={store.isLocked([...basePath, "blank_mode"], redactedPaths)}
                    onEdit={(p, v) => { store.trackEdit(p, v); onDirty(); }}
                    options={BLANK_MODE_OPTIONS}
                    error={fieldErrors[[...basePath, "blank_mode"].join(".")]}
                  />
                  <EnumField
                    path={[...basePath, "degraded_mode"]}
                    label="degraded_mode"
                    value={cfg.degraded_mode ?? BLANK_MODE_OPTIONS[0]}
                    locked={store.isLocked([...basePath, "degraded_mode"], redactedPaths)}
                    onEdit={(p, v) => { store.trackEdit(p, v); onDirty(); }}
                    options={BLANK_MODE_OPTIONS}
                    error={fieldErrors[[...basePath, "degraded_mode"].join(".")]}
                  />
                </>
              )}

              {/* ── Ladder editor ── */}
              {hasLadder && (
                <LadderEditor
                  stages={cfg.ladder!}
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
    </FormSection>
  );
}

/**
 * Switch from ladder (or neither) to blank mode.
 * Emits: set blank_mode (previous or default) + remove ladder.
 */
function switchToBlank(
  id: string,
  _cfg: DisplayConfig,
  store: PatchStore,
  onDirty: () => void,
) {
  const prevBlank = _cfg.blank_mode ?? "power_off";
  // The fetched config's blank_mode value — if the config currently has a ladder,
  // we set the blank_mode to what it was before (or default).
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
