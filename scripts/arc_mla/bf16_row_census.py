#!/usr/bin/env python3
"""Read-only, hash-verified local BF16 admission census; never admission approval."""
import argparse
from functools import lru_cache
from fractions import Fraction
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
NATIVE_CLASSES = ('norm', 'router')
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
    if name.endswith('.mlp.gate.weight'):
        return 'router', None
    if '.mlp.gate.' in name:
        return None, 'native correction bias'
    if len(shape) == 1 and (name == 'model.norm.weight' or 'layernorm.weight' in name):
        return 'norm', None
    if len(shape) != 2:
        return None, 'native vector'
    if name == 'model.embed_tokens.weight':
        return 'embedding', None
    if name == 'lm_head.weight':
        return 'head', None
    if '.shared_experts.' in name:
        return 'shared', None
    if '.mlp.' in name:
        return 'dense', None
    return 'attention', None


def actual_category(maximum, precision):
    if precision in ('int8', 'int16'):
        return category(maximum, precision)
    if maximum >= 0x7f80:
        return 'nonfinite'
    if maximum == 0:
        return 'zero'
    # Norm is a scalar Q16 conversion, including defined rounding to zero for
    # tiny values. Router rows have their own power-of-two scale, not dyadic mu.
    lower, upper = ((0x2780, 0x4700) if precision == 'int16_power_of_two'
                    else (1, 0x5680))  # router [2^-48,2^15); norm |w| < 2^46
    return ('below_range' if maximum < lower else
            'at_or_above_range' if maximum >= upper else 'in_range')


def magnitude(bits):
    exponent, fraction = bits >> 7, bits & 127
    mantissa, power = (fraction, -133) if exponent == 0 else (128 + fraction, exponent - 134)
    return Fraction(mantissa) * Fraction(2) ** power


def statistics(histogram):
    # Finite row maxima only. Even medians are the arithmetic mean of the two
    # middle observations, not a median of per-tensor medians.
    populated = np.flatnonzero(histogram[:0x7f80])
    count = int(histogram[:0x7f80].sum())
    if not count:
        return dict(finite_rows=0, nonfinite_rows=int(histogram[0x7f80:].sum()),
                    min=None, median=None, max=None, median_exact=None, median_bf16_pair=None)
    cumulative = np.cumsum(histogram[:0x7f80])
    lo, hi = [int(np.searchsorted(cumulative, rank + 1)) for rank in ((count-1)//2, count//2)]
    median = (magnitude(lo) + magnitude(hi)) / 2
    return dict(finite_rows=count, nonfinite_rows=int(histogram[0x7f80:].sum()),
                min=float(magnitude(int(populated[0]))), median=float(median),
                max=float(magnitude(int(populated[-1]))),
                median_exact=dict(numerator=str(median.numerator), denominator=str(median.denominator)),
                median_bf16_pair=[f'0x{lo:04x}', f'0x{hi:04x}'])


class RejectionBudget:
    """One shared limit for the entire report; counts are never capped."""
    def __init__(self, limit=1000):
        if type(limit) is not int or not 0 <= limit <= 100000:
            raise ValueError('max-rejected-rows must be in [0,100000]')
        self.limit = limit
        self.remaining = limit


class Distribution:
    """Fixed-size histogram and explicitly capped rejection details."""
    def __init__(self, precision, rejection_budget=None):
        self.precision = precision
        self.histogram = np.zeros(32768, dtype=np.uint64)
        self.counts = empty_counts()
        self.window = empty_counts()
        self.intersection = 0
        self.rejected = []
        self.rejected_count = 0
        self.rejection_budget = rejection_budget if rejection_budget is not None else RejectionBudget()

    def add(self, maximum, row, coordinates):
        actual = actual_category(maximum, self.precision)
        wide = category(maximum, 'int16')
        narrow = category(maximum, 'int8')
        self.histogram[maximum] += 1
        self.counts[actual] += 1
        self.window[wide] += 1
        intersection = narrow in ('zero', 'in_range') and wide not in ('zero', 'in_range')
        self.intersection += int(intersection)
        actual_bad = actual not in ('zero', 'in_range')
        wide_bad = wide not in ('zero', 'in_range')
        if actual_bad or wide_bad:
            self.rejected_count += 1
            if self.rejection_budget.remaining == 0:
                return
            self.rejection_budget.remaining -= 1
            self.rejected.append(dict(semantic_row=row, coordinates=coordinates,
                                      maximum_abs_bf16=f'0x{maximum:04x}',
                                      actual_admission=actual if actual_bad else None,
                                      int16_matrix_window=wide if wide_bad else None,
                                      int8_accepted_int16_rejected=intersection))

    def fields(self):
        return dict(counts=self.counts, int16_window_counts=self.window,
                    int8_accepted_int16_rejected=self.intersection,
                    row_max_abs=statistics(self.histogram), rejected_rows=self.rejected,
                    rejected_row_count=self.rejected_count,
                    rejected_rows_omitted=self.rejected_count - len(self.rejected),
                    rejected_rows_truncated=self.rejected_count > len(self.rejected))


def scan_matrix(f, offset, rows, cols, bits, chunk_elements, distribution=None, row_base=0, head=None, rejection_budget=None):
    result = distribution if distribution is not None else Distribution(bits, rejection_budget)
    f.seek(offset)
    for row in range(rows):
        maximum = 0
        for start in range(0, cols, chunk_elements):
            count = min(chunk_elements, cols - start)
            data = f.read(2 * count)
            if len(data) != 2 * count:
                raise ValueError('source changed during scan')
            maximum = max(maximum, int((np.frombuffer(data, dtype='<u2') & 0x7fff).max()))
        result.add(maximum, row_base + row, [row] if head is None else [head, row])
    return result


def scan_kv(f, offset, m, bits, chunk_elements, rejection_budget=None):
    h, rank, nope, value = (m[k] for k in ('n_heads', 'kv_lora_rank', 'qk_nope_dim', 'v_head_dim'))
    key_result, value_result = Distribution(bits, rejection_budget), Distribution(bits, rejection_budget)
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
            for i, maximum in enumerate(maxima):
                key_result.add(int(maximum), head * rank + column + i, [head, column + i])
        scan_matrix(f, offset + 2 * (head * (nope + value) + nope) * rank,
                    value, rank, bits, chunk_elements, value_result, head * value, head)
    return [('key_transposed_head_rank_nope', [h * rank, nope], key_result),
            ('value_head_value_rank', [h * value, rank], value_result)]


def census(source, manifest_path, selected, chunk_elements=32768, max_rejected_rows=1000):
    policy(selected)
    rejection_budget = RejectionBudget(max_rejected_rows)
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
    precisions = dict(selected, norm='i64_q16', router='int16_power_of_two')
    aggregates = {c: Distribution(precisions[c]) for c in (*CLASSES, *NATIVE_CLASSES)}
    total = Distribution('aggregate')
    tensor_totals = []
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
                    bits = precisions[cls]
                    offset = start + info['data_offsets'][0]
                    semantic = [shape[0], 1] if cls == 'norm' else list(shape)
                    layout = 'norm_scalar_q16' if cls == 'norm' else 'router_expert_row' if cls == 'router' else 'row_major'
                    parts = (scan_kv(f, offset, m, bits, chunk_elements, rejection_budget) if short.endswith('.kv_b_proj.weight')
                             else [(layout, semantic, scan_matrix(f, offset, *semantic, bits, chunk_elements, rejection_budget=rejection_budget))])
                    tensor_total = Distribution(bits)
                    for layout, semantic_shape, distribution in parts:
                        for aggregate in (aggregates[cls], total, tensor_total):
                            aggregate.histogram += distribution.histogram
                            aggregate.intersection += distribution.intersection
                            aggregate.rejected_count += distribution.rejected_count
                            for key in CATEGORIES:
                                aggregate.counts[key] += distribution.counts[key]
                                aggregate.window[key] += distribution.window[key]
                        if sum(distribution.counts.values()) != semantic_shape[0]:
                            raise ValueError('semantic row count mismatch')
                        records.append(dict(tensor=full_name, source=name, source_sha256=entry['sha256'],
                                            source_shape=list(shape), source_dtype=info['dtype'],
                                            tensor_class=cls, precision=bits, layout=layout,
                                            semantic_shape=semantic_shape, rows=semantic_shape[0],
                                            **distribution.fields()))
                    tensor_fields = tensor_total.fields()
                    tensor_fields.pop('rejected_rows')
                    for key in ('rejected_rows_omitted', 'rejected_rows_truncated'):
                        tensor_fields.pop(key)
                    tensor_totals.append(dict(tensor=full_name, source=name, source_sha256=entry['sha256'],
                                              tensor_class=cls, precision=bits, source_dtype=info['dtype'],
                                              source_shape=list(shape), components=[part[0] for part in parts],
                                              rows=sum(tensor_total.counts.values()), **tensor_fields))
            verify(f, entry)  # Detect mutation during scanning, before publishing a report.
        verified.append(entry)
    missing = sorted(prefix + n for n in expected if prefix + n not in found)
    by_class = {}
    for cls, aggregate in aggregates.items():
        fields = aggregate.fields()
        fields.pop('rejected_rows')  # identities live once, under tensor/component records
        for key in ('rejected_rows_omitted', 'rejected_rows_truncated'):
            fields.pop(key)
        by_class[cls] = dict(precision=precisions[cls], rows=sum(aggregate.counts.values()), **fields)
    supported = lambda classes: not any(aggregates[c].counts[k] for c in classes
                                        for k in ('below_range', 'at_or_above_range', 'nonfinite'))
    fields = total.fields()
    fields.pop('rejected_rows')
    retained = max_rejected_rows - rejection_budget.remaining
    fields['rejected_rows_omitted'] = total.rejected_count - retained
    fields['rejected_rows_truncated'] = total.rejected_count > retained
    return dict(schema='arc.bf16-row-census.v3',
                rejection_detail_limit=max_rejected_rows, rejected_rows_retained=retained, engine_dependency=ENGINE, precision=selected,
                source_manifest_sha256=manifest_hash, repo=manifest['repo'], revision=manifest['revision'],
                verified_sources=sorted(verified, key=lambda e: e['name']), absent_shards=absent,
                missing_language_tensors=missing, complete_language_tensor_inventory=not missing,
                pending_preparation=pending, tensors=records, tensor_totals=tensor_totals, native_or_ignored_tensors=native,
                classes=by_class, rows=sum(total.counts.values()), **fields,
                scanned_selectable_rows_supported=supported(CLASSES),
                scanned_native_rows_supported=supported(NATIVE_CLASSES),
                admission_approved=False)


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('--source-dir', type=Path, required=True)
    ap.add_argument('--source-manifest', type=Path, required=True)
    ap.add_argument('--precision', type=Path, required=True)
    ap.add_argument('--out', type=Path, required=True)
    ap.add_argument('--chunk-elements', type=int, default=32768)
    ap.add_argument('--max-rejected-rows', type=int, default=1000,
                    help='global detail cap [0,100000]; full counts always retained')
    a = ap.parse_args()
    try:
        # Exclusive output creation also refuses hard links, symlinks and any
        # pre-existing input alias. Reports may never be written inside source.
        if a.out.resolve().is_relative_to(a.source_dir.resolve()):
            raise ValueError('report output must be outside source directory')
        if a.out.exists() or a.out.is_symlink():
            raise ValueError('report output already exists')
        selected, _ = small_json(a.precision)
        report = census(a.source_dir, a.source_manifest, selected, a.chunk_elements, a.max_rejected_rows)
        with a.out.open('x', encoding='utf-8', newline='\n') as f:
            json.dump(report, f, indent=2, sort_keys=True)
            f.write('\n')
    except (ValueError, OSError, KeyError, TypeError) as e:
        ap.exit(2, f'census: {e}\n')
    print(f"{report['rows']} distribution rows; counts={report['counts']}; admission_approved=false")


if __name__ == '__main__':
    main()
