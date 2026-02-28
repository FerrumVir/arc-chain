"use client";

import BlockList from "@/components/BlockList";
import { getBlocks } from "@/lib/mock-data";
import { Blocks } from "lucide-react";

export default function BlocksPage() {
  const blocks = getBlocks(50);

  return (
    <div className="flex flex-col gap-6">
      {/* Page header */}
      <div className="flex items-center justify-between">
        <div className="flex items-center gap-3">
          <div className="w-8 h-8 rounded-xl flex items-center justify-center" style={{ background: 'var(--gradient-arc)' }}>
            <Blocks className="w-4 h-4 text-white" />
          </div>
          <div>
            <h1 className="text-[20px] font-bold tracking-headline">Blocks</h1>
            <p className="text-[12px] text-[var(--text-tertiary)]">Latest 50 blocks on ARC Chain</p>
          </div>
        </div>
      </div>
      <BlockList blocks={blocks} />
    </div>
  );
}
