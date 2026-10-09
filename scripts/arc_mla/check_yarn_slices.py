#!/usr/bin/env python3
"""Offline packed-fixture -> verified YaRN bundle -> scalar/SIMD/stage execution."""
import copy
import json
import shutil
import subprocess
import sys
from pathlib import Path
import blake3


def read(p):
    return json.loads(p.read_text())


def main():
    binary, work, evidence = (Path(p).resolve() for p in sys.argv[1:4])
    policy = Path(sys.argv[4]).resolve() if len(sys.argv) == 5 and sys.argv[4] != "historical-int8" else None
    historical = len(sys.argv) == 5 and sys.argv[4] == "historical-int8"
    effective = read(policy) if policy else (None if historical else dict(version=1, **{k:"int16" for k in ("attention","dense","shared","embedding","head")}))
    settings = ["--precision", policy] if policy else (["--historical-int8"] if historical else [])
    root = Path(__file__).resolve().parents[2]
    work.mkdir(parents=True)
    evidence.mkdir(parents=True)
    source, slices = work / 'source', work / 'slices'
    config = source / 'config.json'
    pin = source / 'tiny-kimi-packed.source.json'
    manifest = work / 'slices.json'
    commands = evidence / 'commands.log'

    def run(*args, fails=False):
        r = subprocess.run([str(binary), *map(str, args)], capture_output=True, text=True)
        with commands.open('a', encoding='utf-8') as f:
            f.write(f'{list(map(str,args))}\nexit={r.returncode}\n{r.stdout}{r.stderr}\n')
        assert (r.returncode != 0) if fails else (r.returncode == 0), (args, r.stdout, r.stderr)
        return r

    subprocess.run([sys.executable, str(root / 'scripts/arc_mla/make_tiny_kimi_packed.py'),
                    str(source), '--prepared', '--edge'], check=True, stdout=subprocess.DEVNULL)
    # No vision shard is needed by the selected text path.
    index = read(source / 'model.safetensors.index.json')['weight_map']
    vision = {f for n, f in index.items() if n.startswith(('vision_tower.', 'mm_projector.'))}
    for f in vision:
        (source / f).unlink()
    run('slice', '--source-dir', source, '--source-manifest', pin, '--out-dir', slices,
        '--expert-groups', 4, *settings)
    run('slice-manifest', '--source-dir', source, '--source-manifest', pin, '--out-dir', slices,
        '--expert-groups', 4, '--out', manifest, *settings)
    original = read(manifest)
    assert original['pending'] == ['rope_scaling yarn']
    run('slice-assemble', '--manifest', manifest, '--slices', slices, '--out', work / 'legacy.pkg', *settings, fails=True)
    assert not (work / 'legacy.pkg').exists()

    def assemble(out, m=manifest, src=pin, cfg=config, *extra, fails=False):
        return run('slice-assemble-yarn', '--config', cfg, '--source-manifest', src,
                   '--manifest', m, '--slices', slices, '--out-dir', out, '--fixture', *settings, *extra, fails=fails)

    full = work / 'bundle-1'
    assemble(full)
    finalized = read(full / 'manifest.json')
    assert finalized['profile'] == ('arc.synthetic.kimi-k26-yarn.mixed-dyadic-row.i4g32.q16.v1' if effective else 'arc.synthetic.kimi-k26-yarn.i4g32.q16.v1')
    assert finalized['model'].get('precision') == effective
    assert read(manifest) == original, 'pending input changed'
    assert finalized['segments'][1:] == original['segments'], 'weight bytes requantized'
    run('verify', '--package', full / 'stage-0.arcspkg', '--manifest', full / 'manifest.json')
    runs = []
    for kernel, threads in [('scalar', 1), ('simd', 3)]:
        out = evidence / f'{kernel}.json'
        run('golden', '--package', full / 'stage-0.arcspkg', '--cases', source / 'tiny-kimi-packed.cases.json',
            '--out', out, '--kernel', kernel, '--threads', threads)
        runs.append(read(out))
    keys = ('tokens', 'output_hash', 'logits_hashes', 'logits_digest', 'boundary_digests')
    assert runs[0]['model_root'] == runs[1]['model_root'] == finalized['model_root']
    assert len(runs[0]['cases']) == len(runs[1]['cases']) > 0
    assert all(a[k] == b[k] for a,b in zip(runs[0]['cases'], runs[1]['cases']) for k in keys)
    layouts = []
    for count in (1,2,4):
        bundle = work / f'bundle-{count}'
        if count != 1:
            assemble(bundle, manifest, pin, config, '--stages', count)
        assert read(bundle / 'manifest.json') == finalized
        previous, reports = None, []
        for i in range(count):
            report, boundary = evidence / f'{count}-{i}.json', work / f'{count}-{i}.bin'
            run('stage', '--package', bundle / f'stage-{i}.arcspkg', '--manifest', bundle / 'manifest.json',
                *(('--input', previous) if previous else ('--run', evidence / 'scalar.json')),
                '--report', report, '--out', boundary, '--kernel', 'simd' if i % 2 else 'scalar', '--threads', 2)
            reports.append(report); previous = boundary
        layouts.extend(['--layout', f'{count}=' + ','.join(map(str,reports))])
    subprocess.run([sys.executable, str(root / 'scripts/arc_mla/layout_check.py'), '--run', str(evidence / 'scalar.json'),
                    '--label', 'synthetic packed canonical YaRN; precision=' + json.dumps(effective, sort_keys=True), *layouts, '--out', str(evidence / 'layouts.json'),
                    '--summary-md', str(evidence / 'layouts.md')], check=True)

    failures = []
    target = next(s for s in original['slices'] if '.experts.' in s['name'])
    path = slices / (target['blake3'] + '.slice')
    saved = path.read_bytes()
    for kind in ('altered', 'truncated', 'appended', 'missing'):
        out = work / f'bad-{kind}'
        try:
            if kind == 'missing': path.unlink()
            else: path.write_bytes({'altered':bytes([saved[0]^1])+saved[1:], 'truncated':saved[:-1], 'appended':saved+b'x'}[kind])
            assemble(out, fails=True)
            assert not out.exists()
            assert not list(work.glob('.yarn-assembly-*'))
            failures.append(kind)
        finally: path.write_bytes(saved)

    def seal(m):
        m.pop('manifest_blake3', None)
        m['manifest_blake3'] = blake3.blake3(json.dumps(m, sort_keys=True, separators=(',', ':'), ensure_ascii=False).encode()).hexdigest()
        return m

    for kind in ('slice-order', 'segment-order', 'missing-slice', 'missing-segment', 'duplicate-slice',
                 'tensor-offset', 'tensor-shape', 'slice-digest', 'segment-digest', 'source', 'scope', 'pending-cleared'):
        m = copy.deepcopy(original)
        if kind == 'slice-order': m['slices'].reverse()
        elif kind == 'segment-order': m['segments'].reverse()
        elif kind == 'missing-slice': m['slices'].pop(2)
        elif kind == 'missing-segment': m['segments'].pop(2)
        elif kind == 'duplicate-slice': m['slices'].append(m['slices'][0])
        elif kind == 'tensor-offset': m['slices'][0]['tensors'][0]['offset'] = 1
        elif kind == 'tensor-shape': m['slices'][0]['tensors'][0]['shape'][0] += 1
        elif kind == 'slice-digest': m['slices'][0]['blake3'] = '0'*64
        elif kind == 'segment-digest': m['segments'][0]['blake3'] = '0'*64
        elif kind == 'source': m['source']['revision'] = '1'*40
        elif kind == 'scope': m['model'] = finalized['model']
        elif kind == 'pending-cleared': m['pending'] = []
        altered = work / f'{kind}.json'; altered.write_text(json.dumps(seal(m)))
        out = work / f'bad-{kind}'
        assemble(out, altered, fails=True)
        assert not out.exists() and not list(work.glob('.yarn-assembly-*'))
        failures.append(kind)
    # Same sealed manifest, independently changed/omitted caller policy.
    if effective:
        out = work / 'bad-historical-policy'
        run('slice-assemble-yarn', '--config', config, '--source-manifest', pin,
            '--manifest', manifest, '--slices', slices, '--out-dir', out, '--fixture', '--historical-int8', fails=True)
        assert not out.exists()
        failures.append('historical-caller-policy')
    substitutions = []
    if effective:
        for key, value in [('version', 2), ('head', 'int8' if effective['head']=='int16' else 'int16'), ('experts','int16')]:
            m = copy.deepcopy(original);m['precision'][key]=value
            substitutions.append((key,m))
        m=copy.deepcopy(original);m.pop('precision');substitutions.append(('removed',m))
    else:
        m=copy.deepcopy(original);m['precision']={'version':1, **{k:'int16' for k in ['attention','dense','shared','embedding','head']}}
        substitutions.append(('legacy-relabel',m))
    for label,m in substitutions:
        altered=work/f'policy-{label}.json';altered.write_text(json.dumps(seal(m)))
        out=work/f'bad-policy-{label}'
        assemble(out,altered,fails=True)
        assert not out.exists() and not list(work.glob('.yarn-assembly-*'))
        failures.append('policy-'+label)
    changed_config = work / 'changed-config.json'; changed_config.write_bytes(config.read_bytes()+b' ')
    assemble(work / 'bad-config', manifest, pin, changed_config, fails=True)
    assert not (work / 'bad-config').exists()
    failures.append('config-substitution')
    # The fixture may never claim the official full/probe identity.
    for extra in ([], ['--probe-layers', '2'], ['--fixture','--probe-layers','2']):
        out = work / ('bad-identity-' + str(len(extra)))
        run('slice-assemble-yarn', '--config', config, '--source-manifest', pin, '--manifest', manifest,
            '--slices', slices, '--out-dir', out, *settings, *extra, fails=True)
        assert not out.exists()
    failures.append('fixture-to-full-or-probe-substitution')
    before = (full / 'report.json').read_bytes()
    assemble(full, fails=True)
    assert (full / 'report.json').read_bytes() == before
    failures.append('existing-output-preserved')
    corrupt = work / 'corrupt.arcspkg'
    data = bytearray((full / 'stage-0.arcspkg').read_bytes()); data[-64] ^= 1; corrupt.write_bytes(data)
    run('verify', '--package', corrupt, '--manifest', full / 'manifest.json', fails=True)
    failures.append('assembled-package-corruption')
    shutil.copy(full / 'stage-0.arcspkg', evidence / 'stage-0.arcspkg')
    shutil.copy(manifest, evidence / 'pending.json')
    shutil.copy(full / 'manifest.json', evidence / 'manifest.json')
    shutil.copy(full / 'report.json', evidence / 'assembly.json')
    (evidence / 'summary.json').write_text(json.dumps({'scope':'synthetic fixture only; no real K2.6 execution',
        'pass':True, 'precision':effective, 'model_root':finalized['model_root'], 'rejections':failures,
        'layouts':[1,2,4], 'cases':len(runs[0]['cases'])}, indent=2)+'\n')
    print(f'PASS: {len(failures)} corruption/identity/output checks; scalar/SIMD and 1/2/4 stages exact')


if __name__ == '__main__':
    main()
