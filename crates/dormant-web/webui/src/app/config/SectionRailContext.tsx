import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState, type ReactNode, type RefObject } from "react";

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

const SectionRailContext = createContext<SectionRailContextValue | null>(null);

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
