import { ExternalLink, Moon } from "lucide-react";
import { useQuery } from "@tanstack/react-query";
import { api, isSyntheticPreview } from "../lib/tauri";
import { StatusPill } from "./StatusPill";
import { ArcDevice } from "./Logo";

export function Titlebar() {
  const { data: status } = useQuery({
    queryKey: ["status"],
    queryFn: api.nodeStatus,
    refetchInterval: 2000,
  });

  const level = status?.health ?? "offline";

  return (
    <div className="titlebar" data-testid="titlebar" data-tauri-drag-region>
      <div className="titlebar-center">
        <ArcDevice size={14} color="var(--arc)" />
        <span style={{ color: "var(--text)" }}>arc</span>
        <span style={{ opacity: 0.4 }}>·</span>
        <span>node</span>
        <span style={{ opacity: 0.4 }}>·</span>
        <span>testnet</span>
      </div>

      <div className="titlebar-right">
        <StatusPill level={level} />
        {isSyntheticPreview && (
          <span
            style={{
              fontSize: 10,
              color: "var(--text-faint)",
              display: "flex",
              alignItems: "center",
              gap: 4,
            }}
            data-testid="preview-mode"
          >
            <Moon size={10} /> Synthetic preview · not live
          </span>
        )}
        <button
          className="btn btn-ghost btn-sm"
          onClick={() => api.openExternal("https://github.com/FerrumVir/arc-chain")}
          data-testid="open-github"
          aria-label="Open GitHub"
        >
          <ExternalLink size={13} /> GitHub
        </button>
      </div>
    </div>
  );
}
