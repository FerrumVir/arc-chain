#!/usr/bin/env python3
"""Offline CLI checks; segment hashes below are synthetic metadata, never weights."""
import copy
import json
import subprocess
import sys
from pathlib import Path
import blake3


def main():
    binary, out = Path(sys.argv[1]).resolve(), Path(sys.argv[2]).resolve()
    out.mkdir(parents=True, exist_ok=True)
    repo = Path(__file__).resolve().parents[2]
    ref = repo / 'docs/protocol/reference/kimi-k26'
    config = ref / 'config.json'
    source = json.loads((ref / 'source.json').read_text())
    source = {k: source[k] for k in ['repo', 'revision', 'files']}
    historical = {'schema': 'arc.integer-slice-manifest.v1',
                  'profile': 'arc.hf-deepseek-v3.mla-moe.i8-dyadic-row.i4g32-experts.q16.v1',
                  'pending': ['rope_scaling yarn'], 'model': None, 'tables': None, 'model_root': None,
                  'weights': {'prefix': 'language_model.', 'packed_experts': True}, 'source': source,
                  'segments': [{'name': n, 'bytes': size, 'blake3': 'a' * 64} for n, size in [
                      ('embed', 1175224320), ('layer.0', 498147648),
                      ('layer.1', 9663852224), ('head', 1175281664)]]}
    wide = dict(version=1, **{k: 'int16' for k in ['attention', 'dense', 'shared', 'embedding', 'head']})
    policy = out / 'int16-policy.json'
    policy.write_text(json.dumps(wide) + '\n')
    pending = copy.deepcopy(historical)
    pending['precision'] = wide
    pending['profile'] = 'arc.hf-deepseek-v3.mla-moe.mixed-dyadic-row.i4g32-experts.q16.v1'
    # Metadata only: each promoted BF16 matrix adds one byte per element.
    # Native experts, norms, router and per-row scale metadata do not change.
    # Compute the deltas from pinned source dimensions, not the engine layout.
    c = json.loads(config.read_text())['text_config']
    d, v, h, r, q, n, p, vh = (c[k] for k in (
        'hidden_size', 'vocab_size', 'num_attention_heads', 'kv_lora_rank',
        'q_lora_rank', 'qk_nope_head_dim', 'qk_rope_head_dim', 'v_head_dim'))
    attention = q*d + h*(n+p)*q + (r+p)*d + h*r*n + h*vh*r + d*h*vh
    extra = {'embed': v*d, 'head': v*d,
             'layer.0': attention + 3*d*c['intermediate_size'],
             'layer.1': attention + 3*d*c['moe_intermediate_size']*c['n_shared_experts']}
    for segment in pending['segments']:
        segment['bytes'] += extra[segment['name']]

    def write_pending(name, value):
        value = copy.deepcopy(value)
        value['manifest_blake3'] = blake3.blake3(
            json.dumps(value, sort_keys=True, separators=(',', ':')).encode()).hexdigest()
        path = out / name
        path.write_text(json.dumps(value) + '\n')
        return path

    results = []
    def run(name, args, ok, error=''):
        report = out / (name + '.json')
        assert not report.exists(), 'use a fresh evidence directory'
        result = subprocess.run([str(binary), 'yarn-prepare', '--config', str(config),
                                 '--out', str(report), *map(str, args)], capture_output=True, text=True)
        # Save failed assertions too, for a reproducible red/green diagnosis.
        (out / (name + '.log')).write_text(result.stdout + result.stderr)
        assert (result.returncode == 0) == ok, (name, result.stderr)
        if not ok:
            assert error in result.stderr, (name, result.stderr)
            assert not report.exists(), name
            if '--manifest-out' in args:
                assert not Path(args[args.index('--manifest-out') + 1]).exists(), name
        results.append({'name': name, 'exit_code': result.returncode, 'expected_success': ok})
        return json.loads(report.read_text()) if ok else None

    controls = [('default', pending, [], wide),
                ('explicit-int16', pending, ['--precision', policy], wide),
                ('historical-int8', historical, ['--historical-int8'], None)]
    reports, manifests = {}, {}
    pins = {}
    for label, fixture, flags, expected in controls:
        full = run('prepare-full-' + label, flags, True)
        assert full['model'].get('precision') == expected
        assert full['model']['preparation']['scope'] == {'kind': 'full_kimi_k26'}
        assert full['weight_bytes_verified'] is False and full['real_forward_measured'] is False
        pin = write_pending('pending-' + label + '.json', fixture)
        pins[label] = pin
        before = pin.read_bytes()
        target = out / ('probe-manifest-' + label + '.json')
        probe = run('prepare-probe-' + label,
                    ['--probe-layers', '2', '--slice-manifest', pin, '--manifest-out', target, *flags], True)
        m = json.loads(target.read_text())
        assert m['profile'].startswith('arc.experimental.')
        assert m['model']['n_layers'] == 2 and len(m['segments']) == 5
        assert m['model'].get('precision') == expected
        assert m['segments'][1:] == fixture['segments'], 'weight commitments changed'
        assert m['source'] == source and pin.read_bytes() == before
        reports[label] = (full, probe)
        manifests[label] = m
        run('incomplete-full-' + label,
            ['--slice-manifest', pin, '--manifest-out', out / ('missing-' + label + '.json'), *flags],
            False, 'missing selected segment')
        for kind in ['source', 'pending-cleared', 'policy-substituted', 'size-substituted']:
            bad = copy.deepcopy(fixture)
            error = 'not a valid pending K2.6 weight manifest'
            if kind == 'source':
                bad['source']['revision'] = 'forged'
                error = 'pinned K2.6 source'
            elif kind == 'pending-cleared':
                bad['pending'] = []
            elif kind == 'policy-substituted':
                if expected is None: bad['precision'] = wide
                else: bad['precision']['head'] = 'int8'
            else:
                bad['segments'][0]['bytes'] += 1
                error = 'invalid/duplicate source weight segment'
            badpin = write_pending('input-' + label + '-' + kind + '.json', bad)
            run(label + '-' + kind,
                ['--probe-layers', '2', '--slice-manifest', badpin,
                 '--manifest-out', out / (label + '-' + kind + '-manifest.json'), *flags], False, error)

    assert reports['default'] == reports['explicit-int16']
    assert manifests['default'] == manifests['explicit-int16']
    assert manifests['default']['model_root'] != manifests['historical-int8']['model_root']
    # Legacy data without explicit selection must fail; adding a policy label
    # alone is insufficient because the actual declared segment sizes differ.
    for label, pin, flags in [
        ('legacy-under-default', pins['historical-int8'], []),
        ('int16-under-historical', pins['default'], ['--historical-int8']),
    ]:
        run(label, ['--probe-layers', '2', '--slice-manifest', pin,
                    '--manifest-out', out / (label + '-manifest.json'), *flags],
            False, 'not a valid pending K2.6 weight manifest')
    relabelled = copy.deepcopy(historical)
    relabelled['profile'] = pending['profile']
    relabelled['precision'] = wide
    pin = write_pending('input-relabelled.json', relabelled)
    run('no-identity-relabel', ['--probe-layers', '2', '--slice-manifest', pin,
                              '--manifest-out', out / 'relabelled-manifest.json'],
        False, 'invalid/duplicate source weight segment')
    run('missing-probe-value', ['--probe-layers'], False, 'missing --probe-layers')
    run('bad-probe', ['--probe-layers', '61'], False, '1..=3')
    run('bad-context', ['--max-seq', '262145'], False, 'max_seq')
    (out / 'summary.json').write_text(json.dumps({
        'scope': 'CLI and synthetic commitment metadata; no weight validation or real execution',
        'results': results, 'pass': True, 'default_equals_explicit_int16': True,
        'probe_roots': {label: m['model_root'] for label, m in manifests.items()},
    }, indent=2) + '\n')
    print(json.dumps(results, indent=2))


if __name__ == '__main__':
    main()
