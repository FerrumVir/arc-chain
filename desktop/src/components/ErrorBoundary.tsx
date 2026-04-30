import { AlertTriangle, RotateCw } from "lucide-react";
import { Component } from "react";
import type { ErrorInfo, ReactNode } from "react";
import { LogoMark } from "./Logo";

interface Props {
  children: ReactNode;
}

interface State {
  error: Error | null;
  errorInfo: ErrorInfo | null;
}

// Catches render errors from any child tree, shows a non-fatal recovery UI
// instead of the default white screen. Used once, at the top of App.
export class ErrorBoundary extends Component<Props, State> {
  state: State = { error: null, errorInfo: null };

  static getDerivedStateFromError(error: Error): Partial<State> {
    return { error };
  }

  componentDidCatch(error: Error, errorInfo: ErrorInfo): void {
    this.setState({ errorInfo });
    // eslint-disable-next-line no-console
    console.error("arc-desktop render error:", error, errorInfo.componentStack);
  }

  reset = () => {
    this.setState({ error: null, errorInfo: null });
  };

  render() {
    if (!this.state.error) return this.props.children;

    return (
      <div
        data-testid="error-boundary"
        style={{
          display: "grid",
          placeItems: "center",
          height: "100vh",
          width: "100vw",
          background: "var(--bg)",
          color: "var(--text)",
          padding: "var(--space-8)",
          textAlign: "center",
        }}
      >
        <div style={{ maxWidth: 520 }}>
          <div
            style={{
              display: "flex",
              justifyContent: "center",
              marginBottom: "var(--space-6)",
            }}
          >
            <LogoMark size={56} radius={16} />
          </div>
          <div
            style={{
              display: "inline-flex",
              alignItems: "center",
              gap: "var(--space-2)",
              padding: "4px 10px",
              borderRadius: "var(--radius-full)",
              background: "var(--danger-bg)",
              color: "var(--danger)",
              fontSize: "var(--text-xs)",
              fontWeight: 600,
              letterSpacing: "var(--tracking-wider)",
              textTransform: "uppercase",
              marginBottom: "var(--space-4)",
            }}
          >
            <AlertTriangle size={12} /> render error
          </div>
          <h1
            className="onboarding-title"
            style={{ fontSize: "var(--text-2xl)", marginBottom: "var(--space-3)" }}
          >
            something went sideways
          </h1>
          <p
            style={{
              color: "var(--text-muted)",
              fontSize: "var(--text-md)",
              marginBottom: "var(--space-6)",
              lineHeight: 1.55,
            }}
          >
            The app hit an unexpected error. Your node keeps running - this is
            a UI glitch. Restart the view to recover.
          </p>

          <div
            style={{
              fontFamily: "var(--font-mono)",
              fontSize: "var(--text-xs)",
              padding: "var(--space-4)",
              background: "var(--surface)",
              border: "1px solid var(--border)",
              borderRadius: "var(--radius-md)",
              color: "var(--danger)",
              textAlign: "left",
              overflow: "auto",
              maxHeight: 180,
              marginBottom: "var(--space-5)",
              lineHeight: 1.5,
            }}
            data-testid="error-boundary-message"
          >
            {this.state.error.message}
            {this.state.errorInfo && (
              <div
                style={{ color: "var(--text-muted)", marginTop: "var(--space-2)" }}
              >
                {this.state.errorInfo.componentStack?.split("\n")
                  .slice(0, 6)
                  .join("\n")}
              </div>
            )}
          </div>

          <div
            style={{
              display: "flex",
              gap: "var(--space-3)",
              justifyContent: "center",
            }}
          >
            <button
              className="btn btn-primary btn-lg"
              onClick={this.reset}
              data-testid="btn-error-restart"
            >
              <RotateCw size={14} /> Restart view
            </button>
            <button
              className="btn btn-secondary btn-lg"
              onClick={() => window.location.reload()}
              data-testid="btn-error-reload"
            >
              Reload app
            </button>
          </div>
        </div>
      </div>
    );
  }
}
