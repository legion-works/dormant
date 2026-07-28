/**
 * Collapsible form section shell with section-header styling
 * matching the Dashboard conventions.
 */
import { useRef, useState } from "react";
import type { ReactNode } from "react";
import { useRegisterSection } from "./SectionRailContext";

interface FormSectionProps {
  id: string;
  title: string;
  children: ReactNode;
  defaultOpen?: boolean;
}

export default function FormSection({ id, title, children, defaultOpen = true }: FormSectionProps) {
  const [open, setOpen] = useState(defaultOpen);
  const rootRef = useRef<HTMLDivElement>(null);
  useRegisterSection(id, title, rootRef);

  return (
    <div ref={rootRef} id={`config-section-${id}`} data-config-section={id} className="cf-section">
      <div className="cf-section__header">
        <button
          type="button"
          className="cf-section__toggle"
          onClick={() => setOpen((o) => !o)}
          aria-expanded={open}
        >
          <span className={`cf-section__chevron${open ? " cf-section__chevron--open" : ""}`}>
            {"▶"}
          </span>
          <h2 className="cf-section__title">{title}</h2>
        </button>
      </div>
      {open && <div className="cf-section__body">{children}</div>}
    </div>
  );
}
