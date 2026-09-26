import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import "./ds.css";
import "./index.css";
import App from "./App";
import { ErrorBoundary } from "./app/ErrorBoundary";

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <ErrorBoundary><App /></ErrorBoundary>
  </StrictMode>,
);
