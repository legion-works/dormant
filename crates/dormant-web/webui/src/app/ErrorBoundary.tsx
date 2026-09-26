import { Component } from "react";
import type { ReactNode } from "react";

interface ErrorBoundaryState {
  error: Error | null;
}

export class ErrorBoundary extends Component<{ children: ReactNode }, ErrorBoundaryState> {
  state: ErrorBoundaryState = { error: null };

  static getDerivedStateFromError(error: Error): ErrorBoundaryState {
    return { error };
  }

  render() {
    if (this.state.error) {
      return (
        <main className="page-content">
          <section className="card" role="alert">
            <h1>The dashboard hit an error: {this.state.error.message}</h1>
            <p>The daemon is still running; reload the dashboard to try again.</p>
          </section>
        </main>
      );
    }
    return this.props.children;
  }
}
