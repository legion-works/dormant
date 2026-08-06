/** Section-rail registration context + hooks — the shared registration state
 * consumed by SectionRailProvider (in SectionRailContext.tsx) and by the form
 * sections. Pure logic + React hooks, no JSX. */
import { createContext, useContext, useEffect, type RefObject } from "react";

export interface SectionRegistration {
  id: string;
  title: string;
  anchor: HTMLElement | null;
}

interface SectionRailContextValue {
  register: (id: string, title: string, anchor: HTMLElement | null) => void;
  unregister: (id: string, anchor: HTMLElement | null) => void;
  sections: SectionRegistration[];
}

export const SectionRailContext = createContext<SectionRailContextValue | null>(null);

export function useRegisterSection(
  id: string,
  title: string,
  anchor: HTMLElement | RefObject<HTMLElement | null> | null,
) {
  const context = useContext(SectionRailContext);
  const register = context?.register;
  const unregister = context?.unregister;
  useEffect(() => {
    if (!register || !unregister) return;
    const anchorElement = anchor && "current" in anchor ? anchor.current : anchor;
    register(id, title, anchorElement);
    return () => unregister(id, anchorElement);
  }, [anchor, register, unregister, id, title]);
}

export function useRegisteredSections() {
  const context = useContext(SectionRailContext);
  if (!context) throw new Error("useRegisteredSections must be used inside SectionRailProvider");
  return context.sections;
}
