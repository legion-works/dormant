/** Exercise verdict aggregation — folds a step list into a single verdict. */
import type { ExerciseStep, ExerciseVerdict } from "../../api/types";

/** Aggregate a step list into a single verdict: failed takes precedence
 * over unconfirmable, which takes precedence over confirmed. An empty
 * step list (never run) is `"not_run"` — distinct from any real verdict. */
export function aggregateExerciseVerdict(steps: ExerciseStep[]): ExerciseVerdict | "not_run" {
  if (steps.length === 0) return "not_run";
  if (steps.some((step) => step.verdict === "failed")) return "failed";
  if (steps.some((step) => step.verdict === "unconfirmable")) return "unconfirmable";
  return "confirmed";
}
