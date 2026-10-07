# Verifier-owned audit context

ARC-76's independent review reproduced a native head changing a requested
`Rp64Argmax` to `Argmax`. The former audit trusted the revealed rule and prompt
boundary, so both stages could return `Valid` for the wrong output sequence.

`audit_stage` and `audit_all` now require `&AuditContext` as their fourth
argument. Construct it with `AuditContext::new(&original_request,
&accepted_tokens)` from verifier-owned records. Do not construct it from a
worker's reveal, returned item, or replacement request. Accepted tokens are the
observed transcript, not a claim of already verified arithmetic: the head
audit recomputes each selection from trusted earlier outputs. It also checks
the final emission, which has no subsequent input row.

The context owns snapshots of the request and transcript. Audits refuse empty
or invalid requests, missing/truncated/excess output evidence, continuation
after EOS, and metadata inconsistent with the committed positions. Partial
prefill is auditable with no emitted outputs. Prompt tokens, prompt boundary,
sequence ID and selection must match at every stage; generated input tokens
must match the accepted history. The coordinator rejects returned items that
change the request/input metadata before accepting their output, without
failing other streams in the batch.

The caller must retain the original request, accepted transcript and verifier
ledger together. This fixes the local trust-boundary bypass without signing
infrastructure. It does not authenticate a caller that replaces its own
trusted records, nor add signatures or remote attestation.

## Contract changes and migration

- The audit API has no legacy three-argument or reveal-only fallback.
- `Revealed` now carries `seq`. Reveal frames use wire tag **8**; legacy tag
  **3** is rejected. Upgrade worker and verifier peers together. Mixed versions
  fail rather than silently accepting an unbound reveal.
- Step/Tree, StageCommit and PositionCommit formats, stage-record domain,
  activation/logits/token digests and stage roots are unchanged.
- ARC-71 consumers pinned to an older #167 revision must migrate both the audit
  calls and reveal peers when advancing their pin. This change does not edit
  #166 or change its pin.
- ENG-1 must retain a distinct trusted context for each request in a batch.
  ENG-9 transports the new reveal layout while keeping verifier evidence
  separate from worker responses. ENG-8 must retain the accepted path and
  original request when auditing generation through the Step path; tree
  transport results alone do not supply trusted history or an acceptance
  policy. The existing tree interface is not a fused speculative kernel.

## Verification

The shared fixture `tests/support/audit_context_cases.rs` runs against native
workers and separate TCP worker processes. It reproduces the original
12-output attack, checks `RequestMismatch` for rule substitution, and restores
the claimed rule to expose `WrongToken` at position 9 (committed 14, expected
12). It independently forges sequence, prompt boundary, prompt tokens and
generated history, and refuses missing transcripts and missing/duplicate
reveals. Honest neighbours, incomplete prefill, `LogitsMismatch` and the final
emission comparison are controls. Existing split/batch/failover and golden
tests remain in place.

The regional-swarm scope and historical measurement provenance are unchanged.
This audit repair does not complete Kimi-scale regional acceptance or establish
a Kimi speed claim; the CI regional fixture remains synthetic.
