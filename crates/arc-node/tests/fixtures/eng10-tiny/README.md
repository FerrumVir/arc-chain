# Deterministic ENG-10 fixture

Generated with tools from **7a952ec7a4d00f1f0973c1c8be8e81fb5f874568**.
This is a synthetic packed checkpoint, not Kimi weights or a performance claim.
The complete plain manifest and 18 content-addressed slices total about 250 KiB;
the pending YaRN manifest is included without a second copy of the slices.
The two `trusted-*-blake3.txt` files are independently fixed test trust inputs,
not values that production callers should read from an untrusted manifest.

**Executable consent is stop/restart only.** Revoke the running local worker
by stopping its process (Ctrl-C/SIGINT gracefully closes transfers and the
listener). Save `host_slices = false` to refuse its next startup. Editing that
setting alone does not stop a running worker: there is **no live consent toggle,
config watcher, RPC or desktop control**. Library consent handles are not
controls exposed by the executable. On Unix, SIGTERM uses the OS default
termination behavior; interrupted `.part` files remain untrusted resume state.

From the repository root, with Python NumPy and BLAKE3 installed:

```sh
cargo build --locked -p arc-inference --bin arc-mla
python3 scripts/arc_mla/make_tiny_kimi_packed.py .task-tmp/eng10-plain
target/debug/arc-mla slice --source-dir .task-tmp/eng10-plain --source-manifest .task-tmp/eng10-plain/tiny-kimi-packed.source.json --out-dir .task-tmp/eng10-slices --expert-groups 4 --threads 1
target/debug/arc-mla slice-manifest --source-dir .task-tmp/eng10-plain --source-manifest .task-tmp/eng10-plain/tiny-kimi-packed.source.json --out-dir .task-tmp/eng10-slices --expert-groups 4 --out .task-tmp/eng10-slices/manifest.json
python3 scripts/arc_mla/make_tiny_kimi_packed.py .task-tmp/eng10-yarn --yarn
target/debug/arc-mla slice --source-dir .task-tmp/eng10-yarn --source-manifest .task-tmp/eng10-yarn/tiny-kimi-packed.source.json --out-dir .task-tmp/eng10-yarn-slices --expert-groups 4 --threads 1
target/debug/arc-mla slice-manifest --source-dir .task-tmp/eng10-yarn --source-manifest .task-tmp/eng10-yarn/tiny-kimi-packed.source.json --out-dir .task-tmp/eng10-yarn-slices --expert-groups 4 --out .task-tmp/eng10-yarn-slices/manifest.json
```

Copy `eng10-slices/manifest.json` and its `*.slice` files here. Copy the YaRN
manifest as `pending-yarn-manifest.json`. Unit preparation records and source
shards are intentionally excluded. Expected canonical manifest digests:

- Plain: `665a36ea80a2ba1154c528395a41e0770c077686c6bcbe2431610df6f9cc883b`
- Pending YaRN: `cec485abd4d76ec653e18cb8acb88cff81c6b84b850b226534166262534ff8da`

Run the targeted integration path:

```sh
cargo test --locked -p arc-node --lib slice_distribution
cargo test --locked -p arc-node --test slice_worker_cli
```

The explicit local entry point is `arc-node slice-worker --node-config FILE
--manifest FILE --manifest-blake3 TRUSTED_DIGEST --slice EXACT_NAME --cache DIR
--mirror DIRECTORY_URL --listen 127.0.0.1:PORT`. Repeat `--slice`, `--mirror`,
and `--peer` as needed. Node TOML must contain `[slice_distribution]` with
`host_slices = true`; download/upload byte limits also come from this section.
Omitting consent denies transfers and listener startup. The CLI tests exercise
the actual binary's startup, offline assembly and (on Unix) SIGINT shutdown,
interrupted GET/Range bodies, closed port and disabled-consent restart.

The operator explicitly supplies selection and sources. No ARC-68/69/71
network assignment protocol is assumed, and this entry point never enables
inference. Peer keys and verified cache objects use `blake3-<hex>`; the mirror
adapter maps the pinned record to ENG-10 `<hex>.slice`.

After downloading, explicitly promote a complete stage using the same trusted
manifest and exact selected names:

```sh
arc-node slice-assemble --node-config node.toml --manifest manifest.json \
  --manifest-blake3 TRUSTED_DIGEST --cache cache --stage 1:2 \
  --slice layer.1.core --slice layer.1.experts.0 --slice layer.1.experts.1 \
  --slice layer.1.experts.2 --slice layer.1.experts.3 --output stage.arcspkg
```

This offline command uses `SliceWorker::assemble_cached_stage`: it re-hashes
and copies cached `blake3-<hex>` objects into a private temporary directory as
`<hex>.slice`, runs the upstream assembler, and publishes the package only after
all checks succeed. It does not hard-link mutable cache files or overwrite an
existing output. The output's parent directory must exist. Corrupt/partial
objects, pending profiles and incomplete stage/expert selections cannot publish
an output package. A failure after the upstream writer starts also leaves no
published package. Temporary copies are removed on ordinary return/error;
abrupt process termination can leave private temporary directories, never a
published partial output. Promotion requires temporary disk space for selected
slices plus the package and runs synchronously as an explicit offline action.
The command neither downloads nor activates inference. Authenticated island
assignment, automatic peer discovery and inference activation remain separate
integration work.

`slice_distribution.max_concurrent_serves` bounds combined verification and
serving (default 2, allowed 1–64). Every router clone and Range request shares
the store's limit. Admission has no waiting queue: excess requests receive 503
before file access. A slot remains held through response completion, error or
cancellation. Full-file integrity verification is retained on each request;
inode, length and modification time are not trusted as integrity evidence.
Downloads remain serialized per store. No island fill-time or throughput
claim is made; those require representative measurements.
