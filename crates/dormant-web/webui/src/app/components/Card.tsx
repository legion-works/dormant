/**
 * Glass-panel card container using the Legion Works DS tokens.
 * The DS .lw-glass utility provides backdrop-filter glass;
 * opaque mode switches to var(--glass-fill-strong) without blur.
 */
import type { ElementType, ReactNode } from "react";
import "./Card.css";

interface CardProps {
  children: ReactNode;
  className?: string;
  as?: ElementType;
  /** Disable glass effect — use opaque surface (for data lists). */
  opaque?: boolean;
}

export default function Card({ children, className = "", as: Tag = "div", opaque = false }: CardProps) {
  const cls = `dormant-card${opaque ? " dormant-card--opaque" : " lw-glass"}${className ? " " + className : ""}`;
  return <Tag className={cls}>{children}</Tag>;
}
