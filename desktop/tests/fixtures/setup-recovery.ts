/** Browser-only synthetic IPC/clipboard controls, shared by tests and the isolated preview. */
export function installSetupRecoveryFixture(options: { pending?: boolean; clipboard?: "ok" | "fail" | "absent"; diagnostic?: boolean } = {}) {
  const home = "/Volumes/Data/setup-recovery-fixture";
  const platform = { endpointConfigured: false, appVersion: "0.8.16", os: "macos", arch: "aarch64", homeDir: home };
  let mode = options.pending ? "pending" : "reject";
  let calls = 0;
  const pending: { resolve: (value: unknown) => void; reject: (error: Error) => void }[] = [];
  const writes: string[] = [];
  const mock: Record<string, unknown> = {
    open_external: { __reject: "synthetic preview: external navigation disabled" },
  };
  if (options.diagnostic) mock.node_start_block = `ARC refused to start the node: cannot lock ${home}/.arc/data-v3/.arc-node.lock (os error 35)`;
  Object.defineProperty(mock, "node_care_info", { enumerable: true, get: () => {
    calls++;
    if (mode === "reject") return Promise.reject(new Error("synthetic metadata lookup rejected"));
    if (mode === "ready") return platform;
    if (mode === "empty") return { ...platform, homeDir: "" };
    return new Promise((resolve, reject) => pending.push({ resolve, reject }));
  } });
  Object.defineProperty(navigator, "clipboard", { configurable: true, value: options.clipboard === "absent" ? undefined : {
    writeText: (text: string) => {
      writes.push(text);
      return options.clipboard === "fail" ? Promise.reject(new Error("synthetic clipboard denied")) : Promise.resolve();
    },
    readText: () => Promise.resolve(writes.at(-1) ?? ""),
  } });
  const control = {
    calls: () => calls, writes: () => [...writes], pending: () => pending.length,
    mode: (value: "reject" | "pending" | "ready" | "empty") => { mode = value; },
    recover: () => { mode = "ready"; pending.splice(0).forEach((p) => p.resolve(platform)); },
    reject: () => { mode = "reject"; pending.splice(0).forEach((p) => p.reject(new Error("synthetic retry rejected"))); },
  };
  Object.assign(window, { __ARC_MOCK__: mock, __ARC_SETUP_RECOVERY__: control });
}
