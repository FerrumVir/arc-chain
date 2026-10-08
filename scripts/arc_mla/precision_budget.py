#!/usr/bin/env python3
"""Offline planning estimates from pinned config/source metadata; never admission."""
import argparse
import hashlib
import json
from pathlib import Path

GIB=2**30
ROOT=Path(__file__).resolve().parents[2]


def budget():
    config_path=ROOT/'docs/protocol/reference/kimi-k26/config.json'
    source_path=ROOT/'docs/protocol/packages/kimi-k26.source.json'
    c=json.loads(config_path.read_bytes())['text_config']; source=json.loads(source_path.read_bytes())
    d,v,h,r,qr,n,p,vh=(c[k] for k in ('hidden_size','vocab_size','num_attention_heads','kv_lora_rank','q_lora_rank','qk_nope_head_dim','qk_rope_head_dim','v_head_dim'))
    attn=[(qr,d),(h*(n+p),qr),(r+p,d),(h*r,n),(h*vh,r),(d,h*vh)]
    ff=c['intermediate_size'];mf=c['moe_intermediate_size'];experts=c['n_routed_experts'];shared=c['n_shared_experts']
    native_expert_elements=3*experts*d*mf
    files={x['name']:x['bytes'] for x in source['files']}
    rows=[]
    for depth in (1,2,3):
        moe=depth-1
        classes={'embedding':[(v,d)],'head':[(v,d)],'attention':attn*depth,
                 'dense':[(ff,d),(ff,d),(d,ff)],'shared':[(shared*mf,d),(shared*mf,d),(d,shared*mf)]*moe}
        # Norms: attn/ffn d, q_a qr, kv_a r, and final d. Router is I16+k; bias I64.
        norms=depth*(2*d+qr+r)+d
        router=moe*experts*d;bias=moe*experts
        fixed_bytes=8*norms+2*router+9*bias+moe*(native_expert_elements//2+native_expert_elements//32*2)
        bf16_elements={k:sum(a*b for a,b in matrices) for k,matrices in classes.items()}
        row_scales=5*sum(a for matrices in classes.values() for a,_ in matrices)
        raw_params=sum(bf16_elements.values())+norms+router+bias+moe*native_expert_elements
        reference_fp32=4*raw_params
        selected_sources=[f'model-{i:05d}-of-000064.safetensors' for i in range(1,depth+1)]+['model-00062-of-000064.safetensors']
        retained=sum(files[name] for name in selected_sources)
        largest_bf16=2*max(a*b for matrices in classes.values() for a,b in matrices)
        # Conversion read_bf16 may hold both byte and u16 arrays, then output.
        conversion_scratch=2*largest_bf16+row_scales+2*d+49*2**20
        kv_b_elements=h*(n+vh)*r
        transpose_scratch=2*kv_b_elements+2*h*r*n+2*h*vh*r
        # Stage writer + 49 open slice readers + tables/replay buffer.
        assembly_ram=(8+49*8+8)*2**20+depth*2**20
        largest_reference_matrix=max(v*d,experts*d*mf if moe else 0)
        reference_transient=4*largest_reference_matrix
        for policy in ('legacy','int16','mixed'):
            wide=set() if policy=='legacy' else set(classes) if policy=='int16' else {'attention','dense','head'}
            weight_bytes=fixed_bytes+row_scales+sum(elements*(2 if key in wide else 1) for key,elements in bf16_elements.items())
            tables=depth*4096*32*2*4
            disk_margin=2**29;reserve=5*GIB
            peak_disk=retained+2*weight_bytes+tables+disk_margin+reserve
            engine_ram=weight_bytes+tables+assembly_ram+3*GIB
            reference_ram=retained+reference_fp32+reference_transient+3*GIB
            rows.append(dict(layers=depth,policy=policy,selected_sources=selected_sources,
                class_matrix_elements=bf16_elements,retained_source_bytes=retained,
                slice_payload_bytes=weight_bytes,assembly_copy_bytes=weight_bytes,canonical_table_bytes=tables,
                disk_header_alignment_scratch_margin_bytes=disk_margin,disk_reserve_bytes=reserve,
                peak_disk_with_retained_sources_bytes=peak_disk,
                optional_persisted_fp32_cache_extra_disk_bytes=reference_fp32,
                conversion_scratch_ram_bytes=conversion_scratch,kv_b_transpose_ram_bytes=transpose_scratch,
                assembly_buffer_ram_bytes=assembly_ram,largest_source_bf16_tensor_bytes=largest_bf16,
                reference_fp32_parameters_bytes=reference_fp32,reference_largest_fp32_tensor_scratch_bytes=reference_transient,
                engine_resident_planning_bytes=engine_ram,reference_resident_planning_bytes=reference_ram,
                sequential_available_ram_planning_bytes=max(conversion_scratch+transpose_scratch+3*GIB,engine_ram,reference_ram)))
    return dict(engine_dependency='596a61f6b1ae88e8ad7072e1d514467308ddae22',dependency_review='provisional/unreviewed',
        pins={str(p.relative_to(ROOT)):hashlib.sha256(p.read_bytes()).hexdigest() for p in (config_path,source_path)},
        assumptions=['Calculated bytes, not measured disk/RSS. No host admission.',
        'Retain every selected original shard; do not delete source needed by reference.',
        'Slices and one assembled package set coexist; atomic rename does not duplicate the staging bundle.',
        'No simultaneous whole-model plus split bundle copies; add another complete payload if retained.',
        'FP32 reference expands original source values independently of ARC policy; no on-disk FP32 cache by default. If persisted, add the explicit cache bytes.',
        'RAM planning assumes all selected sources may be resident, FP32 parameters, one largest FP32 tensor transient and 3 GiB runtime/OS margin. Not an allocator hard bound.',
        'ARC exits before reference loads. For concurrency sum process budgets instead.',
        'max_seq=4096, one sequence; activation/cache/capture growth or larger contexts need rebudgeting.',
        '5 GiB disk reserve and 512 MiB metadata/alignment/crash-work allowance are planning assumptions; count existing leftovers in actual measured free space.'],rows=rows)


def main():
    ap=argparse.ArgumentParser(description=__doc__);ap.add_argument('out',type=Path);args=ap.parse_args()
    data=budget();args.out.write_text(json.dumps(data,indent=2)+'\n')
    print('| Layers | Policy | Retained source GiB | Slices GiB | Assembly copy GiB | Peak disk GiB | FP32 params GiB | Sequential RAM plan GiB |')
    print('|---:|---|---:|---:|---:|---:|---:|---:|')
    for r in data['rows']:
        print('| '+str(r['layers'])+' | '+r['policy']+' | '+' | '.join(f'{r[k]/GIB:.4f}' for k in ['retained_source_bytes','slice_payload_bytes','assembly_copy_bytes','peak_disk_with_retained_sources_bytes','reference_fp32_parameters_bytes','sequential_available_ram_planning_bytes'])+' |')


if __name__=='__main__':main()
