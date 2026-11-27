import json
import os
from pathlib import Path
import subprocess
import sys
import unittest

if sys.version_info < (3, 10):
    raise RuntimeError("Run qualification tests with Python 3.10+; use the installed Python 3.12 interpreter on this Mac")

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from coordinator_target import COORDINATORS, coordinator_target


class CoordinatorTargetTests(unittest.TestCase):
    def test_pins_match_recorded_validator_hosts(self):
        root = Path(__file__).resolve().parents[3]
        hosts = json.loads((root / "scripts/release/tests/fixtures/host-config-six-current-20260927.json").read_text())["hosts"]
        for site, (netloc, identity) in COORDINATORS.items():
            host = next(row for row in hosts if row["site"] == site)
            self.assertEqual(netloc, host["ip"])
            self.assertEqual(identity, host["validator"].removeprefix("0x"))

    def test_rejects_arbitrary_origins_and_unknown_sites(self):
        for site in ("", "nyc", "AMS", "https://136.244.109.1", "sgp/path", "sgp "):
            with self.subTest(site=site), self.assertRaisesRegex(ValueError, "pinned sgp or ams"):
                coordinator_target(site)

    def test_ams_process_binds_url_identity_and_rejects_sgp_evidence(self):
        code = """
import community_shard_six_machine as qual
from test_community_shard_six_machine import make_readiness
assert qual.COORDINATOR_SITE == 'ams'
assert qual.COORDINATOR_URL == 'https://136.244.109.1'
assert qual.COORDINATOR_ID == '5772741c93d8a4b04ec39007cb568a31e13ffba0d3e786596d1900d30e529f21'
payload = make_readiness(False)
qual.validate_readiness_payload(payload, expect_ready=False)
payload['coordinator_id'] = 'f521309b041da7aefc742548bdc002c31b47183aacfbbbf245ded09845d0415b'
try:
    qual.validate_readiness_payload(payload, expect_ready=False)
except qual.QualificationError as error:
    assert 'coordinator_id pin mismatch' in str(error)
else:
    raise AssertionError('AMS must reject SGP readiness')
"""
        directory = Path(__file__).resolve().parent
        env = dict(os.environ, ARC_SHARD_QUAL_SITE="ams", PYTHONPATH=os.pathsep.join((str(directory.parent), str(directory))), PYTHONDONTWRITEBYTECODE="1")
        subprocess.run([sys.executable, "-B", "-c", code], env=env, check=True, timeout=20)


if __name__ == "__main__":
    unittest.main()
