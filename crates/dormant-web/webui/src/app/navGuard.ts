/**
 * Global nav guard — Shell reads this before navigating away from Config
 * to prevent data loss when the operator has unsaved edits.
 *
 * Lives in its own non-component module to satisfy the React Fast Refresh
 * rule (non-component exports from component files are flagged by oxlint).
 */

export interface NavGuardState {
  dirtyCount: number;
  discard: () => void;
  dirtySections: Set<string>;
}

export const navGuard: { current: NavGuardState | null } = { current: null };
