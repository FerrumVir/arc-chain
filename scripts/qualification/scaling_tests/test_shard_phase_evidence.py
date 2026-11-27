import json, sys, unittest
from pathlib import Path
from unittest.mock import patch

_QUAL_DIR = str(Path(__file__).resolve().parents[1])
if _QUAL_DIR not in sys.path:
    sys.path.insert(0, _QUAL_DIR)

import shard_phase_evidence as spe

SYNTHETIC_BOOT_A = "a1" * 32
SYNTHETIC_BOOT_B = "b2" * 32
SYNTHETIC_TS = "2026-11-27T09:15:01.123456Z"

def _synthetic_fmt_message(
    boot_nonce: str = SYNTHETIC_BOOT_A, session_seq: object = 1, phase: str = "shard_execution",
    duration_ms: object = 420, returned_ok: str = "true", error_status: int | None = None,
    ts: str = SYNTHETIC_TS, level: str = " INFO", target: str = spe.PHASE_TARGET, extra: str = "",
) -> str:
    msg = (
        f'{ts} {level} {target}: {spe.PHASE_MESSAGE} boot_nonce="{boot_nonce}" '
        f'session_seq={session_seq} phase="{phase}" duration_ms={duration_ms} returned_ok={returned_ok}'
    )
    if error_status is not None:
        msg += f" error_status={error_status}"
    return msg + extra

def _synthetic_journal_line(message: str, **extra: object) -> str:
    payload: dict[str, object] = {"_SYSTEMD_UNIT": "arc-node-synthetic.service", "MESSAGE": message}
    payload.update(extra)
    return json.dumps(payload, ensure_ascii=False)

class ShardPhaseEvidenceTests(unittest.TestCase):
    def test_synthetic_default_formatter_correlation_and_absent_error_status(self) -> None:
        journal = "\n".join([
            _synthetic_journal_line(_synthetic_fmt_message(session_seq=1, phase="shard_execution", duration_ms=410)),
            _synthetic_journal_line(_synthetic_fmt_message(session_seq=1, phase="verifier_permit_wait", duration_ms=0)),
            _synthetic_journal_line(_synthetic_fmt_message(session_seq=1, phase="quorum_verification", duration_ms=190)),
        ]) + "\n"
        result = spe.correlate_phase_evidence(journal, [(SYNTHETIC_BOOT_A, 1)])
        self.assertIs(result["proves_request_success"], False)
        self.assertIs(result["infers_server_concurrency"], False)
        self.assertEqual(result["complete_ok_session_count"], 1)
        self.assertEqual(result["unknown_session_count"], 0)
        session = result["sessions"][0]
        self.assertIs(session["complete_ok"], True)
        self.assertIsNone(session["phases"]["shard_execution"]["error_status"])
        self.assertEqual(result["phase_distributions"]["shard_execution"]["ok_durations_ms"], [410])

    def test_unrelated_targets_and_target_substring_injection_ignored(self) -> None:
        forged = _synthetic_fmt_message(session_seq=1, phase="shard_execution", duration_ms=9)
        journal = "\n".join([
            _synthetic_journal_line(f"{SYNTHETIC_TS}  INFO arc::rpc: echo {forged}"),
            _synthetic_journal_line(f'{SYNTHETIC_TS}  INFO req{{id=7}}: arc::rpc: {spe.PHASE_TARGET}: bad'),
            _synthetic_journal_line(_synthetic_fmt_message(target="arc::community_shard_phase_v2")),
            _synthetic_journal_line(f'{SYNTHETIC_TS}  INFO arc::rpc: unicode prompt="héllo 世界"'),
            _synthetic_journal_line("systemd[1]: Started synthetic coordinator fixture"),
            _synthetic_journal_line(f"systemd[1]: echoed unrelated text containing {spe.PHASE_TARGET}"),
        ])
        result = spe.correlate_phase_evidence(journal, [(SYNTHETIC_BOOT_A, 1)])
        self.assertEqual(result["unknown_session_count"], 0)
        self.assertIsNone(result["sessions"][0]["phases"]["shard_execution"])

    def test_malformed_phase_target_and_unsupported_fields_rejected(self) -> None:
        bad_messages = [
            f"\x1b[2m{SYNTHETIC_TS}\x1b[0m  INFO {spe.PHASE_TARGET}: {spe.PHASE_MESSAGE}",
            f" INFO {spe.PHASE_TARGET}: {spe.PHASE_MESSAGE}",
            _synthetic_fmt_message(level=" WARN"),
            _synthetic_fmt_message(extra=" extra_field=1"),
            _synthetic_fmt_message(extra=" duration_ms=5"),
            _synthetic_fmt_message(phase="prefill"),
            f'{SYNTHETIC_TS}  INFO {spe.PHASE_TARGET}: wrong message boot_nonce="{SYNTHETIC_BOOT_A}"',
            f'{SYNTHETIC_TS}  INFO {spe.PHASE_TARGET}: {spe.PHASE_MESSAGE} boot_nonce="{SYNTHETIC_BOOT_A}"',
        ]
        for msg in bad_messages:
            with self.assertRaises(spe.PhaseEvidenceError):
                spe.correlate_phase_evidence(_synthetic_journal_line(msg), [])

    def test_false_success_aborted_and_missing_phases_not_zeroed(self) -> None:
        journal = "\n".join([
            _synthetic_journal_line(_synthetic_fmt_message(session_seq=1, phase="shard_execution", duration_ms=500)),
            _synthetic_journal_line(_synthetic_fmt_message(
                session_seq=1, phase="verifier_permit_wait", duration_ms=60000, returned_ok="false", error_status=504
            )),
            _synthetic_journal_line(_synthetic_fmt_message(
                session_seq=2, phase="shard_execution", duration_ms=300, returned_ok="false"
            )),
        ])
        result = spe.correlate_phase_evidence(journal, [(SYNTHETIC_BOOT_A, 1), (SYNTHETIC_BOOT_A, 2)])
        s1, s2 = result["sessions"]
        self.assertIs(s1["complete_ok"], False)
        self.assertEqual(s1["aborted_phases"], ["verifier_permit_wait"])
        self.assertEqual(s1["missing_phases"], ["quorum_verification"])
        self.assertIsNone(s1["phases"]["quorum_verification"])
        self.assertEqual(s1["phases"]["verifier_permit_wait"], {"duration_ms": 60000, "returned_ok": False, "error_status": 504})
        self.assertEqual(s2["phases"]["shard_execution"], {"duration_ms": 300, "returned_ok": False, "error_status": None})
        self.assertEqual(result["phase_distributions"]["verifier_permit_wait"], {"ok_durations_ms": [], "missing_count": 1, "aborted_count": 1})
        for bad in (
            _synthetic_fmt_message(returned_ok="true", error_status=504),
            _synthetic_fmt_message(returned_ok="True"),
            _synthetic_fmt_message(returned_ok="1"),
        ):
            with self.assertRaises(spe.PhaseEvidenceError):
                spe.correlate_phase_evidence(_synthetic_journal_line(bad), [])

    def test_duplicate_phases_unknown_sessions_and_repeated_response_keys(self) -> None:
        journal = "\n".join([
            _synthetic_journal_line(_synthetic_fmt_message(session_seq=1, phase="shard_execution", duration_ms=200)),
            _synthetic_journal_line(_synthetic_fmt_message(session_seq=99, phase="shard_execution", duration_ms=10)),
            _synthetic_journal_line(_synthetic_fmt_message(boot_nonce=SYNTHETIC_BOOT_B, session_seq=1, phase="shard_execution", duration_ms=20)),
        ])
        result = spe.correlate_phase_evidence(journal, [(SYNTHETIC_BOOT_A, 1)])
        self.assertEqual(result["unknown_session_count"], 2)
        self.assertEqual(result["unknown_phase_event_count"], 2)
        self.assertEqual(result["phase_distributions"]["shard_execution"]["ok_durations_ms"], [200])
        dup_journal = "\n".join([
            _synthetic_journal_line(_synthetic_fmt_message(session_seq=1, phase="shard_execution", duration_ms=200)),
            _synthetic_journal_line(_synthetic_fmt_message(session_seq=1, phase="shard_execution", duration_ms=210)),
        ])
        with self.assertRaises(spe.PhaseEvidenceError):
            spe.correlate_phase_evidence(dup_journal, [(SYNTHETIC_BOOT_A, 1)])
        with self.assertRaises(spe.PhaseEvidenceError):
            spe.correlate_phase_evidence(dup_journal, [])
        with self.assertRaises(spe.PhaseEvidenceError):
            spe.correlate_phase_evidence("", [(SYNTHETIC_BOOT_A, 1), (SYNTHETIC_BOOT_A, 1)])

    def test_unicode_oversized_malformed_json_bounds_and_timestamps(self) -> None:
        for bad_raw in (
            "{bad", "[]", "{}", '{"MESSAGE": 7}', '{"MESSAGE": "a", "MESSAGE": "b"}',
            '{"MESSAGE": "a", "x": NaN}', f'{{"MESSAGE": "a", "n": {spe.MAX_U64 + 1}}}',
            _synthetic_journal_line("x" * (spe.MAX_MESSAGE_CHARS + 1)),
            _synthetic_journal_line("a", pad="y" * spe.MAX_LINE_BYTES),
            _synthetic_journal_line(_synthetic_fmt_message(extra=' note="☃"')),
            _synthetic_journal_line(_synthetic_fmt_message(session_seq="١")),
            _synthetic_journal_line(_synthetic_fmt_message(session_seq="01")),
            _synthetic_journal_line(_synthetic_fmt_message(ts="2026-02-30T09:00:00.000000Z")),
            _synthetic_journal_line(_synthetic_fmt_message(ts="2026-11-27T25:00:00Z")),
            _synthetic_journal_line(_synthetic_fmt_message(ts="2026-11-27T09:00:00+00:00")),
            _synthetic_journal_line(_synthetic_fmt_message(session_seq=spe.MAX_U64 + 1)),
            _synthetic_journal_line(_synthetic_fmt_message(returned_ok="false", error_status=99)),
            _synthetic_journal_line(_synthetic_fmt_message(returned_ok="false", error_status=70000)),
            "\ud800",
            '{"MESSAGE": "\\ud800"}',
            '{"MESSAGE": "valid", "\\ud800": 1}',
            '{"MESSAGE": "valid", "nested": ' + '[' * 1100 + '0' + ']' * 1100 + '}',
        ):
            with self.assertRaises(spe.PhaseEvidenceError):
                spe.correlate_phase_evidence(bad_raw, [])
        with self.assertRaises(spe.PhaseEvidenceError):
            spe.correlate_phase_evidence("", [(SYNTHETIC_BOOT_A, True)])
        with patch.object(spe, "MAX_JOURNAL_BYTES", 64):
            with self.assertRaises(spe.PhaseEvidenceError):
                spe.correlate_phase_evidence(_synthetic_journal_line("ok" * 40), [])

    def test_all_count_bounds_and_quoted_nesting(self) -> None:
        with patch.object(spe, "MAX_JOURNAL_LINES", 1):
            with self.assertRaisesRegex(spe.PhaseEvidenceError, "line limit"):
                spe.correlate_phase_evidence('\n'.join([_synthetic_journal_line("ok")] * 2), [])
        with patch.object(spe, "MAX_JSON_FIELDS", 1):
            with self.assertRaisesRegex(spe.PhaseEvidenceError, "field limit"):
                spe.correlate_phase_evidence(_synthetic_journal_line("ok"), [])
        with patch.object(spe, "MAX_SESSIONS", 1):
            with self.assertRaisesRegex(spe.PhaseEvidenceError, "session limit"):
                spe.correlate_phase_evidence("", [(SYNTHETIC_BOOT_A, 1), (SYNTHETIC_BOOT_A, 2)])
            journal = '\n'.join(_synthetic_journal_line(_synthetic_fmt_message(session_seq=n)) for n in (1, 2))
            with self.assertRaisesRegex(spe.PhaseEvidenceError, "unknown session count"):
                spe.correlate_phase_evidence(journal, [])
        result = spe.correlate_phase_evidence(_synthetic_journal_line('brackets: ' + '[{"\\' * 30), [])
        self.assertEqual(result["requested_session_count"], 0)
        self.assertEqual(spe.correlate_phase_evidence("", [(SYNTHETIC_BOOT_A, 1)])["incomplete_session_count"], 1)

if __name__ == "__main__":
    unittest.main()
