#!/usr/bin/env python3
"""Offline BF16 fixture -> mixed precision package -> scalar/SIMD/split checks.
Timing is process wall and CLI internal time; RSS from OS time per process.
No model downloads, reference-quality assessment or tolerance PASS.
"""
import argparse
import datetime
import hashlib
import json
import platform
import re
import subprocess
import sys
import time
from pathlib import Path


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('binary', type=Path)
    ap.add_argument('out', type=Path)
    ap.add_argument('--build-label', default='release')
    args = ap.parse_args()
    binary = args.binary.resolve()
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=True)
    measures = []

    def run(label, command, expected=0):
        command = list(map(str, command))
        timed = Path('/usr/bin/time').exists() and platform.system() in ('Linux', 'Darwin')
        invocation = (['/usr/bin/time', '-l' if platform.system() == 'Darwin' else '-v'] if timed else []) + command
        start = time.perf_counter()
        result = subprocess.run(invocation, capture_output=True, text=True)
        wall = time.perf_counter() - start
        (out / (label + '.stdout')).write_text(result.stdout)
        (out / (label + '.stderr')).write_text(result.stderr)
        rss = None
        if timed:
            pattern = r'(\d+)\s+maximum resident set size' if platform.system() == 'Darwin' else r'Maximum resident set size \(kbytes\):\s*(\d+)'
            match = re.search(pattern, result.stderr)
            if match:
                rss = int(match[1]) * (1 if platform.system() == 'Darwin' else 1024)
        measures.append(dict(label=label, command=command, wall_seconds=wall, peak_rss_bytes=rss,
                             returncode=result.returncode))
        assert (result.returncode == 0) if expected == 0 else (result.returncode != 0), (label, result.stderr)
        return result

    source = out / 'source'
    run('fixture', [sys.executable, Path(__file__).with_name('make_tiny_mla_model.py'), source, '--variant', 'kimi'])
    base = [binary, 'convert', '--source-dir', source, '--source-manifest', source / 'tiny-mla.source.json', '--experts', 'i4g32', '--threads', '1']
    wide = dict(version=1, attention='int16', dense='int16', shared='int16', embedding='int16', head='int16')
    mixed = dict(wide, dense='int8', head='int8')
    policies = dict(legacy=None, int16=wide, mixed=mixed)
    for label, precision in policies.items():
        print(f'{label}: precision={json.dumps(precision, sort_keys=True)}', flush=True)
        settings = []
        if precision:
            precision_path = out / (label + '-precision.json')
            precision_path.write_text(json.dumps(precision))
            settings = ['--precision', precision_path]
        pkg, manifest = out / (label+'.arcspkg'), out / (label+'-manifest.json')
        run(label+'-convert', base+settings+['--out', pkg, '--manifest-out', manifest, '--report', out/(label+'-convert.json')])
        run(label+'-verify', [binary, 'verify', '--package', pkg, '--manifest', manifest, '--full-digest'])
        runs = []
        for kernel in ('scalar', 'simd'):
            path = out / (label+'-'+kernel+'.json')
            run(label+'-'+kernel, [binary, 'golden', '--package', pkg, '--cases', source/'tiny-mla.cases.json', '--out', path, '--kernel', kernel, '--threads', '1'])
            runs.append(json.loads(path.read_text()))
        for key in ('model_root', 'matrix_digest', 'boundary_matrix_digest'):
            assert runs[0][key] == runs[1][key], (label,key)
        # Native wire boundary consumption with separate converted stage packages.
        previous = None
        for layer in range(4):
            tag = f'{label}-stage-{layer}'
            stage = out/(tag+'.arcspkg')
            run(tag+'-convert', base+settings+['--layers',f'{layer}:{layer+1}','--out',stage])
            boundary = out/(tag+'.bin')
            run(tag, [binary,'stage','--package',stage,'--manifest',manifest,
                      *(['--input',previous] if previous else ['--run',out/(label+'-scalar.json')]),
                      '--out',boundary,'--report',out/(tag+'.json'),'--kernel','simd','--threads','1'])
            previous = boundary
        # Stage CLI checks each replay against the committed run when --run is used.
        run(label+'-whole-stage', [binary,'stage','--package',pkg,'--run',out/(label+'-scalar.json'),
                                  '--out',out/(label+'-whole.bin'),'--report',out/(label+'-whole.json'),'--threads','1'])
        assert previous.read_bytes() == (out/(label+'-whole.bin')).read_bytes()
    # Actual CLI argument/schema rejection must leave no output package.
    for label, tail in [('trailing',['--precision']), ('option-value',['--precision','--threads','1'])]:
        dest = out/(label+'-rejected.arcspkg')
        r = run(label,base+['--out',dest]+tail,expected=1)
        assert '--precision requires' in r.stderr and not dest.exists()
    for label, policy in [('version',dict(wide,version=2)), ('experts',dict(wide,experts='int16')), ('missing', {k:v for k,v in wide.items() if k!='head'})]:
        path = out/(label+'-bad.json'); path.write_text(json.dumps(policy))
        dest = out/(label+'-rejected.arcspkg')
        run(label,base+['--out',dest,'--precision',path],expected=1)
        assert not dest.exists()
    facts = dict(timestamp_utc=datetime.datetime.now(datetime.timezone.utc).isoformat(),
                 platform=platform.platform(), machine=platform.machine(), build=args.build_label,
                 threads=1, workload='synthetic BF16; 4 layers dense0+MoE1..3, width64, vocab300; fresh processes; warm filesystem caches; no GPU',
                 precision_decision='unapproved; no quality/tolerance assessment',
                 policies=policies,
                 measurements=measures, files={p.name:dict(bytes=p.stat().st_size,sha256=hashlib.sha256(p.read_bytes()).hexdigest()) for p in out.glob('*.arcspkg')})
    (out/'measurements.json').write_text(json.dumps(facts,indent=2)+'\n')
    print(json.dumps({k:json.loads((out/(k+'-scalar.json')).read_text())['matrix_digest'] for k in ('legacy','int16','mixed')},indent=2))


if __name__ == '__main__':
    main()
