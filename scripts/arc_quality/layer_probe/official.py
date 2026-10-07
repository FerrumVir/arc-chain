"""Execute unmodified AST definitions from pinned Apache-2.0 official source.

No AutoModel/from_pretrained, remote code loader, tokenizer or download path.
Only the eager dense layer and its dependencies are selected. Original method
bodies are not rewritten; actual torch modules execute attention/MLP/norms.
"""
import ast
import json
import math
import types
import typing
import warnings
from pathlib import Path
from .compare import sha, validate_request

REFERENCE_SHA='1fd8d198ff6ad69a5aec6fd85bf489d91ae2c432560b1e8ba7e34f710463c80a'
OFFICIAL_CONFIG_SHA='85825ca6e18cbe539eb83ee09eedfb3f4222265929f06e9f535a6d9364f55899'
NAMES=['DeepseekV3RMSNorm','DeepseekV3RotaryEmbedding','DeepseekV3LinearScalingRotaryEmbedding',
       'DeepseekV3DynamicNTKScalingRotaryEmbedding','yarn_find_correction_dim','yarn_find_correction_range',
       'yarn_get_mscale','yarn_linear_ramp_mask','DeepseekV3YarnRotaryEmbedding','rotate_half',
       'apply_rotary_pos_emb','DeepseekV3MLP','DeepseekV3Attention','DeepseekV3DecoderLayer']


def definitions():
    import torch
    from torch import nn
    path=Path(__file__).with_name('reference')/'modeling_deepseek.py'
    data=path.read_bytes()
    if sha(data)!=REFERENCE_SHA: raise ValueError('official reference source pin mismatch')
    config=(path.parent/'config.json').read_bytes()
    if sha(config)!=OFFICIAL_CONFIG_SHA: raise ValueError('official config pin mismatch')
    tree=ast.parse(data,filename=str(path))
    nodes={n.name:n for n in tree.body if isinstance(n,(ast.ClassDef,ast.FunctionDef))}
    env={'torch':torch,'nn':nn,'math':math,'warnings':warnings,'Optional':typing.Optional,'Tuple':typing.Tuple,
         'DeepseekV3Config':types.SimpleNamespace,'Cache':object,'ACT2FN':{'silu':torch.nn.functional.silu},
         'logger':types.SimpleNamespace(warning_once=lambda text:None)}
    for name in NAMES:
        if name=='DeepseekV3DecoderLayer': env['ATTENTION_CLASSES']={'eager':env['DeepseekV3Attention']}
        exec(compile(ast.Module(body=[nodes[name]],type_ignores=[]),str(path),'exec'),env)
    return env


def execute(source_dir, source_pin, original_source_dir, request, request_bytes, original_source_pin=None):
    validate_request(request, request_bytes)
    import torch
    from safetensors import safe_open
    torch.set_num_threads(1)
    torch.use_deterministic_algorithms(True)
    torch.manual_seed(0)
    pin_bytes=Path(source_pin).read_bytes();pin=json.loads(pin_bytes)
    if sha(pin_bytes)!=request['source_manifest_sha256']: raise ValueError('source provenance mismatch')
    source_dir=Path(source_dir);original_source_dir=Path(original_source_dir)
    config_bytes=(source_dir/'config.json').read_bytes()
    if sha(config_bytes)!=request['config_sha256']: raise ValueError('config mismatch')
    config=json.loads(config_bytes)['text_config']
    original_config=json.loads((original_source_dir/'config.json').read_bytes())['text_config']
    original_pin=Path(original_source_pin or original_source_dir/'tiny-kimi-packed.source.json').read_bytes()
    if sha(original_pin)!=request['original_source_manifest_sha256']: raise ValueError('original source mismatch')
    graph={'embedding':'original','executed_layers':[0],'head':'original','source_layer_count':original_config['num_hidden_layers']}
    if request['graph']!=graph or config['first_k_dense_replace']<1:
        raise ValueError('reference supports declared original dense layer 0 only')
    if request['positions']!=list(range(len(request['token_ids']))) or request['mask']!='causal_no_padding':
        raise ValueError('position/masking mismatch')
    if request['shape']!={'hidden_size':config['hidden_size'],'vocab_size':config['vocab_size']}:
        raise ValueError('model shape mismatch')
    expected_config=dict(original_config);expected_config['num_hidden_layers']=1
    if config not in (expected_config, original_config): raise ValueError('reduced graph changed original model fields')
    config['num_hidden_layers']=1
    env=definitions()
    config.update(_attn_implementation='eager',attention_dropout=0.0)
    cfg=types.SimpleNamespace(**config)
    layer=env['DeepseekV3DecoderLayer'](cfg,layer_idx=0).eval()
    norm=env['DeepseekV3RMSNorm'](cfg.hidden_size,eps=cfg.rms_norm_eps).eval()
    index=json.loads((source_dir/'model.safetensors.index.json').read_text())['weight_map']
    pins={f['name']:f for f in pin['files']}
    pins[pin['index']['name']]=pin['index']
    used={}
    verified={}
    def verify_file(directory, filename, expected):
        import hashlib
        path=directory/filename
        digest=hashlib.sha256()
        with path.open('rb') as f:
            for chunk in iter(lambda:f.read(1024*1024), b''): digest.update(chunk)
        if path.stat().st_size != expected['bytes'] or digest.hexdigest() != expected['sha256']:
            raise ValueError('source file pin mismatch')
        return digest.hexdigest()
    original_doc=json.loads(original_pin)
    original_pins={f['name']:f for f in original_doc['files']}
    original_pins[original_doc['index']['name']]=original_doc['index']
    for directory, entries in [(source_dir,pins),(original_source_dir,original_pins)]:
        for filename in ['config.json','model.safetensors.index.json']:
            verify_file(directory,filename,entries[filename])
    def tensor(name):
        name='language_model.'+name
        filename=index[name];p=source_dir/filename
        if filename not in verified:
            digest=verify_file(source_dir,filename,pins[filename])
            if verify_file(original_source_dir,filename,original_pins[filename]) != digest:
                raise ValueError('selected weights differ from original fixture')
            verified[filename]=digest
        with safe_open(p,framework='pt',device='cpu') as f:
            raw_tensor=f.get_tensor(name)
            if raw_tensor.dtype != torch.bfloat16: raise ValueError('expected BF16 reference source')
            t=raw_tensor.float()
        if not torch.isfinite(t).all(): raise ValueError('nonfinite reference weights')
        used[name]={'file':filename,'file_sha256':verified[filename],'shape':list(t.shape),'source_dtype':'BF16','compute_dtype':'float32'}
        return t
    state={name:tensor('model.layers.0.'+name) for name in layer.state_dict()}
    layer.load_state_dict(state,strict=True)
    del state
    norm.load_state_dict({'weight':tensor('model.norm.weight')},strict=True)
    embed=tensor('model.embed_tokens.weight');head=tensor('lm_head.weight')
    ids=torch.tensor([request['token_ids']],dtype=torch.long)
    positions=torch.tensor([request['positions']],dtype=torch.long)
    length=ids.shape[1]
    mask=torch.zeros((1,1,length,length),dtype=torch.float32)
    mask.masked_fill_(torch.ones((length,length),dtype=torch.bool).triu(1),torch.finfo(torch.float32).min)
    with torch.inference_mode():
        h0=torch.nn.functional.embedding(ids,embed)
        h1=layer(h0,attention_mask=mask,position_ids=positions,use_cache=False)[0]
        logits=torch.nn.functional.linear(norm(h1),head)
    if not torch.isfinite(h1).all() or not torch.isfinite(logits).all():raise ValueError('nonfinite reference output')
    return {'schema':'arc.layer-probe.raw.v1','engine':'official-pytorch-fp32','alignment':request,
            'provenance':{'reference_sha256':REFERENCE_SHA,'official_config_sha256':OFFICIAL_CONFIG_SHA,
                'request_sha256':sha(request_bytes),'source_manifest_sha256':sha(pin_bytes),
                'package_sha256':request['package_sha256'],'torch_version':torch.__version__,
                'device':'cpu','reference_definitions':NAMES,'weight_tensors':used,
                'execution':'unmodified pinned eager decoder forward; source BF16 decoded to FP32, no cache'},
            'numeric':{'dtype':'float32','fraction_bits':None,'unit_scale':1.0},
            'tensors':{'layer.0.output':h1[0].tolist(),'logits':logits[0].tolist()}}
