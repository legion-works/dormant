/**
 * Display editors section — one card per display.
 *
 * Renders the mode toggle (simple blank vs escalation ladder) and
 * the corresponding editor for each display.
 *
 * W1-5: per-entity collapse with localStorage, 230px label column,
 * changed-field markers.
 */
import FormSection from "./FormSection";
import LadderEditor from "./LadderEditor";
import ScreensaverEditor from "./ScreensaverEditor";
import { EnumField, HexCodeField, PANEL_TYPES } from "./fields";
import { useState, useEffect, useCallback } from "react";
import type { DisplayConfig, LadderStage, RuleConfig } from "../../api/types";
import type { PatchStore } from "./patch";
import CreateEntityForm from "./CreateEntityForm";
import { referencingEntities } from "./entityCrud";
import { useConfirmDialog } from "../components";
import { readEntityExpanded, writeEntityExpanded, displaySummary } from "./density";

const BLANK_MODE_OPTIONS = ["power_off", "screen_off_audio_on", "brightness_zero"] as const;

const STARTER_LADDER: LadderStage[] = [
  { kind: "render_black", dwell: "30s" },
  { kind: "power_off" },
];

type EffectiveMode = { kind: "blank" } | { kind: "ladder" };

function getEffectiveMode(id: string, cfg: DisplayConfig, store: PatchStore): EffectiveMode {
  const pendingLadder = store.getEdit(["displays", id, "ladder"]);
  const pendingBlank = store.getEdit(["displays", id, "blank_mode"]);
  if (pendingLadder !== undefined) return { kind: "ladder" };
  if (pendingBlank !== undefined) return { kind: "blank" };
  if (cfg.ladder && cfg.ladder.length > 0) return { kind: "ladder" };
  return { kind: "blank" };
}

interface DisplaysSectionProps {
  displays: Record<string, DisplayConfig>;
  store: PatchStore;
  redactedPaths: string[][];
  onDirty: () => void;
  fieldErrors: Record<string, string | undefined>;
  entityCrudEnabled?: boolean;
  rules?: Record<string, RuleConfig>;
  createPrefill?: Record<string, unknown> | null;
}

export default function DisplaysSection({
  displays, store, redactedPaths, onDirty, fieldErrors,
  entityCrudEnabled = true, rules = {}, createPrefill = null,
}: DisplaysSectionProps) {
  const ids = Object.keys(displays);
  const [, rerender] = useState(0);
  const [showCreate, setShowCreate] = useState(createPrefill != null);

  useEffect(() => { if (createPrefill) setShowCreate(true); }, [createPrefill]);

  const [expanded, setExpanded] = useState<Record<string, boolean>>(() => {
    const out: Record<string, boolean> = {};
    for (const id of ids) out[id] = readEntityExpanded("displays", id);
    return out;
  });
  const toggleExpanded = useCallback((id: string) => {
    setExpanded((prev) => {
      const next = !prev[id];
      writeEntityExpanded("displays", id, next);
      return { ...prev, [id]: next };
    });
  }, []);

  const { confirm, dialog } = useConfirmDialog();
  if (ids.length === 0 && !entityCrudEnabled) return null;

  async function handleDelete(id: string) {
    const refs = referencingEntities("displays", id, { zones: {}, rules });
    const accepted = await confirm({
      title: `Delete display "${id}"?`,
      description: refs.length > 0 ? `Referenced by ${refs.join(", ")}. Deleting it may make the pending config invalid.` : "Nothing else references displays.",
      confirmLabel: "Delete display", tone: "danger",
    });
    if (accepted) { store.trackDelete("displays", id); onDirty(); }
  }

  return (
    <>
    <FormSection title="Displays">
      {ids.map((id) => {
        const cfg = displays[id];
        const basePath = ["displays", id];
        const open = expanded[id] !== false;
        const scope = (store.getEdit([...basePath, "scope"]) ?? cfg.scope ?? "private") as "private" | "shared";

        const fetchedLadder = cfg.ladder && cfg.ladder.length > 0;
        const fetchedBlank = cfg.blank_mode !== undefined;
        const hasNeither = !fetchedLadder && !fetchedBlank;
        const effective = getEffectiveMode(id, cfg, store);
        const isLadder = effective.kind === "ladder";
        const isBlank = effective.kind === "blank";
        const modeSwitchPending = (isLadder && !fetchedLadder) || (isBlank && fetchedLadder) || (hasNeither && (store.getEdit(["displays", id, "ladder"]) !== undefined || store.getEdit(["displays", id, "blank_mode"]) !== undefined));

        return (
          <div key={id} className="cf-card">
            <div className="cf-card__header">
              <button type="button" className="cf-section__toggle"
                onClick={() => toggleExpanded(id)} aria-expanded={open}
                style={{ minWidth: 0, gap: "4px" }}>
                <span className={`cf-section__chevron${open ? " cf-section__chevron--open" : ""}`}>{"▶"}</span>
              </button>
              <span className="cf-card__name">{id}</span>
              {!open && <span className="cf-card__summary-type">{displaySummary(cfg)}</span>}
              {entityCrudEnabled && (
                <button type="button" className="cf-apply__btn cf-apply__btn--danger cf-card__delete"
                  onClick={() => handleDelete(id)}>Delete</button>
              )}
            </div>

            {open && (
            <>
              {hasNeither && !modeSwitchPending && (
                <p className="cf-placeholder">This display has neither blank_mode nor a ladder — fix the config file.</p>
              )}

              <div className="cf-card__fields">
                <div className="cf-field cf-field--row">
                  <EnumField path={[...basePath, "scope"]} label="scope" value={scope}
                    locked={store.isLocked([...basePath, "scope"], redactedPaths)}
                    onEdit={(p, v) => { store.trackEdit(p, v); onDirty(); }}
                    options={["private", "shared"]}
                    error={fieldErrors[[...basePath, "scope"].join(".")]} />
                </div>

                {scope === "shared" && (<>
                  <div className="cf-field cf-field--row"><HexCodeField path={[...basePath, "shared_input_code"]} label="shared_input_code"
                    value={cfg.shared_input_code ?? ""} locked={store.isLocked([...basePath, "shared_input_code"], redactedPaths)}
                    onEdit={(p, v) => { store.trackEdit(p, v); onDirty(); }}
                    error={fieldErrors[[...basePath, "shared_input_code"].join(".")]} /></div>
                  <div className="cf-field cf-field--row"><HexCodeField path={[...basePath, "shared_input_write_code"]} label="shared_input_write_code (optional)"
                    value={cfg.shared_input_write_code ?? ""} locked={store.isLocked([...basePath, "shared_input_write_code"], redactedPaths)}
                    onEdit={(p, v) => { store.trackEdit(p, v); onDirty(); }}
                    error={fieldErrors[[...basePath, "shared_input_write_code"].join(".")]} /></div>
                  <div className="cf-field cf-field--row"><HexCodeField path={[...basePath, "shared_peer_input_code"]} label="shared_peer_input_code (optional peer read code)"
                    value={cfg.shared_peer_input_code ?? ""} locked={store.isLocked([...basePath, "shared_peer_input_code"], redactedPaths)}
                    onEdit={(p, v) => { store.trackEdit(p, v); onDirty(); }}
                    error={fieldErrors[[...basePath, "shared_peer_input_code"].join(".")]} /></div>
                  <div className="cf-field cf-field--row"><HexCodeField path={[...basePath, "shared_peer_input_write_code"]} label="shared_peer_input_write_code (optional peer write code)"
                    value={cfg.shared_peer_input_write_code ?? ""} locked={store.isLocked([...basePath, "shared_peer_input_write_code"], redactedPaths)}
                    onEdit={(p, v) => { store.trackEdit(p, v); onDirty(); }}
                    error={fieldErrors[[...basePath, "shared_peer_input_write_code"].join(".")]} /></div>
                </>)}

                <div className="cf-field cf-field--row">
                  <label className="cf-field__label">Mode</label>
                  <div style={{ display: "flex", gap: "8px", alignItems: "center" }}>
                    <button type="button" className={`cf-apply__btn${isBlank ? " cf-apply__btn--apply" : ""}`}
                      onClick={() => { switchToBlank(id, cfg, store, onDirty); rerender((n) => n + 1); }}
                      aria-label="Simple blank mode" aria-pressed={isBlank}>
                      Simple blank
                      {isBlank && modeSwitchPending && <span style={{ fontSize: "10px", color: "var(--text-faint)", marginLeft: "4px" }}> — applies on Apply</span>}
                    </button>
                    <button type="button" className={`cf-apply__btn${isLadder ? " cf-apply__btn--apply" : ""}`}
                      onClick={() => { switchToLadder(id, cfg, store, onDirty); rerender((n) => n + 1); }}
                      aria-label="Escalation ladder" aria-pressed={isLadder}>
                      Escalation ladder
                      {isLadder && modeSwitchPending && <span style={{ fontSize: "10px", color: "var(--text-faint)", marginLeft: "4px" }}> — applies on Apply</span>}
                    </button>
                  </div>
                </div>

                <div className="cf-field cf-field--row">
                  <EnumField path={[...basePath, "panel_type"]} label="panel_type" value={cfg.panel_type ?? "unknown"}
                    locked={store.isLocked([...basePath, "panel_type"], redactedPaths)}
                    onEdit={(p, v) => { store.trackEdit(p, v); onDirty(); }} options={PANEL_TYPES}
                    error={fieldErrors[[...basePath, "panel_type"].join(".")]}
                    help="Panel technology — picks technology-appropriate wear heuristics. unknown is always safe." />
                </div>

                {isMacosDdcciPowerOffHazard(cfg, store, id) && (
                  <div className="cf-field cf-field--row cf-field--warning" role="alert">
                    <label className="cf-field__label" htmlFor={`${id}-power-off-opt-in`}>
                      power_off_opt_in
                    </label>
                    <div style={{ display: "flex", flexDirection: "column", gap: "6px" }}>
                      <span className="cf-field__warning-text">
                        power_off on this shared macOS DDC/CI topology can be unrecoverable:
                        USB-C link and hub may drop; set power_off_opt_in = true only after
                        testing physical recovery.
                      </span>
                      <label style={{ display: "flex", alignItems: "center", gap: "8px" }}>
                        <input
                          id={`${id}-power-off-opt-in`}
                          type="checkbox"
                          checked={cfg.power_off_opt_in === true}
                          disabled={store.isLocked([...basePath, "power_off_opt_in"], redactedPaths)}
                          onChange={(ev) => {
                            store.trackEdit([...basePath, "power_off_opt_in"], ev.target.checked);
                            onDirty();
                          }}
                        />
                        <span>I have tested physical recovery for this display.</span>
                      </label>
                    </div>
                  </div>
                )}

                {isBlank && (<>
                  <div className="cf-field cf-field--row">
                    <EnumField path={[...basePath, "blank_mode"]} label="blank_mode" value={cfg.blank_mode ?? BLANK_MODE_OPTIONS[0]}
                      locked={store.isLocked([...basePath, "blank_mode"], redactedPaths)}
                      onEdit={(p, v) => { store.trackEdit(p, v); onDirty(); }} options={BLANK_MODE_OPTIONS}
                      error={fieldErrors[[...basePath, "blank_mode"].join(".")]}
                      help="power_off = full display power-off (DDC VCP D6 or DPMS). screen_off_audio_on = panel off, audio keeps playing. brightness_zero = brightness to zero; instant but pixels may stay faintly lit." />
                  </div>
                  <div className="cf-field cf-field--row">
                    <EnumField path={[...basePath, "degraded_mode"]} label="degraded_mode" value={cfg.degraded_mode ?? BLANK_MODE_OPTIONS[0]}
                      locked={store.isLocked([...basePath, "degraded_mode"], redactedPaths)}
                      onEdit={(p, v) => { store.trackEdit(p, v); onDirty(); }} options={BLANK_MODE_OPTIONS}
                      error={fieldErrors[[...basePath, "degraded_mode"].join(".")]}
                      help="Used when the primary mode isn't supported by the display." />
                  </div>
                </>)}

                {isLadder && (
                  <LadderEditor stages={cfg.ladder ?? STARTER_LADDER} displayId={id} store={store}
                    redactedPaths={redactedPaths} onDirty={onDirty} fieldErrors={fieldErrors} />
                )}
              </div>

              {cfg.screensaver && (
                <div style={{ marginTop: "14px" }}>
                  <ScreensaverEditor screensaver={cfg.screensaver} displayId={id} store={store}
                    redactedPaths={redactedPaths} onDirty={onDirty} fieldErrors={fieldErrors} />
                </div>
              )}
            </>
            )}
          </div>
        );
      })}

      {entityCrudEnabled && (
        showCreate ? (
          <CreateEntityForm collection="displays" existingIds={ids} initialFields={createPrefill ?? undefined}
            onCreate={(id, value) => { store.trackCreate("displays", id, value); onDirty(); setShowCreate(false); }}
            onCancel={() => setShowCreate(false)} />
        ) : (
          <button type="button" className="cf-apply__btn cf-card__add" onClick={() => setShowCreate(true)}>+ Add display</button>
        )
      )}
    </FormSection>
    {dialog}
    </>
  );
}

function switchToBlank(id: string, cfg: DisplayConfig, store: PatchStore, onDirty: () => void) {
  const prevBlank = cfg.blank_mode ?? "power_off";
  store.trackEdit(["displays", id, "blank_mode"], prevBlank);
  store.trackRemove(["displays", id, "ladder"]);
  onDirty();
}

function switchToLadder(id: string, cfg: DisplayConfig, store: PatchStore, onDirty: () => void) {
  const prevLadder = cfg.ladder && cfg.ladder.length > 0 ? cfg.ladder : STARTER_LADDER;
  store.trackEdit(["displays", id, "ladder"], prevLadder);
  store.trackRemove(["displays", id, "blank_mode"]);
  store.trackRemove(["displays", id, "degraded_mode"]);
  onDirty();
}

/**
 * True when the display is wired into the macOS shared-DDC/CI power-off
 * hazard topology (issue #126): scope = "shared", FIRST controller =
 * "ddcci", and the effective primary blank mode is "power_off". Mirrors
 * `is_macos_power_off_hazard` in `dormant_core::config::validate` so the
 * web UI surfaces the hazard checkbox in the same conditions the daemon
 * would emit its load-time warning. `power_off_opt_in = true` is treated
 * as acknowledgement and suppresses the checkbox display.
 */
function isMacosDdcciPowerOffHazard(
  cfg: DisplayConfig,
  store: PatchStore,
  id: string,
): boolean {
  const pendingScope = store.getEdit(["displays", id, "scope"]) ?? cfg.scope ?? "private";
  if (pendingScope !== "shared") return false;
  const pendingControllers = (store.getEdit(["displays", id, "controllers"]) as string[] | undefined)
    ?? cfg.controllers;
  if (pendingControllers[0] !== "ddcci") return false;
  const pendingBlank = (store.getEdit(["displays", id, "blank_mode"]) as string | undefined)
    ?? cfg.blank_mode;
  if (pendingBlank !== "power_off") return false;
  const pendingOptIn = store.getEdit(["displays", id, "power_off_opt_in"]) as boolean | undefined;
  if (pendingOptIn === true) return false;
  if (cfg.power_off_opt_in === true) return false;
  return true;
}
