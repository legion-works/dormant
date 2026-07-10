/**
 * Screensaver editor — scalar config fields + source list.
 *
 * Scalar fields: audio, scale_mode, transition, transition_duration.
 * Sources: per-source cards with path, recurse, shuffle, order, image_duration.
 *
 * Ancestor-lock: the ENTIRE sources editor locks when any redacted path
 * is a descendant of this display's `screensaver.source` prefix
 * (e.g. a redacted URL inside a source entry).
 *
 * The working source array is `store.getEdit(sourcePath) ?? fetchedSources`,
 * so sequential edits on different fields of the same source accumulate
 * instead of overwriting each other.
 */
import { BoolField, DurationField, EnumField, NumberField, TextField } from "./fields";
import type { ScreensaverConfig, ScreensaverSource } from "../../api/types";
import type { PatchStore } from "./patch";

const SCALE_MODES = ["fill", "fit", "stretch", "center"] as const;
const TRANSITIONS = ["none", "crossfade"] as const;
const SOURCE_ORDERS = ["sequential"] as const;

/** Default source appended when "Add Source" is clicked. */
const DEFAULT_SOURCE: ScreensaverSource = {
  path: "",
  recurse: false,
  shuffle: false,
  order: "sequential",
};

interface ScreensaverEditorProps {
  screensaver: ScreensaverConfig;
  displayId: string;
  store: PatchStore;
  redactedPaths: string[][];
  onDirty: () => void;
  fieldErrors: Record<string, string | undefined>;
}

/**
 * Read the effective source array: pending state wins over fetched prop.
 */
function getEffectiveSources(displayId: string, fetched: ScreensaverSource[], store: PatchStore): ScreensaverSource[] {
  const sourcePath = ["displays", displayId, "screensaver", "source"];
  const pending = store.getEdit(sourcePath);
  return (pending as ScreensaverSource[] | undefined) ?? fetched;
}

export default function ScreensaverEditor({ screensaver, displayId, store, redactedPaths, onDirty, fieldErrors }: ScreensaverEditorProps) {
  const basePath = ["displays", displayId, "screensaver"];
  const sourcePath = [...basePath, "source"];

  // Ancestor-lock: any redacted path that starts with the source-path prefix
  // means the entire sources editor is locked.
  const sourcesLocked = store.isLocked(sourcePath, redactedPaths);
  const sourcesLockReason = sourcesLocked ? "contains credentialed URLs — edit in the config file" : undefined;

  // Working array: pending overlay wins over fetched prop.
  const fetchedSources: ScreensaverSource[] = screensaver.source ?? [];
  const sources = getEffectiveSources(displayId, fetchedSources, store);

  /**
   * Whether a value represents "no input" — null from the server's JSON
   * serialisation of Rust Option::None, or an empty/whitespace string
   * from a cleared input field.
   */
  function isAbsentInput(v: unknown): boolean {
    return v === null || (typeof v === "string" && v.trim() === "");
  }

  /**
   * Strip absent values from optional ScreensaverSource fields.
   *
   * Every `Option` field in the Rust schema serialises None as JSON null:
   * path, order, image_duration.  A cleared input also produces "".
   * These must be absent in emitted patches — the server rejects null
   * and empty strings for Option fields.
   */
  function cleanSource(s: ScreensaverSource): ScreensaverSource {
    const out: Record<string, unknown> = { ...s };
    // All three Option fields in the Rust struct (see config/schema.rs).
    const optionalKeys = ["path", "order", "image_duration"];
    for (const key of optionalKeys) {
      if (isAbsentInput(out[key])) delete out[key];
    }
    return out as ScreensaverSource;
  }

  function emitSources(next: ScreensaverSource[]) {
    store.trackEdit(sourcePath, next.map(cleanSource));
    onDirty();
  }

  return (
    <div className="cf-card" style={{ borderStyle: "dashed" }}>
      <div className="cf-card__header">
        <span className="cf-card__name">Screensaver</span>
        {sourcesLocked && (
          <span className="cf-field__lock" title={sourcesLockReason} aria-label={sourcesLockReason}>{"🔒"}</span>
        )}
      </div>

      <div className="cf-card__fields">
        {/* ── Scalar fields ── */}
        <BoolField
          path={[...basePath, "audio"]}
          label="audio"
          value={screensaver.audio}
          locked={store.isLocked([...basePath, "audio"], redactedPaths)}
          onEdit={(p, v) => { store.trackEdit(p, v); onDirty(); }}
          error={fieldErrors[[...basePath, "audio"].join(".")]}
          help="Play the screensaver's audio track. Off by default (muted)."
        />

        <EnumField
          path={[...basePath, "scale_mode"]}
          label="scale_mode"
          value={screensaver.scale_mode ?? "fill"}
          locked={store.isLocked([...basePath, "scale_mode"], redactedPaths)}
          onEdit={(p, v) => { store.trackEdit(p, v); onDirty(); }}
          options={SCALE_MODES}
          error={fieldErrors[[...basePath, "scale_mode"].join(".")]}
          help="fill = crop to fill the screen; fit = letterbox, no crop; stretch = distort to fill; center = native size, centered."
        />

        <EnumField
          path={[...basePath, "transition"]}
          label="transition"
          value={screensaver.transition ?? "crossfade"}
          locked={store.isLocked([...basePath, "transition"], redactedPaths)}
          onEdit={(p, v) => { store.trackEdit(p, v); onDirty(); }}
          options={TRANSITIONS}
          error={fieldErrors[[...basePath, "transition"].join(".")]}
          help="none = hard cut between images; crossfade = fade between them."
        />

        <DurationField
          path={[...basePath, "transition_duration"]}
          label="transition_duration"
          value={screensaver.transition_duration ?? "1s"}
          locked={store.isLocked([...basePath, "transition_duration"], redactedPaths)}
          onEdit={(p, v) => { store.trackEdit(p, v); onDirty(); }}
          error={fieldErrors[[...basePath, "transition_duration"].join(".")]}
        />

        <NumberField
          path={[...basePath, "shift_px"]}
          label="shift_px"
          value={screensaver.shift_px ?? 2}
          locked={store.isLocked([...basePath, "shift_px"], redactedPaths)}
          onEdit={(p, v) => { store.trackEdit(p, v); onDirty(); }}
          error={fieldErrors[[...basePath, "shift_px"].join(".")]}
          help="Pixel-shift distance (px) applied periodically while the screensaver is active, to reduce static-image burn-in risk."
        />

        <DurationField
          path={[...basePath, "shift_interval"]}
          label="shift_interval"
          value={screensaver.shift_interval ?? "120s"}
          locked={store.isLocked([...basePath, "shift_interval"], redactedPaths)}
          onEdit={(p, v) => { store.trackEdit(p, v); onDirty(); }}
          error={fieldErrors[[...basePath, "shift_interval"].join(".")]}
          help="Interval between successive pixel shifts."
        />
      </div>

      {/* ── Sources ── */}
      <div style={{ marginTop: "14px" }}>
        <div style={{
          fontFamily: "var(--font-mono)", fontSize: "var(--text-2xs)",
          color: "var(--text-muted)", marginBottom: "8px",
          textTransform: "uppercase", letterSpacing: "var(--tracking-caps)",
        }}>
          Sources
          {sourcesLocked && <span className="cf-field__lock" title={sourcesLockReason} aria-label={sourcesLockReason}>{" 🔒"}</span>}
        </div>

        {sources.map((src, idx) => {
          const srcBase = [...sourcePath, String(idx)];
          const srcLocked = sourcesLocked || store.isLocked(srcBase, redactedPaths);

          return (
            <div key={idx} className="cf-card" style={{ marginBottom: "8px", borderStyle: srcLocked ? "dashed" : undefined }}>
              <div className="cf-card__header">
                <span className="cf-card__type">Source {idx + 1}</span>
                {!sourcesLocked && (
                  <button
                    type="button"
                    className="cf-apply__btn cf-apply__btn--discard"
                    style={{ marginLeft: "auto", padding: "2px 8px", fontSize: "10px" }}
                    onClick={() => {
                      const effective = getEffectiveSources(displayId, fetchedSources, store);
                      const next = effective.filter((_, i) => i !== idx);
                      emitSources(next);
                    }}
                    aria-label="Remove source"
                    title="Remove source"
                  >
                    ✕
                  </button>
                )}
              </div>

              <div className="cf-card__fields">
                <TextField
                  path={[...srcBase, "path"]}
                  label="path"
                  value={src.path ?? ""}
                  locked={srcLocked || store.isLocked([...srcBase, "path"], redactedPaths)}
                  onEdit={(_p, v) => {
                    const effective = getEffectiveSources(displayId, fetchedSources, store);
                    const next = [...effective];
                    next[idx] = { ...next[idx], path: v as string };
                    emitSources(next);
                  }}
                  error={fieldErrors[[...srcBase, "path"].join(".")]}
                />

                <BoolField
                  path={[...srcBase, "recurse"]}
                  label="recurse"
                  value={src.recurse ?? false}
                  locked={srcLocked || store.isLocked([...srcBase, "recurse"], redactedPaths)}
                  onEdit={(_p, v) => {
                    const effective = getEffectiveSources(displayId, fetchedSources, store);
                    const next = [...effective];
                    next[idx] = { ...next[idx], recurse: v as boolean };
                    emitSources(next);
                  }}
                  error={fieldErrors[[...srcBase, "recurse"].join(".")]}
                />

                <BoolField
                  path={[...srcBase, "shuffle"]}
                  label="shuffle"
                  value={src.shuffle ?? false}
                  locked={srcLocked || store.isLocked([...srcBase, "shuffle"], redactedPaths)}
                  onEdit={(_p, v) => {
                    const effective = getEffectiveSources(displayId, fetchedSources, store);
                    const next = [...effective];
                    next[idx] = { ...next[idx], shuffle: v as boolean };
                    emitSources(next);
                  }}
                  error={fieldErrors[[...srcBase, "shuffle"].join(".")]}
                  help="Shuffle this source's items. Mutually exclusive with order."
                />

                <EnumField
                  path={[...srcBase, "order"]}
                  label="order"
                  value={src.order ?? "sequential"}
                  locked={srcLocked || store.isLocked([...srcBase, "order"], redactedPaths)}
                  onEdit={(_p, v) => {
                    const effective = getEffectiveSources(displayId, fetchedSources, store);
                    const next = [...effective];
                    next[idx] = { ...next[idx], order: v as string };
                    emitSources(next);
                  }}
                  options={SOURCE_ORDERS}
                  error={fieldErrors[[...srcBase, "order"].join(".")]}
                  help="sequential = in directory order. For random playback, use the shuffle flag instead."
                />

                <DurationField
                  path={[...srcBase, "image_duration"]}
                  label="image_duration"
                  value={src.image_duration ?? ""}
                  locked={srcLocked || store.isLocked([...srcBase, "image_duration"], redactedPaths)}
                  onEdit={(_p, v) => {
                    const effective = getEffectiveSources(displayId, fetchedSources, store);
                    const next = [...effective];
                    next[idx] = { ...next[idx], image_duration: v as string };
                    emitSources(next);
                  }}
                  error={fieldErrors[[...srcBase, "image_duration"].join(".")]}
                />
              </div>
            </div>
          );
        })}

        {/* Add source button */}
        {!sourcesLocked && (
          <button
            type="button"
            className="cf-apply__btn"
            onClick={() => {
              const effective = getEffectiveSources(displayId, fetchedSources, store);
              const next = [...effective, { ...DEFAULT_SOURCE }];
              emitSources(next);
            }}
            aria-label="Add source"
          >
            + Add source
          </button>
        )}
      </div>
    </div>
  );
}
