#!/usr/bin/env python3
"""Offline actual-converter row-window controls. Synthetic only, no downloads."""
import argparse
import hashlib
import json
from pathlib import Path
import struct
import subprocess
import sys


def main():
    ap=argparse.ArgumentParser(description=__doc__)
    ap.add_argument('binary',type=Path)
    ap.add_argument('out',type=Path)
    a=ap.parse_args(); binary=a.binary.resolve(); out=a.out.resolve()
    out.mkdir(parents=True,exist_ok=True)
    source=out/'source'
    subprocess.run([sys.executable,str(Path(__file__).with_name('make_tiny_mla_model.py')),str(source),'--variant','kimi'],check=True)
    manifest=json.loads((source/'tiny-mla.source.json').read_text())
    shards={p.name:p.read_bytes() for p in source.glob('*.safetensors')}
    policy=dict(version=1,attention='int16',dense='int16',shared='int16',embedding='int16',head='int16')
    policy_path=out/'policy.json'; policy_path.write_text(json.dumps(policy))
    tensor_cases=[('attention','model.layers.0.self_attn.q_a_proj.weight'),
                  ('dense','model.layers.0.mlp.up_proj.weight'),
                  ('shared','model.layers.1.mlp.shared_experts.up_proj.weight'),
                  ('embedding','model.embed_tokens.weight'),('head','lm_head.weight'),
                  ('attention-key-transpose','model.layers.0.self_attn.kv_b_proj.weight')]
    results=[]
    for tensor_class,name in tensor_cases:
        shard_name,raw,header,start=None,None,None,None
        for fname,b in shards.items():
            n=struct.unpack_from('<Q',b)[0]; h=json.loads(b[8:8+n])
            if name in h:shard_name,raw,header,start=fname,b,h,8+n
        assert raw is not None
        info=header[name]; assert info['dtype']=='BF16'
        begin,end=info['data_offsets']; rows,cols=info['shape']
        # Constant selected tensor: both original and transposed semantic rows
        # have exactly the same declared max; unselected tensors are untouched.
        for label,bits,accepted in [('zero',0,True),('below',0x36ff,False),('lower',0x3700,True),
                                    ('upper-neighbor',0x4e7f,True),('upper',0x4e80,False)]:
            tag=f'{tensor_class}-{label}'
            case=out/tag; case.mkdir(exist_ok=True)
            for fname,b in shards.items():
                if fname==shard_name:
                    b=b[:start+begin]+struct.pack('<H',bits)*(rows*cols)+b[start+end:]
                (source/fname).write_bytes(b)
            for entry in manifest['files']:
                entry['sha256']=hashlib.sha256((source/entry['name']).read_bytes()).hexdigest()
            (source/'tiny-mla.source.json').write_text(json.dumps(manifest))
            pkg=case/'output.arcspkg'
            assert not pkg.exists(), 'use a fresh evidence directory'
            r=subprocess.run([str(binary),'convert','--source-dir',str(source),'--source-manifest',str(source/'tiny-mla.source.json'),
                              '--experts','i4g32','--precision',str(policy_path),'--out',str(pkg),'--threads','1'],capture_output=True,text=True)
            (case/'stdout.txt').write_text(r.stdout); (case/'stderr.txt').write_text(r.stderr)
            assert (r.returncode==0)==accepted,(tag,r.stderr)
            if not accepted:
                assert name in r.stderr and 'conversion row 0' in r.stderr and '[2^-17,2^30)' in r.stderr,(tag,r.stderr)
                if tensor_class=='attention-key-transpose':assert 'transposed key rows [head,rank,nope]' in r.stderr
            results.append(dict(tensor=name,tensor_class=tensor_class,case=label,maximum_bf16=f'{bits:04x}',
                                returncode=r.returncode,accepted=accepted,source_sha256=next(e['sha256'] for e in manifest['files'] if e['name']==shard_name)))
    (out/'results.json').write_text(json.dumps(dict(evidence='synthetic converter controls, not real tensor census',cases=results),indent=2)+'\n')
    print(f'{len(results)} synthetic actual-converter controls passed')


if __name__=='__main__':main()
