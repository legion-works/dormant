import { useEffect, useState } from "react";
import { useRegisteredSections } from "./SectionRailContext";

export function SectionRail() {
  const sections = useRegisteredSections();
  const [activeId, setActiveId] = useState<string | null>(null);

  useEffect(() => {
    setActiveId(sections[0]?.id ?? null);
    if (typeof IntersectionObserver === "undefined") return;
    const observer = new IntersectionObserver(
      (entries) => {
        const visible = sections.filter((section) => entries.some((entry) => entry.isIntersecting && entry.target === section.anchor));
        if (visible[0]) setActiveId(visible[0].id);
      },
      { rootMargin: "-10% 0px -70% 0px", threshold: 0 },
    );
    sections.forEach((section) => section.anchor && observer.observe(section.anchor));
    return () => observer.disconnect();
  }, [sections]);

  function select(id: string) {
    const section = sections.find((item) => item.id === id);
    if (!section?.anchor) return;
    const reduced = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
    section.anchor.scrollIntoView({ behavior: reduced ? "auto" : "smooth", block: "start" });
    const route = window.location.hash.slice(1).split("#")[0];
    window.location.hash = `#${route}#config-section-${id}`;
  }

  return (
    <nav className="config-section-rail" aria-label="Config sections">
      {sections.map((section) => (
        <button
          key={section.id}
          type="button"
          aria-label={section.title}
          data-title={section.title}
          aria-current={activeId === section.id ? "location" : undefined}
          onClick={() => select(section.id)}
        />
      ))}
    </nav>
  );
}
