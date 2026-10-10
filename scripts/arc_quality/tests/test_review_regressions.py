"""ARC-74 review reproductions: no model, endpoint, credentials or spend."""
import contextlib
import copy
import io
import json
import math
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

from arc_quality import provenance, reference, report, stats


class Certification(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.policy = json.loads((Path(__file__).parents[1] / 'policy.json').read_text())
        self.items = []
        for group, rule in self.policy['groups'].items():
            for i in range(rule['certification_items']):
                self.items.append({'id': f'{group}/{i}', 'benchmark': rule['members'][i % len(rule['members'])],
                                   'user': 'question', 'gold': 'answer'})
        self.prov = {'model_id': 'test/model', 'model_revision': 'immutable-revision',
                     'weights_sha256': 'a' * 64, 'tokenizer_sha256': 'b' * 64}
        self.arc = {'schema': 'arc.quality-scored.v1',
                    'provenance': {**self.prov, 'run_sha256': ['c' * 64]},
                    'run': [{'sha256': 'c' * 64}],
                    'results': [{'id': it['id'], 'benchmark': it['benchmark'], 'correct': True,
                                 'answer_key': 'A', 'prompt_tokens': [1], 'tokens': [2],
                                 'item_sha256': provenance.digest(it)} for it in self.items]}
        self.ref = copy.deepcopy(self.arc)
        self.ppl = {'schema': 'arc.modern-quality.v1', 'scored_tokens': 1022,
                    'provenance': {**self.prov, 'tokens_sha256': 'd' * 64},
                    'bf16_reference': {'ppl': 6.0, 'scored_tokens': 1022, 'tokens_sha256': 'd' * 64},
                    'integer_engine': {'ppl': 6.0, 'scored_tokens': 1022, 'tokens_sha256': 'd' * 64, 'logits_digest': 'e' * 64},
                    'ppl_delta_percent': 0.0}

    def write(self, name, value):
        path = self.root / name
        path.write_text(json.dumps(value))
        return str(path)

    def compare(self, ppl=True, ref_b=None):
        return report.compare(self.items, [self.write('arc.json', self.arc)],
                              [self.write('ref.json', self.ref)], self.policy,
                              [self.write('refb.json', ref_b)] if ref_b else None,
                              [self.write('ppl.json', self.ppl)] if ppl else None)

    def assert_blocked(self, result, reason):
        self.assertFalse(result['overall']['certifying'])
        self.assertNotEqual(result['overall']['verdict'], 'PASS')
        self.assertTrue(any(reason in b for b in result['overall']['blockers']), result['overall'])

    def test_complete_evidence_passes_under_still_unapproved_policy(self):
        r = self.compare()
        self.assertEqual(r['overall']['verdict'], 'PASS')
        self.assertTrue(r['overall']['certifying'])
        self.assertEqual(r['policy']['status'], 'PROPOSED, not approved by TJ')

    def test_missing_ppl_at_full_certification_sizes(self):
        self.assert_blocked(self.compare(ppl=False), 'perplexity evidence missing')

    def test_prompt_mismatch_even_without_generated_token_ids(self):
        for row in self.ref['results']:
            row['prompt_tokens'] = [99]
            row['tokens'] = None
        r = self.compare()
        self.assert_blocked(r, 'known prompt token mismatch')
        self.assertEqual(r['overall']['verdict'], 'FAIL')
        self.assertEqual(r['pooled']['prompt_alignment']['mismatched'], len(self.items))

    def test_api_alignment_is_explicitly_unavailable(self):
        for row in self.ref['results']:
            row['prompt_tokens'] = row['tokens'] = None
        r = self.compare()
        self.assert_blocked(r, 'alignment unavailable')
        self.assertEqual(r['pooled']['prompt_alignment']['unavailable'], len(self.items))

    def test_missing_item_despite_remaining_certification_size(self):
        it = {'id': 'extra', 'benchmark': 'gsm8k'}
        self.items.append(it)
        self.assert_blocked(self.compare(), 'incomplete item coverage')

    def test_missing_group_and_unexpected_item(self):
        self.items = [i for i in self.items if i['benchmark'] != 'gsm8k']
        self.assert_blocked(self.compare(), 'missing groups')
        self.assert_blocked(self.compare(), 'unexpected scored items')

    def test_missing_benchmark_within_full_size_group(self):
        for item in self.items:
            if item['benchmark'] == 'mbpp':
                item['benchmark'] = 'humaneval'
        for doc in (self.arc, self.ref):
            for row, item in zip(doc['results'], self.items):
                row['benchmark'] = item['benchmark']
                row['item_sha256'] = provenance.digest(item)
        self.assert_blocked(self.compare(), 'incomplete benchmark coverage')

    def test_invalid_prompt_ids_do_not_count_as_matching(self):
        for value in ([], 'same-text', [-1], [True]):
            self.ref['results'][0]['prompt_tokens'] = value
            with self.subTest(value=value), self.assertRaisesRegex(ValueError, 'prompt tokens'):
                self.compare()

    def test_duplicate_input_ids_are_rejected(self):
        self.items.append(self.items[0])
        with self.assertRaisesRegex(ValueError, 'duplicate'):
            self.compare()

    def test_model_tokenizer_and_run_provenance(self):
        for field in ('model_id', 'model_revision', 'weights_sha256', 'tokenizer_sha256', 'run_sha256'):
            with self.subTest(field=field):
                original = self.ref['provenance'][field]
                self.ref['provenance'][field] = ['f' * 64] if field == 'run_sha256' else 'f' * 64
                self.assert_blocked(self.compare(), 'provenance')
                self.ref['provenance'][field] = original
        del self.ref['provenance']
        self.assert_blocked(self.compare(), 'provenance')

    def test_conflicting_runtime_and_ppl_provenance(self):
        self.arc['run'][0]['profile'] = 'actual-arc-profile'
        self.ppl['integer_engine']['profile'] = 'different-profile'
        self.assert_blocked(self.compare(), 'profile provenance mismatch')
        self.arc['run'].append({'sha256': 'f' * 64, 'profile': 'different-profile'})
        self.arc['provenance']['run_sha256'].append('f' * 64)
        self.assert_blocked(self.compare(), 'runtime provenance differs')

    def test_item_content_provenance(self):
        self.items[0]['user'] = 'changed question'
        self.assert_blocked(self.compare(), 'item provenance')

    def test_perplexity_provenance_and_coverage(self):
        self.ppl['provenance']['weights_sha256'] = 'f' * 64
        self.assert_blocked(self.compare(), 'perplexity model provenance')
        self.ppl['provenance'] = self.prov
        self.ppl['integer_engine']['scored_tokens'] = 1000
        self.assert_blocked(self.compare(), 'scored-token coverage')
        self.assert_blocked(self.compare(), 'input provenance')

    def test_perplexity_numbers_are_validated_and_delta_recomputed(self):
        for value in (0, -1, math.inf, math.nan):
            self.ppl['integer_engine']['ppl'] = value
            with self.subTest(value=value), self.assertRaises(ValueError):
                self.compare()
        self.ppl['integer_engine']['ppl'] = 9
        with self.assertRaisesRegex(ValueError, 'delta disagrees'):
            self.compare()

    def test_second_reference_must_cover_same_inputs(self):
        other = copy.deepcopy(self.ref)
        other['results'].pop()
        self.assert_blocked(self.compare(ref_b=other), 'second reference coverage')
        other = copy.deepcopy(self.ref)
        other['results'][0]['prompt_tokens'] = [99]
        self.assert_blocked(self.compare(ref_b=other), 'second reference prompt')


class Budget(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.items = [{'id': 'a', 'system': None, 'user': 'hi', 'max_tokens': 1}]
        self.args = SimpleNamespace(items=[str(self.root / 'items.jsonl')], limit=0, chars_per_token=3,
            price_in_per_mtok=1, price_out_per_mtok=1, budget_usd=0.00002,
            dry_run=False, out=str(self.root / 'api.json'), api_key_env='', extra_body='',
            base_url='https://example.invalid', model='mock', timeout=1, label='mock',
            context_tokens=19, max_attempts=1)
        self.reply = {'choices': [{'message': {'content': 'x'}}],
                      'usage': {'prompt_tokens': 1, 'completion_tokens': 1}}

    def run_mock(self):
        (self.root / 'items.jsonl').write_text(''.join(json.dumps(i) + '\n' for i in self.items))
        return reference.run_openai(self.args)

    def test_million_token_times_ten_override_sends_nothing(self):
        self.args.extra_body = '{"max_tokens":1000000,"n":10}'
        with patch.object(reference, 'post_json') as post, contextlib.redirect_stdout(io.StringIO()):
            with self.assertRaisesRegex(SystemExit, 'protected'):
                self.run_mock()
            post.assert_not_called()

    def test_all_override_fields_fail_closed(self):
        for field in ('model', 'messages', 'n', 'max_tokens', 'max_completion_tokens', 'best_of',
                      'stream', 'temperature', 'tools', 'reasoning_effort', 'vendor_extension'):
            self.args.extra_body = json.dumps({field: 10})
            with self.subTest(field=field), patch.object(reference, 'post_json') as post:
                with contextlib.redirect_stdout(io.StringIO()), self.assertRaises(SystemExit):
                    self.run_mock()
                post.assert_not_called()

    def test_nonfinite_negative_money_rejected_even_dry_run(self):
        self.args.dry_run = True
        for field in ('price_in_per_mtok', 'price_out_per_mtok', 'budget_usd'):
            old = getattr(self.args, field)
            for value in (-1, math.nan, math.inf, -math.inf):
                setattr(self.args, field, value)
                with self.subTest(field=field, value=value), patch.object(reference, 'post_json') as post:
                    with contextlib.redirect_stdout(io.StringIO()), self.assertRaises(SystemExit):
                        self.run_mock()
                    post.assert_not_called()
            setattr(self.args, field, old)

    def test_caps_and_ratio_are_validated(self):
        for field, values in [('context_tokens', (0, -1, 1.5)), ('chars_per_token', (0, -1, math.inf, math.nan)),
                              ('max_attempts', (0, 7)), ('timeout', (0, math.inf))]:
            old = getattr(self.args, field)
            for value in values:
                setattr(self.args, field, value)
                with self.subTest(field=field, value=value), contextlib.redirect_stdout(io.StringIO()):
                    with self.assertRaises(SystemExit):
                        self.run_mock()
            setattr(self.args, field, old)
        for cap in (0, -1, 1.5, 1000000, True):
            self.items[0]['max_tokens'] = cap
            with self.subTest(cap=cap), contextlib.redirect_stdout(io.StringIO()), self.assertRaises(SystemExit):
                self.run_mock()

    def test_estimate_cannot_authorize_a_request(self):
        self.args.context_tokens = None
        with patch.object(reference, 'post_json') as post, contextlib.redirect_stdout(io.StringIO()):
            with self.assertRaisesRegex(SystemExit, 'context-tokens'):
                self.run_mock()
            post.assert_not_called()

    def test_exact_budget_boundary_and_missing_usage(self):
        del self.reply['usage']
        with patch.object(reference, 'post_json', return_value=self.reply) as post:
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(self.run_mock(), 0)
            body = post.call_args.args[1]
            self.assertEqual((body['max_tokens'], body['n']), (1, 1))
        doc = json.loads((self.root / 'api.json').read_text())
        self.assertEqual(float(doc['usage']['reserved_cost_usd']), self.args.budget_usd)
        self.assertIsNone(doc['cases'][0]['prompt_tokens'])
        self.args.budget_usd = 0.000019999999
        with patch.object(reference, 'post_json') as post, contextlib.redirect_stdout(io.StringIO()):
            with self.assertRaises(SystemExit):
                self.run_mock()
            post.assert_not_called()

    def test_retry_needs_another_full_reservation(self):
        self.args.max_attempts = 2
        with patch.object(reference, 'post_json', side_effect=TimeoutError) as post:
            with contextlib.redirect_stdout(io.StringIO()), self.assertRaisesRegex(SystemExit, 'remaining budget'):
                self.run_mock()
            self.assertEqual(post.call_count, 1)
        self.args.budget_usd *= 2
        with patch.object(reference, 'post_json', side_effect=[TimeoutError(), self.reply]) as post:
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(self.run_mock(), 0)
            self.assertEqual(post.call_count, 2)

    def test_retry_cannot_consume_next_items_budget(self):
        self.items.append({**self.items[0], 'id': 'b'})
        self.args.budget_usd *= 2
        self.args.max_attempts = 2
        with patch.object(reference, 'post_json', side_effect=[TimeoutError(), self.reply]) as post:
            with contextlib.redirect_stdout(io.StringIO()), self.assertRaisesRegex(SystemExit, 'remaining budget'):
                self.run_mock()
            self.assertEqual(post.call_count, 2)

    def test_bad_usage_stops_before_next_request(self):
        self.items.append({**self.items[0], 'id': 'b'})
        self.args.budget_usd *= 2
        for usage in ({'prompt_tokens': 20}, {'completion_tokens': 2}, {'prompt_tokens': -1},
                      {'completion_tokens': math.nan}):
            self.reply['usage'] = usage
            with patch.object(reference, 'post_json', return_value=self.reply) as post:
                with contextlib.redirect_stdout(io.StringIO()), self.assertRaises(RuntimeError):
                    self.run_mock()
                self.assertEqual(post.call_count, 1)

    def test_transport_itself_does_not_retry(self):
        with patch.object(reference.urllib.request, 'urlopen', side_effect=TimeoutError) as post:
            with self.assertRaises(TimeoutError):
                reference.post_json('https://example.invalid', {}, {}, 1)
            self.assertEqual(post.call_count, 1)


class LargeMcNemar(unittest.TestCase):
    def test_balanced_extreme_and_asymmetric_counts(self):
        for b, c in ((512, 512), (2000, 2000), (1024, 0), (0, 2000), (600, 500), (2100, 1900)):
            # Independent exact rational oracle: factorial-based combinations.
            m = b + c
            expected = min(1, (2 * sum(math.comb(m, i) for i in range(min(b, c) + 1))) / (1 << m))
            actual = stats.mcnemar_exact(b, c)
            self.assertEqual(actual, expected)
            self.assertTrue(math.isfinite(actual))
            self.assertEqual(actual, stats.mcnemar_exact(c, b))
        self.assertEqual(stats.mcnemar_exact(1024, 0), math.ldexp(1.0, -1023))

    def test_invalid_counts(self):
        for b, c in ((-1, 2), (1.5, 2), (True, 2)):
            with self.assertRaises(ValueError):
                stats.mcnemar_exact(b, c)
