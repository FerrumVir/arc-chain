/** Isolated browser-only fixture. Node 24; no native host, reporting endpoint or external navigation. */
import { createServer } from 'vite';
import react from '@vitejs/plugin-react';
import { fileURLToPath } from 'node:url';
import { installSetupRecoveryFixture } from '../tests/fixtures/setup-recovery.ts';

const port = Number(process.env.PREVIEW_PORT ?? 49420);
if (!Number.isInteger(port) || port < 1024 || port > 65535) throw new Error('PREVIEW_PORT must be 1024–65535');
const server = await createServer({
  root: fileURLToPath(new URL('../', import.meta.url)), configFile: false, envPrefix: [],
  define: { __ARC_PROD_TAURI__: 'false' },
  plugins: [react(), { name: 'synthetic-setup-recovery', transformIndexHtml: () => [{
    tag: 'script', injectTo: 'head-prepend', children: `localStorage.removeItem('arc-desktop-state-v1'); (${installSetupRecoveryFixture.toString()})();`,
  }] }],
  server: { host: '127.0.0.1', port, strictPort: true },
});
await server.listen();
console.log(`Synthetic setup recovery preview: http://127.0.0.1:${port}`);
console.log('Browser console: __ARC_SETUP_RECOVERY__.mode("pending"), then Copy details, then __ARC_SETUP_RECOVERY__.recover().');
console.log('calls() and writes() inspect synthetic IPC and clipboard; reload resets the fixture. Ctrl-C stops this server.');
const stop = async () => { await server.close(); process.exit(0); };
process.on('SIGINT', stop);
process.on('SIGTERM', stop);
