import type { Metadata } from "next";
import "./globals.css";
import Header from "@/components/Header";

export const metadata: Metadata = {
  title: "ARC Scan — ARC Chain Block Explorer",
  description:
    "Explore blocks, transactions, and accounts on ARC Chain. Independent BLAKE3 verification in your browser.",
};

export default function RootLayout({
  children,
}: {
  children: React.ReactNode;
}) {
  return (
    <html lang="en">
      <head>
        <link rel="preconnect" href="https://fonts.googleapis.com" />
        <link rel="preconnect" href="https://fonts.gstatic.com" crossOrigin="anonymous" />
        <link
          href="https://fonts.googleapis.com/css2?family=Inter:wght@300;400;500;600;700&family=JetBrains+Mono:wght@400;500&display=swap"
          rel="stylesheet"
        />
      </head>
      <body className="min-h-screen flex flex-col">
        <Header />
        <main className="flex-1 mx-auto w-full max-w-[1400px] px-4 sm:px-6 lg:px-8 pt-4 pb-16">
          {children}
        </main>
        <footer className="border-t border-[var(--border)]">
          <div className="mx-auto max-w-[1400px] px-4 sm:px-6 lg:px-8 py-8 flex flex-col sm:flex-row items-center justify-between gap-4">
            <div className="flex items-center gap-3">
              <div className="w-6 h-6 rounded-md flex items-center justify-center overflow-hidden" style={{ background: 'var(--gradient-arc)' }}>
                {/* eslint-disable-next-line @next/next/no-img-element */}
                <img
                  src="/brand/arc-logo-white.png"
                  alt="ARC"
                  width={14}
                  height={14}
                  className="object-contain"
                />
              </div>
              <div className="flex items-center gap-2 text-[13px]">
                <span className="font-medium text-[var(--text)]">
                  ARC Scan
                </span>
                <span className="text-[var(--text-tertiary)]">&middot;</span>
                <span className="text-[var(--text-tertiary)]">Verifiable L1 Explorer</span>
              </div>
            </div>
            <div className="flex items-center gap-6 text-[12px] text-[var(--text-tertiary)]">
              <span>Powered by ARC Chain</span>
              <span className="hidden sm:inline">&middot;</span>
              <span className="hidden sm:inline">Every transaction cryptographically verifiable</span>
            </div>
          </div>
        </footer>
      </body>
    </html>
  );
}
