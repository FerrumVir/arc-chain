import { ExternalLink, Moon } from "lucide-react";
import { useQuery } from "@tanstack/react-query";
import { api, isSyntheticPreview } from "../lib/tauri";
import { StatusPill } from "./StatusPill";
import { Wordmark } from "./Logo";

export function Titlebar() {
  const { data: status } = useQuery({
    queryKey: ["status"],
    queryFn: api.nodeStatus,
    refetchInterval: 2000,
  });

  // Until the first status arrives nothing is known: say "Checking", not "Offline".
  const level = status ? status.health : "checking";

  return (
    <div className="titlebar" data-testid="titlebar" data-tauri-drag-region>
      <div className="titlebar-center">
        <Wordmark height={13} />
        <span className="titlebar-sep" aria-hidden="true">·</span>
        <span>node</span>
        <span className="titlebar-sep" aria-hidden="true">·</span>
        <span>testnet</span>
      </div>

      <div className="titlebar-right">
        <StatusPill level={level} />
        {isSyntheticPreview && (
          <span className="titlebar-preview" data-testid="preview-mode">
            <Moon size={10} /> Synthetic preview · not live
          </span>
        )}
        <button
          className="btn btn-ghost btn-sm titlebar-github"
          onClick={() => api.openExternal("https://github.com/FerrumVir/arc-chain")}
          data-testid="open-github"
          aria-label="Open GitHub"
        >
          <ExternalLink size={13} /> <span className="titlebar-github-label">GitHub</span>
        </button>
      </div>
    </div>
  );
}
