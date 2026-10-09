"""Runnable offline fixture comparison; all model/reference/code inputs are local."""
import argparse
import json
import shutil
import subprocess
import sys
from pathlib import Path
from .compare import canonical, compare, sha, sha_file
from .official import REFERENCE_SHA
from .policies import RUN_NAMES, policy

ENGINE_SHA='8bd1e6a1696304517a261a06aee43c142b44f128'


def main():
    p=argparse.ArgumentParser()
    p.add_argument('--engine-source',required=True)
    p.add_argument('--arc-mla',required=True)
    p.add_argument('--diagnostic',required=True)
    p.add_argument('--out',required=True)
    p.add_argument('--layers',type=int,choices=[1,2,3],default=1)
    p.add_argument('--policy',choices=RUN_NAMES,default='default')
    args=p.parse_args()
    precision=policy(args.policy)
    engine=Path(args.engine_source).resolve();binary=Path(args.arc_mla).resolve();diagnostic=Path(args.diagnostic).resolve()
    head=subprocess.check_output(['git','rev-parse','HEAD'],cwd=engine,text=True).strip()
    if head!=ENGINE_SHA:raise ValueError('engine checkout is not the provisional exact pin')
    subprocess.run(['git','diff','--quiet','HEAD','--','scripts/arc_mla','crates/arc-inference'],cwd=engine,check=True)
    observer=Path(__file__).resolve().parents[3]/'tools/quality-layer-probe/reference/model.rs'
    if observer.read_bytes() != (engine/'crates/arc-inference/src/modern/mla/model.rs').read_bytes():
        raise ValueError('observer source differs from pinned engine')
    out=Path(args.out).resolve();out.mkdir(parents=True,exist_ok=False)
    def command(*argv):
        r=subprocess.run(list(map(str,argv)),capture_output=True,text=True)
        with (out/'commands.log').open('a',encoding='utf-8') as f:f.write(f'{list(map(str,argv))}\nexit={r.returncode}\n{r.stdout}{r.stderr}\n')
        if r.returncode:raise RuntimeError(r.stderr)
    precision_args=['--historical-int8'] if args.policy == 'legacy' else []
    if precision is not None and args.policy != 'default':
        (out/'precision.json').write_bytes(canonical(precision))
        precision_args=['--precision',out/'precision.json']
    original=out/'original';source=out/'source';slices=out/'slices';bundle=out/'bundle'
    command(sys.executable,engine/'scripts/arc_mla/make_tiny_kimi_packed.py',original,'--prepared')
    shutil.copytree(original,source)
    cfg=json.loads((source/'config.json').read_text());cfg['text_config']['num_hidden_layers']=args.layers
    (source/'config.json').write_bytes(canonical(cfg))
    pin=source/'tiny-kimi-packed.source.json';pin_doc=json.loads(pin.read_text())
    pin_doc['repo']=('arc-test/tiny-kimi-original-layer0-head' if args.layers==1 else f'arc-test/tiny-kimi-original-layers{args.layers}-head')
    for item in pin_doc['files']:
        if item['name']=='config.json':item.update(bytes=(source/'config.json').stat().st_size,sha256=sha((source/'config.json').read_bytes()))
    pin.write_bytes(canonical(pin_doc))
    manifest=out/'slices.json'
    command(binary,'slice','--source-dir',source,'--source-manifest',pin,'--out-dir',slices,'--expert-groups',4,*precision_args)
    command(binary,'slice-manifest','--source-dir',source,'--source-manifest',pin,'--out-dir',slices,'--expert-groups',4,'--out',manifest,*precision_args)
    command(binary,'slice-assemble-yarn','--config',source/'config.json','--source-manifest',pin,'--manifest',manifest,
            '--slices',slices,'--out-dir',bundle,'--fixture',*precision_args)
    final=json.loads((bundle/'manifest.json').read_text());package=bundle/'stage-0.arcspkg'
    request={'engine_sha':ENGINE_SHA,'precision':precision,'model_root':final['model_root'],'scope':final['model']['preparation']['scope'],
             'graph':{'embedding':'original','executed_layers':list(range(args.layers)),'head':'original','source_layer_count':4},
             'shape':{'hidden_size':cfg['text_config']['hidden_size'],'vocab_size':cfg['text_config']['vocab_size']},
             'token_ids':[1,42,7,3],'positions':[0,1,2,3],'mask':'causal_no_padding',
             'source_manifest_sha256':sha(pin.read_bytes()),'config_sha256':sha((source/'config.json').read_bytes()),
             'original_source_manifest_sha256':sha((original/'tiny-kimi-packed.source.json').read_bytes()),
             'package_sha256':sha_file(package),'reference_sha256':REFERENCE_SHA}
    if args.layers>1:
        request['moe']={k:cfg['text_config'][k] for k in ['n_routed_experts','num_experts_per_tok','n_shared_experts','n_group','topk_group']}
    request_bytes=canonical(request);(out/'request.json').write_bytes(request_bytes)
    command(sys.executable,'-m','arc_quality.layer_probe.measured',out/'arc-resources.json',diagnostic,package,bundle/'manifest.json',pin,out/'request.json',out/'arc.json')
    # Reference executes independently from ARC raw outputs; only the committed
    # pre-run request/weights are shared.
    command(sys.executable,'-m','arc_quality.layer_probe.measured',out/'reference-resources.json',sys.executable,'-m','arc_quality.layer_probe.reference_worker',out)
    reference=json.loads((out/'reference.json').read_bytes())
    arc=json.loads((out/'arc.json').read_text())
    report=compare(arc,reference,request,request_bytes)
    report['setup']={'engine_checkout':head,'arc_mla_sha256':sha_file(binary),'diagnostic_sha256':sha_file(diagnostic),
                     'reference_dtype':'source BF16/F32 and packed INT4 group32 -> FP32 CPU','arc_dtype':'dyadic integer weights, Q16 activation/logits',
                     'original_head_preserved':True,'runtime_io':'local files; no downloader or API client invoked',
                     'dependency_review':'provisional/unreviewed', 'policy_name':args.policy,
                     'profile_role':'primary INT16 default' if args.policy in ('default','int16') else 'historical comparison experiment',
                     'inactive_classes':['shared'] if args.layers==1 else [],
                     'measurements':{k:json.loads((out/(k+'-resources.json')).read_bytes()) for k in ['arc','reference']}}
    (out/'comparison.json').write_bytes(canonical(report))
    print(json.dumps(report,indent=2))


if __name__=='__main__':main()
