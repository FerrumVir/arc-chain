import { useEffect, useState } from "react";
import { AnimatePresence, motion } from "framer-motion";
import { Sidebar } from "./components/Sidebar";
import { Titlebar } from "./components/Titlebar";
import { Onboarding } from "./screens/Onboarding";
import { Dashboard } from "./screens/Dashboard";
import { Earnings } from "./screens/Earnings";
import { Inference } from "./screens/Inference";
import { Network } from "./screens/Network";
import { Logs } from "./screens/Logs";
import { Settings } from "./screens/Settings";
import { Wallet } from "./screens/Wallet";
import { useAppStore } from "./lib/store";
import { api } from "./lib/tauri";
import { appUpdater } from "./lib/updater";

const SCREENS = {
  dashboard: Dashboard,
  wallet: Wallet,
  inference: Inference,
  earnings: Earnings,
  network: Network,
  logs: Logs,
  settings: Settings,
} as const;

export function App() {
  const onboardedFlag = useAppStore((s) => s.onboarded);
  const identity = useAppStore((s) => s.identity);
  const config = useAppStore((s) => s.config);
  const route = useAppStore((s) => s.route);
  const setConfig = useAppStore((s) => s.setConfig);
  const setIdentity = useAppStore((s) => s.setIdentity);
  const setOnboarded = useAppStore((s) => s.setOnboarded);
  const [configHydrated, setConfigHydrated] = useState(false);

  // Compute onboarded from *either* the explicit flag (set by the wizard on
  // launch) or the presence of a persisted identity+config pair. This way a
  // reinstall or fresh launch with an existing Rust-side store skips onboarding.
  const onboarded = onboardedFlag || (!!identity && !!config);

  useEffect(() => {
    let active = true;
    (async () => {
      const [loadedIdentity, loadedConfig] = await Promise.all([
        api.loadIdentity().catch(() => null),
        api.loadConfig().catch(() => null),
      ]);
      if (loadedIdentity) setIdentity(loadedIdentity);
      if (loadedConfig) setConfig(loadedConfig);
      if (loadedIdentity && loadedConfig) setOnboarded(true);
      if (active) setConfigHydrated(true);
    })();
    return () => {
      active = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // `autoUpdate` used to be persisted but never read. Wait for the native
  // store to hydrate so a stale localStorage value cannot start a check that
  // the persisted setting disabled, then let the singleton scheduler own one
  // startup timer and one daily interval for the whole app.
  useEffect(() => {
    appUpdater.setAutoChecksEnabled(
      configHydrated && config?.autoUpdate === true,
    );
    return () => appUpdater.setAutoChecksEnabled(false);
  }, [configHydrated, config?.autoUpdate]);

  if (!onboarded) {
    return <Onboarding />;
  }

  const Screen = SCREENS[route];

  return (
    <div className="app-shell" data-testid="app-shell">
      <Titlebar />
      <Sidebar />
      <main className="main" data-testid="main">
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
    </div>
  );
}
