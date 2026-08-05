/** SectionRailProvider — owns the registration map and exposes it via the
 * section-rail context defined in sectionRail.ts. */
import { useCallback, useMemo, useRef, useState, type ReactNode } from "react";
import { SectionRailContext, type SectionRegistration } from "./sectionRail";

export function SectionRailProvider({ tab, children }: { tab: string; children: ReactNode }) {
  const registrations = useRef(new Map<string, SectionRegistration>());
  const [sections, setSections] = useState<SectionRegistration[]>([]);
  const previousTab = useRef(tab);
  if (previousTab.current !== tab) {
    previousTab.current = tab;
    registrations.current.clear();
    if (sections.length > 0) setSections([]);
  }

  const register = useCallback((id: string, title: string, anchor: HTMLElement | null) => {
    registrations.current.set(id, { id, title, anchor });
    setSections([...registrations.current.values()]);
  }, []);
  const unregister = useCallback((id: string, anchor: HTMLElement | null) => {
    const current = registrations.current.get(id);
    if (current?.anchor === anchor) {
      registrations.current.delete(id);
      setSections([...registrations.current.values()]);
    }
  }, []);
  const value = useMemo(() => ({ register, unregister, sections }), [register, unregister, sections]);

  return <SectionRailContext.Provider value={value}>{children}</SectionRailContext.Provider>;
}
