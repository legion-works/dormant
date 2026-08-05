/**
 * DisplaySamplingEditor — per-display compositor-sampling fields.
 *
 * Mirrors `[displays.<id>.compositor_output]` and the optional
 * `[displays.<id>.sampling]` table:
 *
 *   - compositor_output         (text, top-level on DisplayConfig)
 *   - sampling.expected_source  (text, literal — no source enumeration)
 *   - sampling.source_poll_interval (humantime duration string)
 *   - sampling.stream_mode      (enum with an explicit "Inherit global"
 *                                sentinel that maps to a `remove` patch)
 *   - sampling.watched_apps     (list of Tizen app ids; issue #232)
 *
 * The stream_mode select intentionally exposes three options rather than
 * the two-element enum: the operator may want to inherit the global
 * wear.active_sampling.stream_mode for a single display instead of
 * overriding it per-display. Clearing the select back to "Inherit
 * global" emits a `remove` patch on the stream_mode path so the server
 * can fall back to the global setting.
 *
 * ALL FIVE FIELDS share the same "empty/cleared → remove" invariant.
 * The server rejects JSON `null` for Option fields (config_patch.rs
 * json_to_toml_value), rejects empty strings for the three Option /
 * humantime fields (validate.rs:1554-1599), and rejects whitespace-only
 * values for the same reasons. The only on-wire shape that expresses
 * "unset this override" is a `remove` patch — the editor translates
 * cleared text inputs and the "Inherit global" select choice through
 * `trackRemove` so the operator never lands in an unreachable 422
 * state. This mirrors ScreensaverEditor's `cleanSource` (which strips
 * empty optional fields before emit).
 *
 * `watched_apps` follows the same empty-array → `remove` invariant:
 * emptying the list disables the port-8001 app-visibility check and
 * the operator's catalog reverts to the daemon-shipped seed
 * ([`defaults::WEAR_SAMPLING_DEFAULT_WATCHED_APPS`]).
 *
 * Patch paths are deep arrays (e.g. `["displays", id, "sampling",
 * "expected_source"]`) so the server builds the intermediate `sampling`
 * table lazily on first set (config_patch.rs walk_table auto-vivifies),
 * matching how the ScreensaverEditor handles its nested sources array.
 */
import { DurationField, StringListField, TextField } from "./fields";
import type { PatchStore } from "./patch";
import type { DisplaySamplingConfig } from "../../api/types";

const STREAM_MODES = ["warm", "per-tick"] as const;

/**
 * Sentinel value for the stream_mode <select> when the operator wants
 * the global wear.active_sampling.stream_mode to take effect. Rendered
 * as the empty string so the <select> element's native `value` property
 * is naturally empty (rather than an arbitrary sentinel that would
 * leak into test selectors). Never sent on the wire — the editor
 * translates it into a `remove` patch.
 */
const INHERIT_GLOBAL = "";

interface DisplaySamplingEditorProps {
  displayId: string;
  compositor_output?: string | null;
  sampling?: DisplaySamplingConfig | null;
  store: PatchStore;
  redactedPaths: string[][];
  onDirty: () => void;
  fieldErrors: Record<string, string | undefined>;
}

/**
 * Effective value for a scalar patch:
 *
 *   1. If a `remove` is pending, return the per-type unset sentinel so
 *      the React control reflects the cleared state instead of the
 *      stale fetched prop (the store's `getEdit` collapses both "no
 *      edit" and "remove pending" into `undefined`).
 *   2. Else if a `set` is pending, return the pending value.
 *   3. Else return the fetched prop.
 */
function effective<T>(store: PatchStore, path: string[], fetched: T, unset: T): T {
  if (store.isRemoved(path)) return unset;
  const pending = store.getEdit(path);
  return pending === undefined ? fetched : (pending as T);
}

/**
 * "Empty" for the text-input contract: an empty string OR
 * whitespace-only input. The server rejects both forms (validate.rs
 * rejects whitespace for compositor_output/expected_source; humantime
 * rejects "" outright for source_poll_interval). The editor must
 * translate them through `trackRemove` instead of `trackEdit` so the
 * operator never lands in an unreachable 422.
 */
function isTextEmpty(value: unknown): boolean {
  return typeof value !== "string" || value.trim() === "";
}

export default function DisplaySamplingEditor({
  displayId,
  compositor_output,
  sampling,
  store,
  redactedPaths,
  onDirty,
  fieldErrors,
}: DisplaySamplingEditorProps) {
  const basePath = ["displays", displayId];
  const samplingPath = [...basePath, "sampling"];
  const compositorOutputPath = [...basePath, "compositor_output"];
  const expectedSourcePath = [...samplingPath, "expected_source"];
  const sourcePollIntervalPath = [...samplingPath, "source_poll_interval"];
  const streamModePath = [...samplingPath, "stream_mode"];
  const watchedAppsPath = [...samplingPath, "watched_apps"];

  // Text inputs share "" as their unset sentinel. The fetched prop is
  // also defaulted to "" when the JSON key is absent so the React
  // control's `value` prop is always a string (matching TextField /
  // DurationField's `value` type).
  const currentCompositorOutput = effective(store, compositorOutputPath, compositor_output ?? "", "");
  const currentExpectedSource = effective(store, expectedSourcePath, sampling?.expected_source ?? "", "");
  // Empty when unset — the Rust side defaults the field to 15s when
  // the JSON key is absent, so leaving the input blank inherits the
  // default without forcing the operator to type the value back. The
  // placeholder surfaces the default to the operator.
  const currentSourcePollInterval = effective(store, sourcePollIntervalPath, sampling?.source_poll_interval ?? "", "");
  // stream_mode uses null as its "unset" sentinel; the select value
  // mapping below coerces it to "" for the <select> element.
  const currentStreamMode = effective(store, streamModePath, sampling?.stream_mode ?? null, null);
  // watched_apps defaults to an empty array; the empty/cleaned state
  // disables the app-visibility check entirely (back-compat with
  // displays that pre-date #232).
  const currentWatchedApps = effective(store, watchedAppsPath, sampling?.watched_apps ?? [], []);

  const streamModeSelectValue: string = currentStreamMode ?? "";

  function emitEdit(path: string[], value: unknown) {
    store.trackEdit(path, value);
    onDirty();
  }

  function emitRemove(path: string[]) {
    store.trackRemove(path);
    onDirty();
  }

  /**
   * Emit a text-input change. Empty / whitespace-only input collapses
   * to a `remove` so the operator can express "unset this override";
   * non-empty input lands as a `set`.
   */
  function emitText(path: string[], value: unknown) {
    if (isTextEmpty(value)) {
      emitRemove(path);
    } else {
      emitEdit(path, value);
    }
  }

  function emitStreamModeSelect(value: string) {
    if (value === INHERIT_GLOBAL) {
      // Clearing the select removes the override so the global
      // wear.active_sampling.stream_mode is inherited.
      emitRemove(streamModePath);
    } else {
      emitEdit(streamModePath, value);
    }
  }

  /**
   * Emit a watched_apps change. The StringListField's onEdit fires with
   * `(path, value)` — the patch store wants a `(path, value)` pair via
   * `emitEdit`, so we ignore the path arg (it equals `watchedAppsPath`)
   * and forward the typed value. An empty array promotes to a
   * `remove` patch so the operator can express "disable the
   * app-visibility check" without leaving phantom entries behind.
   */
  function emitWatchedApps(_ignoredPath: string[], next: unknown) {
    const list = Array.isArray(next) ? (next as string[]) : [];
    if (list.length === 0) {
      emitRemove(watchedAppsPath);
    } else {
      emitEdit(watchedAppsPath, list);
    }
  }

  return (
    <div className="cf-card" style={{ borderStyle: "dashed" }}>
      <div className="cf-card__header">
        <span className="cf-card__name">Source gate</span>
      </div>
      <div className="cf-card__fields">
        <TextField
          path={compositorOutputPath}
          label="compositor_output"
          value={currentCompositorOutput}
          locked={store.isLocked(compositorOutputPath, redactedPaths)}
          onEdit={emitText}
          error={fieldErrors[compositorOutputPath.join(".")]}
          help="Compositor output the active sampler observes. Required to opt a remote-only display into sampling — leaving this empty means the display is render-only and will not appear in the wear sampled-displays list."
        />

        <TextField
          path={expectedSourcePath}
          label="expected_source"
          value={currentExpectedSource}
          locked={store.isLocked(expectedSourcePath, redactedPaths)}
          onEdit={emitText}
          error={fieldErrors[expectedSourcePath.join(".")]}
          placeholder="HDMI4"
          help="Expected compositor input-source label. The compositor's source-monitor normalizes/canonicalizes before compare, so the literal here does not have to match exactly. Leave empty to skip source verification."
        />

        <DurationField
          path={sourcePollIntervalPath}
          label="source_poll_interval"
          value={currentSourcePollInterval}
          locked={store.isLocked(sourcePollIntervalPath, redactedPaths)}
          onEdit={emitText}
          error={fieldErrors[sourcePollIntervalPath.join(".")]}
          placeholder="15s"
          help="Cadence for the compositor-side source poll. Defaults to 15s; the active-sampling pipeline clamps the value elsewhere."
        />

        <div className="cf-field">
          <label className="cf-field__label" htmlFor={streamModePath.join(".")}>stream_mode</label>
          <div className="cf-field__input-row">
            <select
              id={streamModePath.join(".")}
              className="cf-field__select"
              value={streamModeSelectValue}
              disabled={store.isLocked(streamModePath, redactedPaths)}
              onChange={(e) => emitStreamModeSelect(e.target.value)}
            >
              <option value={INHERIT_GLOBAL}>Inherit global</option>
              {STREAM_MODES.map((opt) => (
                <option key={opt} value={opt}>{opt}</option>
              ))}
            </select>
          </div>
          <span className="cf-field__hint">
            Stream setup strategy for this display. &ldquo;Inherit global&rdquo; defers to the
            wear.active_sampling.stream_mode setting; pick a value here to override per-display.
          </span>
          {fieldErrors[streamModePath.join(".")] && (
            <span className="cf-field__error">{fieldErrors[streamModePath.join(".")]}</span>
          )}
        </div>

        <StringListField
          path={watchedAppsPath}
          label="watched_apps"
          value={currentWatchedApps}
          locked={store.isLocked(watchedAppsPath, redactedPaths)}
          onEdit={emitWatchedApps}
          error={fieldErrors[watchedAppsPath.join(".")]}
          placeholder="111299001912"
          help="Tizen app ids the active-sampling source gate probes for screen ownership via port 8001. A positive visible result forces the gate to mismatched even when expected_source matches — apps own the panel without flipping inputSourceControl. The key is opt-out: when the field is absent the gate inherits the daemon-shipped seed (defaults::WEAR_SAMPLING_DEFAULT_WATCHED_APPS — Netflix, YouTube, Prime Video, etc.) so a stock TV config gets app detection out of the box. Add an entry to declare an explicit list (overrides the seed); remove all entries to apply the empty-array opt-out (pure input-only gate, no app probe). Operators should extend this list for their installed app set because current Tizen firmware has no reliable enumeration endpoint."
        />
      </div>
    </div>
  );
}
