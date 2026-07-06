/**
 * Minimal client-side navigation hook.
 *
 * Views use this to switch between the five tabs without
 * depending on Shell-internal state.  Navigation is
 * hash-based (matching the Shell's hashchange listener).
 */
import { useCallback } from "react";

type ViewKey = "dashboard" | "displays" | "events" | "config" | "doctor";

export function useNavigate() {
  return useCallback((key: ViewKey) => {
    window.location.hash = `#/${key}`;
  }, []);
}
