# Public ENG-8 sample

Reproduce from the repository root (Python 3, `pyarrow` and `tokenizers`):

```sh
python scripts/tree_speculation/sample.py --cache ./public-source-cache
```

The script verifies source and tokenizer SHA-256s before sampling, downloads data
only, and never executes transcript commands. Source cache is not a deliverable
or a model cache. CI consumes the committed sample, not a moving dataset endpoint.
`cases.json` contains the exact sampling population, seed, source IDs, base commits,
content lengths, license labels and predeclared full-tree subset. Every prompt is
complete under its stated context rule, with no length truncation. The model
wrapper adds fixed-date metadata and asks to continue the recorded JSON transcript.

Licensing: SWE-bench code/data MIT, SWE-smith trajectory dataset MIT (pinned
README front matter), OpenAssistant Apache-2.0. Source-code excerpts retain their
upstream licensing: Django/SymPy BSD-3-Clause; SWE-smith source-repository notices
are bundled for every selected repository at its recorded commit. `licenses/manifest.json`
records immutable URLs and hashes. GitHub's `NOASSERTION` classifier on deepdiff,
tomli and ptyprocess is not a missing license: the bundled texts explicitly grant
MIT, MIT and ISC permissions, respectively. Notices are unmodified.

Coding localization uses old hunks of the reference patch (no added lines or tests)
and is explicitly oracle-localized. Agent tasks are synthetic SWE-smith repairs;
rollouts, assistant actions, full tool calls and observations are actual dataset
records, not authored fixtures. The source does not include tool definitions; all
available messages and fields through the first tool response are preserved.
Chat retains whole human conversation prefixes ending with a user turn, strips
user IDs/annotation metadata, and selects at most one prefix per conversation.
This sample measures speculative acceptance, not coding correctness/tool execution,
production frequency or model quality.
