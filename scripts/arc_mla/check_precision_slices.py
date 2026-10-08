#!/usr/bin/env python3
"""Offline mixed-policy packed BF16 -> slices -> YaRN -> engine integration."""
import json
import struct
import subprocess
import sys
from pathlib import Path


def read(p):
    return json.loads(p.read_text())


def tensors(path):
    b=path.read_bytes()
    n=struct.unpack_from('<Q',b,8)[0]
    header=json.loads(b[16:16+n]); start=((16+n+63)//64)*64
    return header,{t['name']:b[start+t['offset']:start+t['offset']+t['bytes']] for t in header['tensors']}


def main():
    binary,work,evidence=(Path(p).resolve() for p in sys.argv[1:])
    work.mkdir(parents=True);evidence.mkdir(parents=True)
    wide=dict(version=1,**{k:'int16' for k in ['attention','dense','shared','embedding','head']})
    policies={'legacy':None,'int16':wide,'mixed':dict(wide,embedding='int8',shared='int8')}
    for label,policy in policies.items():
        print(f'{label}: precision={json.dumps(policy, sort_keys=True)}', flush=True)
        if policy:
            (work/(label+'.json')).write_text(json.dumps(policy))
    roots={};baseline=None;commands=[]
    for label,policy in policies.items():
        settings=[]
        if policy:
            path=work/(label+'.json');path.write_text(json.dumps(policy));settings=[path]
        cmd=[sys.executable,Path(__file__).with_name('check_yarn_slices.py'),binary,work/label,evidence/label,*settings]
        subprocess.run(list(map(str,cmd)),check=True);commands.append(list(map(str,cmd)))
        header,contents=tensors(evidence/label/'stage-0.arcspkg')
        roots[label]=read(evidence/label/'manifest.json')['model_root']
        # Published expert bytes/scales, norms, bias and INT16 router unchanged.
        preserved={n:b for n,b in contents.items() if '.experts.' in n or 'norm' in n or 'router' in n}
        if baseline is None: baseline=preserved
        else: assert preserved==baseline, f'{label}: native expert/norm/router bytes changed'
        for t in header['tensors']:
            n=t['name']
            if not n.endswith('.q') or 'router' in n:continue
            if '.experts.' in n:continue
            key=('embedding' if n.startswith('embed.') else 'head' if n.startswith('lm_head.') else 'shared' if '.shared.' in n else 'dense' if any(k in n for k in ['.w_gate.','.w_up.','.w_down.']) else 'attention')
            expected='i16' if policy and policy[key]=='int16' else 'i8'
            assert t['dtype']==expected,(label,n,t['dtype'],expected)
        # Reusing unit records under another policy must fail, not relabel.
        if policy:
            other='mixed' if label=='int16' else 'int16'
            for kind,flags in [('omitted',[]),('different-non-null',['--precision',str(work/(other+'.json'))])]:
                bad=work/label/f'bad-resume-{kind}.json'
                r=subprocess.run([str(binary),'slice-manifest','--source-dir',str(work/label/'source'),
                              '--source-manifest',str(work/label/'source/tiny-kimi-packed.source.json'),
                              '--out-dir',str(work/label/'slices'),'--expert-groups','4','--out',str(bad),*flags],capture_output=True,text=True)
                assert r.returncode!=0 and not bad.exists()
                assert 'unit record' in r.stderr and 'was made for' in r.stderr,r.stderr
                (evidence/label/f'stale-unit-{kind}-rejection.log').write_text(r.stderr)
        # Swap a selected BF16-derived slice payload with bytes from another policy.
        if label!='legacy':
            pending=read(evidence/label/'pending.json');old=read(evidence/'legacy/pending.json')
            target=next(s for s in pending['slices'] if any(t['name']=='lm_head.q' for t in s['tensors']))
            legacy=next(s for s in old['slices'] if s['name']==target['name'])
            path=work/label/'slices'/(target['blake3']+'.slice');saved=path.read_bytes()
            foreign=(work/'legacy/slices'/(legacy['blake3']+'.slice')).read_bytes()
            assert len(foreign)<len(saved)==target['bytes']
            # Retain the raw swap and add adversarial zero padding so length
            # alone cannot reject the foreign-policy payload. No resealing.
            for kind,payload in [('raw',foreign),('equal-length',foreign.ljust(len(saved),b'\0'))]:
                bad=work/label/f'bad-payload-substitution-{kind}'
                try:
                    path.write_bytes(payload)
                    if kind=='equal-length':
                        assert path.stat().st_size==target['bytes'] and payload!=saved
                    r=subprocess.run([str(binary),'slice-assemble-yarn','--config',str(work/label/'source/config.json'),
                    '--source-manifest',str(work/label/'source/tiny-kimi-packed.source.json'),
                    '--manifest',str(work/label/'slices.json'),'--slices',str(work/label/'slices'),'--fixture',
                    '--precision',str(settings[0]),'--out-dir',str(bad)],capture_output=True,text=True)
                    assert r.returncode!=0 and not bad.exists()
                    expected=('selected slice file length mismatch' if kind=='raw' else
                              'selected bytes/metadata differ from committed segment')
                    assert expected in r.stderr,r.stderr
                    assert not list((work/label).glob('.yarn-assembly-*'))
                    (evidence/label/f'payload-substitution-{kind}.log').write_text(r.stderr)
                finally:path.write_bytes(saved)
    assert len(set(roots.values()))==3
    (evidence/'summary.json').write_text(json.dumps({'engine_dependency':'596a61f6b1ae88e8ad7072e1d514467308ddae22','dependency_review':'provisional/unreviewed','scope':'synthetic fixtures only; no quality or precision acceptance','policies':policies,'roots':roots,'native_int4_norm_router_bytes_unchanged':True,'commands':commands},indent=2)+'\n')
    print('PASS: legacy/all-INT16/mixed slice conversion, byte verification and engine consumption')


if __name__=='__main__':main()
