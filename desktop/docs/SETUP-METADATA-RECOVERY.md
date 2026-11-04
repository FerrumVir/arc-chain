# Synthetic setup metadata recovery review

The reviewed #226 head `42177188b5e21593010740b0e6bc9513a547791f` only retried Get help metadata on Copy
details when an error existed. Setup without an error therefore kept unknown versions/system after a failed
lookup while it stayed mounted. The new real-component regression fails at that head: switching the synthetic
metadata service to recovery and pressing Copy details leaves its call count unchanged.

Get help now requests recovery whenever its privacy context is not ready, through the existing shared hook.
The hook makes one initial query attempt, disables query automatic retry/retry-on-remount/reconnect recovery,
and allows a later Copy details action to retry. The application already disables window-focus refetch.
An in-flight request is reused, including clicks in the same render turn with previously cached empty data;
it is not canceled and restarted. This deduplication belongs to the privacy query. Node Care's independent
initial platform lookup is unchanged; it is not merged with that query.

The click copies the details rendered at that moment. It does not wait for metadata, silently recopy after
recovery, or add a clipboard operation on resolution. Without a privacy-ready home directory, native text
remains omitted. On recovery the displayed payload is recomputed, and a later copy may include scrubbed
details. Setup without an error still contains no native diagnostic text. Missing/denied clipboard access
retains the existing selectable details and failure wording. No public strings, reporting, collection,
approval, sends or endpoint policy change. Metadata readiness does not establish report delivery.

## Isolated preview

In this checkout, with Node 24 and pinned dependencies installed using `npm ci`:

```sh
cd desktop
node scripts/preview-setup-recovery.mjs
```

The URL is `http://127.0.0.1:49420`; set `PREVIEW_PORT` to choose another port. The preview uses the actual
React app and existing browser mock seam, with a synthetic metadata/clipboard fixture. It has no native
host, no report endpoint and no real identifiers. External-link commands reject. Its Vite config does not
expose environment variables. Do not connect a Tauri host or use production data with this fixture.

1. Open Get help on the welcome step. Metadata initially rejects; App/System remain unknown.
2. In this page's browser console run `__ARC_SETUP_RECOVERY__.mode("pending")`.
3. Click Copy details. The preview's fake clipboard contains the safe summary; metadata stays pending.
   Click again: `__ARC_SETUP_RECOVERY__.calls()` must not increase again while pending.
4. Run `__ARC_SETUP_RECOVERY__.recover()`. The existing mounted panel shows app 0.8.16 and macos aarch64.
   `__ARC_SETUP_RECOVERY__.writes()` still holds the earlier summaries; recovery alone does not copy.
5. Click Copy details again. Its new captured payload includes the recovered metadata. No home path appears.

The fixture methods `mode("reject")`, `mode("empty")` and `reject()` permit rejected/empty retries. Reload
resets the fixture and setup state for this preview origin. The preview clipboard is in-memory, not the OS
clipboard. Do not follow external destinations. Ctrl-C/SIGTERM closes the server; close the browser context
to discard fixture state. There is no reporting database or node process to clean up.

## Tests and inventory

```sh
cd desktop
npx playwright test setup-metadata-recovery.spec.ts get-help.spec.ts help-entry-points.spec.ts share-privacy.spec.ts
npx playwright test --list
```

The six new scenarios cover rejected/pending/recovered metadata, setup without an error, initial pending
deduplication, rejected and empty retries, clipboard rejection/absence, and error-bearing Get help with
native text omitted until it can be scrubbed. They check same-turn duplicate clicks and preservation of the
current clipboard, not just visible version strings. The existing share-privacy matrix remains in the run.

The collector recount on #226 plus this follow-up is **455 tests in 35 files** (was 449/34). This is collection,
not a full-suite result. README and the existing documentation contract bind that same count. It is not the
inventory of a merge with #229/#230 or other branches; integration must recount the actual combined tree.
Independent code review and Browser verification remain required. All policy and rollout gates remain open.
