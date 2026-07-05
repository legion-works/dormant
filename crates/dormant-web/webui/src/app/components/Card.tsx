/**
 * Glass-panel card container matching the dormant prototype.
 *
 * Uses the DS .lw-glass recipe + an explicit solid fallback
 * for environments where backdrop-filter is unavailable.
 */
import type { ElementType, ReactNode } from "react";
import "./Card.css";

interface CardProps {
  children: ReactNode;
  /** Additional class names. */
  className?: string;
  /** Render as a different element (e.g. "section").  Defaults to "div". */
  as?: ElementType;
  /** Disable glass effect — use opaque surface (for data lists). */
  opaque?: boolean;
}

export default function Card({ children, className = "", as: Tag = "div", opaque = false }: CardProps) {
  return (
    <Tag className={`dormant-card${opaque ? " dormant-card--opaque" : ""}${className ? " " + className : ""}`}>
      {children}
    </Tag>
  );
}
