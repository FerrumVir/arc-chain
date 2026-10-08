#!/usr/bin/env python3
"""Read-only, hash-verified local BF16 admission census; never admission approval."""
import argparse
from functools import lru_cache
import hashlib
import json
import math
from pathlib import Path
import re
import struct
import sys

import numpy as np

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from arc_conformance.mla_moe_reference import expected_sources, weights_from_config

ENGINE = '616ba16a60f43b5e70666ca24f5d7f9ce99ab932'
CLASSES = ('attention', 'dense', 'shared', 'embedding', 'head')
CATEGORIES = ('zero', 'below_range', 'in_range', 'at_or_above_range', 'nonfinite')
LIMIT = 16 * 1024 * 1024  # JSON metadata only; payload buffers are separately bounded.


def strict_object(pairs):
    out = {}
    for key, value in pairs:
        if key in out:
            raise ValueError(f'duplicate JSON key: {key}')
        out[key] = value
    return out


def decode(data):
    return json.loads(data, object_pairs_hook=strict_object,
                      parse_constant=lambda x: (_ for _ in ()).throw(ValueError(x)))


def small_json(path):
    with path.open('rb') as f:
        data = f.read(LIMIT + 1)
    if len(data) > LIMIT:
        raise ValueError('JSON metadata exceeds 16 MiB')
    return decode(data), hashlib.sha256(data).hexdigest()


def policy(value):
    if not isinstance(value, dict) or set(value) != {'version', *CLASSES}:
        raise ValueError('precision requires version and exactly all five classes')
    if type(value['version']) is not int or value['version'] != 1:
        raise ValueError('precision version must be 1')
    if any(value[c] not in ('int8', 'int16') for c in CLASSES):
        raise ValueError('precision selections must be int8 or int16')
    return value


@lru_cache(maxsize=65536)
def category(maximum, bits):
    # Sign-cleared BF16 encodings sort by magnitude. Any NaN/Inf wins over
    # finite entries; +/-zero is the explicit converter exception.
    if maximum >= 0x7f80:
        return 'nonfinite'
    if maximum == 0:
        return 'zero'
    exponent, fraction = maximum >> 7, maximum & 127
    mantissa, power = (fraction, -133) if exponent == 0 else (128 + fraction, exponent - 134)
    qmax = 127 if bits == 'int8' else 32767
    shift = 0
    while mantissa << shift < qmax << 30:
        shift += 1
    mu = (2 * (mantissa << shift) + qmax) // (2 * qmax)
    if mu == 1 << 31:
        shift -= 1
    k = shift - power
    # Derived from the actual dyadic scale, rather than an approximate float
    # interval. For INT16 this agrees with the engine's explicit precheck.
    if k > 62:
        return 'below_range'
    if k < 16:
        return 'at_or_above_range'
    return 'in_range'


def empty_counts():
    return dict.fromkeys(CATEGORIES, 0)


def verify(f, entry):
    f.seek(0)
    digest = hashlib.sha256()
    size = 0
    while chunk := f.read(1024 * 1024):
        size += len(chunk)
        digest.update(chunk)
    if size != entry['bytes'] or digest.hexdigest() != entry['sha256']:
        raise ValueError(f"source hash/length mismatch: {entry['name']}")
    f.seek(0)


def entries(manifest):
    if manifest.get('schema') != 'arc.hf-source.v1':
        raise ValueError('unsupported source manifest schema')
    result = {}
    for entry in manifest['files'] + ([manifest['index']] if 'index' in manifest else []):
        name = entry['name']
        if (not isinstance(name, str) or not name or name.startswith('.') or '/' in name or '\\' in name or ':' in name
                or name in result or type(entry['bytes']) is not int or entry['bytes'] < 0
                or not re.fullmatch('[0-9a-f]{64}', entry['sha256'])):
            raise ValueError('invalid or duplicate source manifest entry')
        result[name] = entry
    if 'config.json' not in result:
        raise ValueError('source manifest has no config.json')
    return result


def tensor_header(f, size):
    raw = f.read(8)
    if len(raw) != 8:
        raise ValueError('truncated safetensors length')
    n = struct.unpack('<Q', raw)[0]
    if n > LIMIT or 8 + n > size:
        raise ValueError('invalid or oversized safetensors header')
    header = decode(f.read(n))
    spans = []
    for name, info in header.items():
        if name == '__metadata__':
            continue
        shape, offsets, dtype = info['shape'], info['data_offsets'], info['dtype']
        if (dtype not in ('BF16', 'F32', 'I32') or not isinstance(shape, list)
                or any(type(d) is not int or d <= 0 for d in shape)
                or not isinstance(offsets, list) or len(offsets) != 2
                or any(type(x) is not int for x in offsets)):
            raise ValueError(f'invalid tensor metadata: {name}')
        begin, end = offsets
        if begin < 0 or end > size - 8 - n or end - begin != math.prod(shape) * (2 if dtype == 'BF16' else 4):
            raise ValueError(f'invalid tensor extent: {name}')
        spans.append((begin, end))
    cursor = 0
    for begin, end in sorted(spans):
        if begin != cursor:
            raise ValueError('overlap or gap in safetensors payload')
        cursor = end
    if cursor != size - 8 - n:
        raise ValueError('unclaimed safetensors bytes')
    return header, 8 + n


def tensor_class(name, shape):
    if '.experts.' in name:
        return None, 'native routed expert (including packed scales and shape)'
    if '.mlp.gate.' in name:
        return None, 'native router or correction bias'
    if len(shape) != 2:
        return None, 'native norm or vector'
    if name == 'model.embed_tokens.weight':
        return 'embedding', None
    if name == 'lm_head.weight':
        return 'head', None
    if '.shared_experts.' in name:
        return 'shared', None
    if '.mlp.' in name:
        return 'dense', None
    return 'attention', None


def scan_matrix(f, offset, rows, cols, bits, chunk_elements):
    counts = empty_counts()
    f.seek(offset)
    for _ in range(rows):
        maximum = 0
        for start in range(0, cols, chunk_elements):
            count = min(chunk_elements, cols - start)
            data = f.read(2 * count)
            if len(data) != 2 * count:
                raise ValueError('source changed during scan')
            maximum = max(maximum, int((np.frombuffer(data, dtype='<u2') & 0x7fff).max()))
        counts[category(maximum, bits)] += 1
    return counts


def scan_kv(f, offset, m, bits, chunk_elements):
    h, rank, nope, value = (m[k] for k in ('n_heads', 'kv_lora_rank', 'qk_nope_dim', 'v_head_dim'))
    key_counts, value_counts = empty_counts(), empty_counts()
    for head in range(h):
        for column in range(0, rank, chunk_elements):
            width = min(chunk_elements, rank - column)
            maxima = np.zeros(width, dtype=np.uint16)
            for n in range(nope):
                f.seek(offset + 2 * ((head * (nope + value) + n) * rank + column))
                data = f.read(2 * width)
                if len(data) != 2 * width:
                    raise ValueError('source changed during scan')
                np.maximum(maxima, np.frombuffer(data, dtype='<u2') & 0x7fff, out=maxima)
            for maximum in maxima:
                key_counts[category(int(maximum), bits)] += 1
        part = scan_matrix(f, offset + 2 * (head * (nope + value) + nope) * rank,
                           value, rank, bits, chunk_elements)
        for key in CATEGORIES:
            value_counts[key] += part[key]
    return [('key_transposed_head_rank_nope', [h * rank, nope], key_counts),
            ('value_head_value_rank', [h * value, rank], value_counts)]


def census(source, manifest_path, selected, chunk_elements=32768):
    policy(selected)
    if not 1 <= chunk_elements <= 524288:
        raise ValueError('chunk-elements must be in [1,524288]')
    manifest, manifest_hash = small_json(manifest_path)
    pinned = entries(manifest)
    config_path = source / 'config.json'
    with config_path.open('rb') as f:
        verify(f, pinned['config.json'])
        config_bytes = f.read(LIMIT + 1)
        if len(config_bytes) > LIMIT:
            raise ValueError('config metadata exceeds 16 MiB')
        config = decode(config_bytes)
        verify(f, pinned['config.json'])
    m, prefix, packed, pending = weights_from_config(config, manifest['max_seq'])
    expected = expected_sources(m, prefix, packed)
    found, records, native, verified, absent = set(), [], [], [pinned['config.json']], []
    for name, entry in sorted(pinned.items()):
        if name == 'config.json':
            continue
        path = source / name
        if not path.exists():
            if name.endswith('.safetensors'):
                absent.append(name)
                continue
            raise ValueError(f'missing pinned metadata file: {name}')
        with path.open('rb') as f:
            verify(f, entry)
            if name.endswith('.safetensors'):
                header, start = tensor_header(f, entry['bytes'])
                for full_name, info in sorted(header.items()):
                    if full_name == '__metadata__':
                        continue
                    if full_name in found:
                        raise ValueError(f'duplicate source tensor: {full_name}')
                    found.add(full_name)
                    short = full_name.removeprefix(prefix) if prefix else full_name
                    ignored = (bool(prefix) and full_name.startswith(('vision_tower.', 'mm_projector.'))
                               or short.endswith('.rotary_emb.inv_freq'))
                    if ignored:
                        native.append(dict(tensor=full_name, source=name, reason='outside language conversion (ignored)',
                                           dtype=info['dtype'], shape=info['shape']))
                        continue
                    if prefix and not full_name.startswith(prefix):
                        raise ValueError(f'missing language_model prefix: {full_name}')
                    if short not in expected:
                        raise ValueError(f'unexpected source tensor: {full_name}')
                    shape, kind = expected[short]
                    dtypes = {'bf16': ('BF16',), 'bias': ('BF16', 'F32'), 'packed': ('I32',), 'shape': ('I32',)}
                    if tuple(info['shape']) != shape or info['dtype'] not in dtypes[kind]:
                        raise ValueError(f'source tensor shape/dtype mismatch: {full_name}')
                    cls, reason = tensor_class(short, shape)
                    if cls is None:
                        native.append(dict(tensor=full_name, source=name, reason=reason,
                                           dtype=info['dtype'], shape=list(shape)))
                        continue
                    bits = selected[cls]
                    offset = start + info['data_offsets'][0]
                    parts = (scan_kv(f, offset, m, bits, chunk_elements) if short.endswith('.kv_b_proj.weight')
                             else [('row_major', list(shape), scan_matrix(f, offset, *shape, bits, chunk_elements))])
                    for layout, semantic_shape, counts in parts:
                        assert sum(counts.values()) == semantic_shape[0]
                        records.append(dict(tensor=full_name, source=name, source_sha256=entry['sha256'],
                                            tensor_class=cls, precision=bits, layout=layout,
                                            semantic_shape=semantic_shape, rows=semantic_shape[0], counts=counts))
            verify(f, entry)  # Detect mutation during scanning, before publishing a report.
        verified.append(entry)
    missing = sorted(prefix + n for n in expected if prefix + n not in found)
    by_class = {c: dict(precision=selected[c], rows=0, counts=empty_counts()) for c in CLASSES}
    total = empty_counts()
    for record in records:
        aggregate = by_class[record['tensor_class']]
        aggregate['rows'] += record['rows']
        for key in CATEGORIES:
            aggregate['counts'][key] += record['counts'][key]
            total[key] += record['counts'][key]
    return dict(schema='arc.bf16-row-census.v1', engine_dependency=ENGINE, precision=selected,
                source_manifest_sha256=manifest_hash, repo=manifest['repo'], revision=manifest['revision'],
                verified_sources=sorted(verified, key=lambda e: e['name']), absent_shards=absent,
                missing_language_tensors=missing, complete_language_tensor_inventory=not missing,
                pending_preparation=pending, tensors=records, native_or_ignored_tensors=native,
                classes=by_class, rows=sum(total.values()), counts=total,
                scanned_selectable_rows_supported=not any(total[k] for k in ('below_range', 'at_or_above_range', 'nonfinite')),
                admission_approved=False)


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('--source-dir', type=Path, required=True)
    ap.add_argument('--source-manifest', type=Path, required=True)
    ap.add_argument('--precision', type=Path, required=True)
    ap.add_argument('--out', type=Path, required=True)
    ap.add_argument('--chunk-elements', type=int, default=32768)
    a = ap.parse_args()
    try:
        # Exclusive output creation also refuses hard links, symlinks and any
        # pre-existing input alias. Reports may never be written inside source.
        if a.out.resolve().is_relative_to(a.source_dir.resolve()):
            raise ValueError('report output must be outside source directory')
        if a.out.exists() or a.out.is_symlink():
            raise ValueError('report output already exists')
        selected, _ = small_json(a.precision)
        report = census(a.source_dir, a.source_manifest, selected, a.chunk_elements)
        with a.out.open('x', encoding='utf-8', newline='\n') as f:
            f.write(json.dumps(report, indent=2, sort_keys=True) + '\n')
    except (ValueError, OSError, KeyError, TypeError) as e:
        ap.exit(2, f'census: {e}\n')
    print(f"{report['rows']} selectable rows; counts={report['counts']}; admission_approved=false")


if __name__ == '__main__':
    main()
