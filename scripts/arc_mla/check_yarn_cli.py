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
    repo=Path(__file__).resolve().parents[2]
    ref=repo/'docs/protocol/reference/kimi-k26'
    config=ref/'config.json'
    source=json.loads((ref/'source.json').read_text())
    source={k:source[k] for k in ['repo','revision','files']}
    pending={'schema':'arc.integer-slice-manifest.v1',
             'profile':'arc.hf-deepseek-v3.mla-moe.i8-dyadic-row.i4g32-experts.q16.v1',
             'pending':['rope_scaling yarn'],'model':None,'tables':None,'model_root':None,
             'weights':{'prefix':'language_model.','packed_experts':True},'source':source,
             'segments':[{'name':n,'bytes':size,'blake3':'a'*64} for n,size in [
                 ('embed',1175224320),('layer.0',498147648),('layer.1',9663852224),('head',1175281664)]]}

    def write_pending(name, v):
        v=copy.deepcopy(v)
        v['manifest_blake3']=blake3.blake3(json.dumps(v,sort_keys=True,separators=(',',':')).encode()).hexdigest()
        path=out/name;path.write_text(json.dumps(v)+'\n');return path

    results=[]
    def run(name, args, ok, error=''):
        report=out/(name+'.json')
        result=subprocess.run([str(binary),'yarn-prepare','--config',str(config),
                               '--out',str(report),*map(str,args)],capture_output=True,text=True)
        assert (result.returncode==0)==ok,(name,result.stderr)
        if not ok:
            assert error in result.stderr,(name,result.stderr)
            assert not report.exists(),name
        (out/(name+'.log')).write_text(result.stdout+result.stderr)
        results.append({'name':name,'exit_code':result.returncode,'expected_success':ok})
        return report

    run('prepare-full',[],True)
    pin=write_pending('pending.json',pending);before=pin.read_bytes()
    target=out/'probe-manifest.json'
    run('prepare-probe',['--probe-layers','2','--slice-manifest',pin,'--manifest-out',target],True)
    m=json.loads(target.read_text());assert m['profile'].startswith('arc.experimental.')
    assert m['model']['n_layers']==2 and len(m['segments'])==5
    assert pin.read_bytes()==before
    run('incomplete-full',['--slice-manifest',pin,'--manifest-out',out/'missing.json'],False,'missing selected segment')
    assert not (out/'missing.json').exists()
    run('missing-probe-value',['--probe-layers'],False,'missing --probe-layers')
    run('bad-probe',['--probe-layers','61'],False,'1..=3')
    run('bad-context',['--max-seq','262145'],False,'max_seq')
    bad=copy.deepcopy(pending);bad['source']['revision']='forged'
    badpin=write_pending('bad-source-input.json',bad)
    run('bad-source',['--probe-layers','2','--slice-manifest',badpin,'--manifest-out',out/'bad.json'],False,'pinned K2.6 source')
    assert not (out/'bad.json').exists()
    (out/'summary.json').write_text(json.dumps({'scope':'CLI and synthetic commitment metadata; no weight validation',
                                             'results':results,'pass':True},indent=2)+'\n')
    print(json.dumps(results,indent=2))

if __name__=='__main__':main()
