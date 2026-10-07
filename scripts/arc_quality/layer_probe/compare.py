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
                'original_source_manifest_sha256', 'package_sha256', 'reference_sha256'}
    if set(request) != required:
        raise ValueError('request fields mismatch')
    if request['engine_sha'] != 'b0673684a8c16f83b318fe1a33ab6b33f3dc79df' or request['reference_sha256'] != REFERENCE_SHA:
        raise ValueError('implementation pin mismatch')
    for key in required:
        if key.endswith('sha256') or key == 'model_root':
            value = request[key]
            if not isinstance(value, str) or len(value) != 64 or any(c not in '0123456789abcdef' for c in value):
                raise ValueError('invalid digest')
    graph = request['graph']
    if set(graph) != {'embedding', 'head', 'executed_layers', 'source_layer_count'} or graph['embedding'] != 'original' or graph['head'] != 'original' or graph['executed_layers'] != [0] or type(graph['source_layer_count']) is not int or graph['source_layer_count'] < 1:
        raise ValueError('unsupported graph')
    if request['scope'] not in ({'kind': 'synthetic_fixture'}, {'kind': 'early_layers_with_head_probe', 'layers': 1}):
        raise ValueError('unsupported scope')
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
    expected_provenance = {
        'request_sha256': sha(request_bytes),
        'package_sha256': request['package_sha256'],
        'source_manifest_sha256': request['source_manifest_sha256'],
    }
    outputs = []
    for doc, engine, numeric in [(arc, 'arc-integer', {'dtype':'int64','fraction_bits':16,'unit_scale':2**-16}),
                                 (reference, 'official-pytorch-fp32', {'dtype':'float32','fraction_bits':None,'unit_scale':1.0})]:
        if doc.get('schema') != 'arc.layer-probe.raw.v1' or doc.get('engine') != engine:
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
        if set(doc.get('tensors',{})) != {'layer.0.output','logits'}:
            raise ValueError('missing or unexpected tensors')
        data = {}
        for name,width in [('layer.0.output',request['shape']['hidden_size']),('logits',request['shape']['vocab_size'])]:
            raw = doc['tensors'][name]
            if not isinstance(raw,list) or len(raw)!=len(request['token_ids']): raise ValueError('missing positions')
            for row in raw:
                if not isinstance(row,list) or len(row)!=width: raise ValueError('tensor shape mismatch')
                for x in row:
                    if type(x) not in (int,float): raise ValueError('nonnumeric tensor')
                    if engine=='arc-integer' and (type(x) is not int or not -(2**63)<=x<2**63):
                        raise ValueError('invalid integer activation')
                    if not math.isfinite(x): raise ValueError('nonfinite tensor')
            data[name] = np.asarray(raw,dtype=np.float64) * numeric['unit_scale']
        outputs.append(data)
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
             'relative_denominator_floor':1e-8}
        if any(not math.isfinite(v) for v in row.values()): raise ValueError('nonfinite metrics')
        metrics[name]=row
    return {'schema':'arc.layer-probe.comparison.v1','status':'MEASURED_NON_CERTIFYING',
            'certified':False,'policy_status':'PROPOSED, not approved by TJ',
            'scope':request['scope'],
            'graph':request['graph'], 'complete_kimi_forward':False,
            'alignment_sha256':sha(canonical(request)), 'request_sha256':sha(request_bytes),
            'arc_raw_sha256':sha(canonical(arc)), 'reference_raw_sha256':sha(canonical(reference)),
            'metrics':metrics}
