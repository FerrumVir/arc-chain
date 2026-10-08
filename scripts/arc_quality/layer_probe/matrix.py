"""Offline seven-policy × three-graph experiment, with original-source reference."""
import argparse
import json
import os
import subprocess
import sys
import struct
from pathlib import Path
from .compare import canonical, sha
from .policies import CLASSES, NAMES


def process_cell(measurement):
    rss = measurement['peak_rss_bytes']
    peak = 'not measured' if rss is None else f'{rss/2**20:.3f}'
    return f"{measurement['wall_seconds']:.6f} / {peak}"


def preserved_payload(package):
    raw=package.read_bytes();n=struct.unpack_from('<Q',raw,8)[0]
    header=json.loads(raw[16:16+n]);start=((16+n+63)//64)*64
    return {t['name']:raw[start+t['offset']:start+t['offset']+t['bytes']] for t in header['tensors']
            if '.experts.' in t['name'] or 'norm' in t['name'] or 'router' in t['name']}


def summarize(root):
    import numpy as np
    rows=[]; interactions={}; identities={}
    for depth in (1,2,3):
        records={p:json.loads((root/f'depth-{depth}'/p/'arc.json').read_bytes()) for p in NAMES}
        refs={p:json.loads((root/f'depth-{depth}'/p/'reference.json').read_bytes()) for p in NAMES}
        baseline=records['legacy']['tensors']
        for name in NAMES:
            assert preserved_payload(root/f'depth-{depth}'/name/'bundle/stage-0.arcspkg')==preserved_payload(root/f'depth-{depth}'/'legacy/bundle/stage-0.arcspkg'),'native INT4/norm/router bytes changed'
            # A policy only affects ARC conversion. Reference source and outputs must remain identical.
            assert refs[name]['tensors']==refs['legacy']['tensors'],(depth,name,'reference values changed')
            assert refs[name]['routing']==refs['legacy']['routing'],(depth,name,'reference routing changed')
            assert refs[name]['provenance']['weight_tensors']==refs['legacy']['provenance']['weight_tensors']
            assert records[name]['observer']['verified_against_unmodified_engine'] is True
            if name=='shared' and depth==1:
                assert records[name]['tensors']==baseline,'inactive shared class changed values'
            if name=='head':
                for i in range(depth):
                    assert records[name]['tensors'][f'layer.{i}.output']==baseline[f'layer.{i}.output'],'head promotion changed upstream activation'
            result=json.loads((root/f'depth-{depth}'/name/'comparison.json').read_bytes())
            identities[f'{depth}/{name}']={
                'package_sha256':records[name]['alignment']['package_sha256'],
                'model_root':records[name]['alignment']['model_root'],
                'tensor_sha256':records[name]['tensor_sha256'],
                'routing_sha256':sha(canonical(records[name]['routing'])),
                'reference_tensor_sha256':refs[name]['tensor_sha256'],
                'reference_routing_sha256':sha(canonical(refs[name]['routing']))}
            for tensor,metrics in result['metrics'].items():
                rows.append({'depth':depth,'policy':name,'inactive':name=='shared' and depth==1,'tensor':tensor,
                             **metrics,'top1_agreements':result['top1']['agreements'] if tensor=='logits' else None,
                             'top1_positions':result['top1']['positions'] if tensor=='logits' else None,
                             'routing_set_mismatches':{k:v['expert_set_mismatches'] for k,v in result['routing_comparison'].items()}})
        interactions[str(depth)]={}
        for key,values in baseline.items():
            base=np.array(values,dtype=np.float64)*2**-16
            joint=np.array(records['int16']['tensors'][key],dtype=np.float64)*2**-16-base
            individual=sum(np.array(records[c]['tensors'][key],dtype=np.float64)*2**-16-base for c in CLASSES)
            residual=joint-individual
            interactions[str(depth)][key]={'max_absolute_nonadditive_residual':float(np.max(np.abs(residual))),
                                           'rmse_nonadditive_residual':float(np.sqrt(np.mean(residual**2)))}
    data={'schema':'arc.precision-fixture-matrix.v1','status':'MEASURED_NON_CERTIFYING','certified':False,
          'dependency_review':'provisional/unreviewed','policies':list(NAMES),'rows':rows,'identities':identities,
          'interactions':interactions,'interaction_definition':'(all-INT16 ARC minus legacy ARC) minus sum(single-class ARC minus legacy ARC), Q16 rescaled; nonlinear and routing interactions, not additive quality attribution',
          'reference':'original selected BF16/F32 and original native INT4 group scales -> official CPU FP32; never dequantized ARC packages',
          'native_int4_norm_router_bytes_unchanged':True,
          'inactive':'shared has no layer in depth 1; its identity changes but execution tensors must equal legacy',
          'host':json.loads((root/'depth-1/legacy/arc-resources.json').read_bytes())['host']}
    (root/'matrix.json').write_bytes(canonical(data))
    lines=['# Synthetic precision errors (non-certifying)', '',str(data['host']),'',
           '|Depth|Policy|Tensor|max abs|relative L2|max abs / ref L2|top-1|routing set differences|','|---:|---|---|---:|---:|---:|---|---|']
    for r in rows:
        top='' if r['top1_positions'] is None else f"{r['top1_agreements']}/{r['top1_positions']}"
        lines.append(f"|{r['depth']}|{r['policy']}{' (inactive)' if r['inactive'] else ''}|{r['tensor']}|{r['max_absolute_error']:.9g}|{r['relative_l2_error']:.9g}|{r['max_absolute_error_over_reference_l2']:.9g}|{top}|{r['routing_set_mismatches']}|")
    lines+=['','All single-class controls use original sources. Simultaneous promotions are nonlinear; `matrix.json` reports the per-tensor nonadditive residual. No ranking of precision classes, tolerance PASS or shipping-precision recommendation.',
            'Norm denominators use max(reference L2, 1e-8). JSON retains max_relative_error for diagnostics only: its elementwise 1e-8 floor makes it unbounded and dominated by near-zero reference values; do not use it for decisions.','',
            '|Depth|Policy|ARC process seconds / peak MiB|Reference process seconds / peak MiB|','|---:|---|---:|---:|']
    for depth in (1,2,3):
        for name in NAMES:
            m=[json.loads((root/f'depth-{depth}'/name/f'{k}-resources.json').read_bytes()) for k in ('arc','reference')]
            lines.append(f"|{depth}|{name}|{process_cell(m[0])}|{process_cell(m[1])}|")
    lines+=['','Measurements are one fresh CPU process per command, including imports/load/capture/serialization, uncontrolled warm OS cache, no repetitions; ARC diagnostic runs whole, split and observed forwards for verification. This is not inference-only timing, a matched throughput benchmark or real-model RAM admission.']
    (root/'matrix.md').write_text('\n'.join(lines)+'\n')
    return data


def main():
    p=argparse.ArgumentParser(description=__doc__)
    for key in ('engine-source','arc-mla','diagnostic','out'):p.add_argument('--'+key,required=True)
    args=p.parse_args();root=Path(args.out).resolve();root.mkdir(parents=True,exist_ok=False)
    for depth in (1,2,3):
        for name in NAMES:
            out=root/f'depth-{depth}'/name
            command=[sys.executable,'-m','arc_quality.layer_probe.run','--engine-source',args.engine_source,
                     '--arc-mla',args.arc_mla,'--diagnostic',args.diagnostic,'--layers',str(depth),'--policy',name,'--out',str(out)]
            with (root/f'{depth}-{name}.log').open('w') as f:subprocess.run(command,stdout=f,stderr=subprocess.STDOUT,check=True)
            env={**os.environ,'ARC_LAYER_FIXTURE':str(out),'ARC_LAYER_DIAGNOSTIC':str(Path(args.diagnostic).resolve())}
            with (root/f'{depth}-{name}-tests.log').open('w') as f:
                subprocess.run([sys.executable,'-m','unittest','arc_quality.layer_probe.test_comparison','-v'],env=env,stdout=f,stderr=subprocess.STDOUT,check=True)
            print(f'Completed depth={depth} policy={name}',flush=True)
    summarize(root)


if __name__=='__main__':main()
