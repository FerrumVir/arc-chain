"""Strict alignment and finite-value checks before numerical comparison."""
import hashlib
import json
import math


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(',', ':'), allow_nan=False).encode()


def sha(data):
    return hashlib.sha256(data).hexdigest()


def sha_file(path):
    digest = hashlib.sha256()
    with open(path, 'rb') as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b''):
            digest.update(chunk)
    return digest.hexdigest()


def validate_request(request, request_bytes):
    from .official import REFERENCE_SHA
    if json.loads(request_bytes) != request:
        raise ValueError('request bytes mismatch')
    required = {'engine_sha', 'model_root', 'scope', 'graph', 'shape', 'token_ids',
                'positions', 'mask', 'source_manifest_sha256', 'config_sha256',
                'original_source_manifest_sha256', 'package_sha256', 'reference_sha256', 'precision'}
    if len(request.get('graph',{}).get('executed_layers',[]))>1: required.add('moe')
    if set(request) != required:
        raise ValueError('request fields mismatch')
    if request['engine_sha'] != '05afa5b068268860e4206307fb44c909645557ed' or request['reference_sha256'] != REFERENCE_SHA:
        raise ValueError('implementation pin mismatch')
    for key in required:
        if key.endswith('sha256') or key == 'model_root':
            value = request[key]
            if not isinstance(value, str) or len(value) != 64 or any(c not in '0123456789abcdef' for c in value):
                raise ValueError('invalid digest')
    from .policies import validate
    validate(request['precision'])
    graph = request['graph']
    if set(graph) != {'embedding', 'head', 'executed_layers', 'source_layer_count'} or graph['embedding'] != 'original' or graph['head'] != 'original' or graph['executed_layers'] not in ([0], [0,1], [0,1,2]) or type(graph['source_layer_count']) is not int or graph['source_layer_count'] < len(graph['executed_layers']):
        raise ValueError('unsupported graph')
    if any(type(i) is not int for i in graph['executed_layers']): raise ValueError('invalid graph indices')
    if request['scope'] not in ({'kind': 'synthetic_fixture'}, {'kind': 'early_layers_with_head_probe', 'layers': 1}):
        raise ValueError('unsupported scope')
    if len(graph['executed_layers'])>1 and request['scope'] != {'kind':'synthetic_fixture'}:
        raise ValueError('multiple layers limited to synthetic fixtures')
    if 'moe' in request:
        moe=request['moe']
        if set(moe)!={'n_routed_experts','num_experts_per_tok','n_shared_experts','n_group','topk_group'} or any(type(x) is not int or x<=0 for x in moe.values()) or moe['num_experts_per_tok']>moe['n_routed_experts']:
            raise ValueError('invalid MoE declaration')
    shape = request['shape']
    if set(shape) != {'hidden_size', 'vocab_size'} or any(type(x) is not int or x <= 0 for x in shape.values()):
        raise ValueError('invalid shape')
    ids = request['token_ids']
    if not isinstance(ids, list) or not ids or any(type(x) is not int or not 0 <= x < shape['vocab_size'] for x in ids):
        raise ValueError('invalid token IDs')
    if request['positions'] != list(range(len(ids))) or any(type(x) is not int for x in request['positions']) or request['mask'] != 'causal_no_padding':
        raise ValueError('invalid position/mask')


def compare(arc, reference, request, request_bytes):
    import numpy as np
    validate_request(request, request_bytes)
    layers=request['graph']['executed_layers']
    expected_provenance = {
        'request_sha256': sha(request_bytes),
        'package_sha256': request['package_sha256'],
        'source_manifest_sha256': request['source_manifest_sha256'],
    }
    outputs = []
    for doc, engine, numeric in [(arc, 'arc-integer', {'dtype':'int64','fraction_bits':16,'unit_scale':2**-16}),
                                 (reference, 'official-pytorch-fp32', {'dtype':'float32','fraction_bits':None,'unit_scale':1.0})]:
        if doc.get('schema') != 'arc.layer-probe.raw.v2' or doc.get('engine') != engine:
            raise ValueError('raw schema/engine mismatch')
        if doc.get('alignment') != request or doc.get('numeric') != numeric:
            raise ValueError('model/input/scope/position/scale alignment mismatch')
        if any(doc.get('provenance',{}).get(k) != v for k,v in expected_provenance.items()):
            raise ValueError('provenance mismatch')
        if engine == 'arc-integer' and doc['provenance'].get('engine_sha') != request['engine_sha']:
            raise ValueError('engine revision mismatch')
        if engine != 'arc-integer':
            from .official import OFFICIAL_CONFIG_SHA
            if doc['provenance'].get('reference_sha256') != request['reference_sha256'] or doc['provenance'].get('official_config_sha256') != OFFICIAL_CONFIG_SHA:
                raise ValueError('reference revision mismatch')
        if doc.get('layer_order') != layers: raise ValueError('missing/reordered layer captures')
        if set(doc.get('tensors',{})) != {f'layer.{i}.output' for i in layers} | {'logits'}:
            raise ValueError('missing or unexpected tensors')
        data = {}
        for name,width in [(f'layer.{i}.output',request['shape']['hidden_size']) for i in layers]+[('logits',request['shape']['vocab_size'])]:
            raw = doc['tensors'][name]
            if not isinstance(raw,list) or len(raw)!=len(request['token_ids']): raise ValueError('missing positions')
            for row in raw:
                if not isinstance(row,list) or len(row)!=width: raise ValueError('tensor shape mismatch')
                for x in row:
                    if type(x) not in (int,float): raise ValueError('nonnumeric tensor')
                    if engine=='arc-integer' and (type(x) is not int or not -(2**63)<=x<2**63):
                        raise ValueError('invalid integer activation')
                    if not math.isfinite(x): raise ValueError('nonfinite tensor')
            if doc.get('tensor_sha256',{}).get(name) != sha(canonical(raw)):
                raise ValueError('altered/reordered capture bytes')
            data[name] = np.asarray(raw,dtype=np.float64) * numeric['unit_scale']
        outputs.append(data)
    routing=compare_routing(arc,reference,request)
    metrics = {}
    for name in outputs[0]:
        a,b=outputs[0][name],outputs[1][name]
        error=a-b
        relative=np.abs(error)/np.maximum(np.abs(b),1e-8)
        row={'count':int(error.size),'max_absolute_error':float(np.max(np.abs(error))),
             'mean_absolute_error':float(np.mean(np.abs(error))),
             'rmse':float(np.sqrt(np.mean(error*error))),
             'max_relative_error':float(np.max(relative)),
             'relative_l2_error':float(np.linalg.norm(error)/max(float(np.linalg.norm(b)),1e-8)),
             'max_absolute_error_over_reference_l2':float(np.max(np.abs(error))/max(float(np.linalg.norm(b)),1e-8)),
             'relative_denominator_floor':1e-8}
        if any(not math.isfinite(v) for v in row.values()): raise ValueError('nonfinite metrics')
        metrics[name]=row
    return {'schema':'arc.layer-probe.comparison.v2','status':'MEASURED_NON_CERTIFYING',
            'certified':False,'policy_status':'PROPOSED, not approved by TJ',
            'scope':request['scope'],
            'graph':request['graph'], 'complete_kimi_forward':False,
            'alignment_sha256':sha(canonical(request)), 'request_sha256':sha(request_bytes),
            'arc_raw_sha256':sha(canonical(arc)), 'reference_raw_sha256':sha(canonical(reference)),
            'precision':request['precision'], 'metrics':metrics,'routing_comparison':routing,
            'metric_notice':'Use relative L2 and max absolute error / reference L2 for interpretation; max_relative_error is unbounded and dominated by near-zero elements (1e-8 floor), not a decision metric. No ranking of precision classes from these synthetic fixtures.',
            'top1':{'arc':np.argmax(outputs[0]['logits'],axis=1).tolist(),
                    'reference':np.argmax(outputs[1]['logits'],axis=1).tolist(),
                    'agreements':int(np.sum(np.argmax(outputs[0]['logits'],axis=1)==np.argmax(outputs[1]['logits'],axis=1))),
                    'positions':len(request['positions']), 'tie_rule':'first index'}}


def compare_routing(arc, reference, request):
    import numpy as np
    expected={f'layer.{i}' for i in request['graph']['executed_layers'][1:]}
    for doc,integer in [(arc,True),(reference,False)]:
        if set(doc.get('routing',{})) != expected: raise ValueError('missing/unexpected routing layer')
        numeric={'weights_fraction_bits':32 if integer else None,'activations_fraction_bits':16 if integer else None}
        if doc.get('routing_numeric') != numeric: raise ValueError('routing scale mismatch')
        for layer,rows in doc['routing'].items():
            if len(rows)!=len(request['token_ids']): raise ValueError('missing routing position')
            for pos,row in enumerate(rows):
                if set(row)!={'position','experts','weights','shared','input'} or row['position']!=pos or type(row['position']) is not int: raise ValueError('routing order mismatch')
                experts=row['experts'];moe=request['moe']
                if len(experts)!=moe['num_experts_per_tok'] or len(set(experts))!=len(experts) or any(type(e) is not int or not 0<=e<moe['n_routed_experts'] for e in experts): raise ValueError('invalid routed experts')
                for field,width in [('weights',len(experts)),('shared',request['shape']['hidden_size']),('input',request['shape']['hidden_size'])]:
                    values=row[field]
                    if len(values)!=width or any(type(x) not in (int,float) or not math.isfinite(x) for x in values):raise ValueError('nonfinite/missing routing values')
                    if integer and any(type(x) is not int or not -(2**63)<=x<2**63 for x in values): raise ValueError('invalid integer routing values')
                if any(w<0 for w in row['weights']):raise ValueError('negative routing weight')
    report={}
    for layer in sorted(expected):
        rows=[]
        for a,b in zip(arc['routing'][layer],reference['routing'][layer]):
            shared_a=np.array(a['shared'])*2**-16;shared_b=np.array(b['shared'])
            input_a=np.array(a['input'])*2**-16;input_b=np.array(b['input'])
            wa={e:w*2**-32 for e,w in zip(a['experts'],a['weights'])};wb=dict(zip(b['experts'],b['weights']))
            rows.append({'position':a['position'],'arc_experts':a['experts'],'reference_experts':b['experts'],
                         'same_expert_set':set(a['experts'])==set(b['experts']),
                         'same_order':a['experts']==b['experts'],
                         'expert_weight_l1':sum(abs(wa.get(e,0)-wb.get(e,0)) for e in set(wa)|set(wb)),
                         'shared_max_absolute_error':float(np.max(np.abs(shared_a-shared_b))),
                         'router_input_max_absolute_error':float(np.max(np.abs(input_a-input_b)))})
        report[layer]={'positions':rows,'expert_set_mismatches':sum(not r['same_expert_set'] for r in rows),
                       'note':'Independent native routing; no forced common experts. Shared/input errors reported separately.'}
    return report
