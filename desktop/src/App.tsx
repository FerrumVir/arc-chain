import { useEffect, useState } from "react";
import { AnimatePresence, motion } from "framer-motion";
import { Sidebar } from "./components/Sidebar";
import { Titlebar } from "./components/Titlebar";
import { DataMigrationBanner } from "./components/DataMigrationBanner";
import { Onboarding } from "./screens/Onboarding";
import { Dashboard } from "./screens/Dashboard";
import { Earnings } from "./screens/Earnings";
import { Inference } from "./screens/Inference";
import { Network } from "./screens/Network";
import { Logs } from "./screens/Logs";
import { Settings } from "./screens/Settings";
import { Wallet } from "./screens/Wallet";
import { useAppStore } from "./lib/store";
import {
  api,
  isBlockedProductionBrowser,
  isSyntheticPreview,
} from "./lib/tauri";
import { appUpdater } from "./lib/updater";
import type { DataMigrationNotice } from "./lib/types";

const SCREENS = {
  dashboard: Dashboard,
  wallet: Wallet,
  inference: Inference,
  earnings: Earnings,
  network: Network,
  logs: Logs,
  settings: Settings,
} as const;

function SyntheticPreviewBanner() {
  if (!isSyntheticPreview) return null;
  return (
    <aside
      className="synthetic-preview-banner"
      data-testid="synthetic-preview-banner"
      role="status"
    >
      <strong>SYNTHETIC UI PREVIEW — NOT LIVE ARC DATA</strong>
      <span>
        Balances, blocks, inference activity, receipts, earnings, and
        projections on this browser page are test fixtures. Open the signed ARC
        desktop app to read a node and chain host.
      </span>
    </aside>
  );
}

function ProductionBrowserBlocker() {
  return (
    <main
      className="production-browser-blocker"
      data-testid="production-browser-blocker"
    >
      <div>
        <p>ARC DESKTOP SAFETY BOUNDARY</p>
        <h1>Open the native ARC app</h1>
        <p>
          This production bundle is outside its signed native host, so node,
          chain, inference, receipt, and earnings data are disabled. No preview
          values or network-error fallback has been loaded.
        </p>
      </div>
    </main>
  );
}

export function App() {
  const onboardedFlag = useAppStore((s) => s.onboarded);
  const config = useAppStore((s) => s.config);
  const route = useAppStore((s) => s.route);
  const setConfig = useAppStore((s) => s.setConfig);
  const setIdentity = useAppStore((s) => s.setIdentity);
  const setOnboarded = useAppStore((s) => s.setOnboarded);
  const [configHydrated, setConfigHydrated] = useState(false);
  const [dataMigration, setDataMigration] =
    useState<DataMigrationNotice | null>(null);

  // The wizard stages identity/config before downloading and starting the
  // node. Those drafts must not unmount it while setup can still fail.
  // Native-store hydration below restores completion for existing installs.
  const onboarded = onboardedFlag;

  useEffect(() => {
    let active = true;
    (async () => {
      const [loadedIdentity, loadedConfig, loadedMigration] = await Promise.all([
        api.loadIdentity().catch(() => null),
        api.loadConfig().catch(() => null),
        api.loadDataMigrationNotice().catch(() => null),
      ]);
      if (loadedIdentity) setIdentity(loadedIdentity);
      if (loadedConfig) setConfig(loadedConfig);
      if (loadedIdentity && loadedConfig) setOnboarded(true);
      if (active) setDataMigration(loadedMigration);
      if (active) setConfigHydrated(true);
    })();
    return () => {
      active = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Wait for the native store to hydrate so stale localStorage preferences
  // cannot start checks or unattended installs that persisted config disabled.
  // The singleton scheduler owns one startup timer and one daily interval.
  useEffect(() => {
    const autoChecksEnabled =
      configHydrated && config?.autoUpdate === true;
    appUpdater.setAutoChecksEnabled(autoChecksEnabled);
    appUpdater.setAutoInstallEnabled(
      autoChecksEnabled && config?.autoInstallUpdates === true,
    );
    return () => {
      appUpdater.setAutoInstallEnabled(false);
      appUpdater.setAutoChecksEnabled(false);
    };
  }, [configHydrated, config?.autoUpdate, config?.autoInstallUpdates]);

  if (isBlockedProductionBrowser) {
    return <ProductionBrowserBlocker />;
  }

  if (!onboarded) {
    if (!isSyntheticPreview) return <Onboarding />;
    return (
      <div className="synthetic-preview-page">
        <Onboarding />
        <SyntheticPreviewBanner />
      </div>
    );
  }

  const Screen = SCREENS[route];

  return (
    <div
      className={`app-shell${isSyntheticPreview ? " synthetic-preview" : ""}`}
      data-testid="app-shell"
    >
      <Titlebar />
      <Sidebar />
      <main className="main" data-testid="main">
        {dataMigration && (
          <DataMigrationBanner
            notice={dataMigration}
            onDismissed={() => setDataMigration(null)}
          />
        )}
        <AnimatePresence mode="wait">
          <motion.div
            key={route}
            initial={{ opacity: 0, y: 8 }}
            animate={{ opacity: 1, y: 0 }}
            exit={{ opacity: 0, y: -6 }}
            transition={{ duration: 0.22, ease: [0.22, 1, 0.36, 1] }}
          >
            <Screen />
          </motion.div>
        </AnimatePresence>
      </main>
      <SyntheticPreviewBanner />
    </div>
  );
}
