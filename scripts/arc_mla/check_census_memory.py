#!/usr/bin/env python3
"""Synthetic real-config geometry stress; no model weights, network or conversion."""
import argparse
import hashlib
import json
from pathlib import Path
import resource
import sys
import time

import bf16_row_census as c


class SyntheticBF16:
    """Virtual all-NaN payload: every semantic row needs rejection details."""
    def __init__(self):
        self.bytes_read = 0
        self.max_read = 0

    def seek(self, offset):
        pass

    def read(self, count):
        assert count % 2 == 0
        self.bytes_read += count
        self.max_read = max(self.max_read, count)
        return b'\xc1\x7f' * (count // 2)


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('out', type=Path)
    ap.add_argument('--limit', type=int, default=1000)
    a = ap.parse_args()
    a.out.mkdir(parents=True, exist_ok=False)
    root = Path(__file__).resolve().parents[2]
    config_path = root/'docs/protocol/reference/kimi-k26/config.json'
    manifest_path = root/'docs/protocol/packages/kimi-k26.source.json'
    config, _ = c.small_json(config_path)
    manifest, _ = c.small_json(manifest_path)
    model, prefix, packed, _ = c.weights_from_config(config, manifest['max_seq'])
    expected = c.expected_sources(model, prefix, packed)
    source = SyntheticBF16()
    budget = c.RejectionBudget(a.limit)
    counts = {cls: 0 for cls in (*c.CLASSES, *c.NATIVE_CLASSES)}
    records, native = [], []
    started = time.monotonic()
    for name, (shape, _) in sorted(expected.items()):
        cls, reason = c.tensor_class(name, shape)
        if cls is None:
            native.append(dict(tensor=prefix+name, shape=list(shape), reason=reason))
            continue
        precision = {'norm':'i64_q16', 'router':'int16_power_of_two'}.get(cls, 'int16')
        semantic = [shape[0], 1] if cls == 'norm' else list(shape)
        if name.endswith('.kv_b_proj.weight'):
            parts = c.scan_kv(source, 0, model, precision, 32768, budget)
        else:
            parts = [('norm_scalar_q16' if cls == 'norm' else 'row_major', semantic,
                      c.scan_matrix(source, 0, *semantic, precision, 32768, rejection_budget=budget))]
        for layout, geometry, distribution in parts:
            assert distribution.counts['nonfinite'] == geometry[0]
            assert distribution.rejected_count == geometry[0]
            counts[cls] += geometry[0]
            records.append(dict(tensor=prefix+name, layout=layout, semantic_shape=geometry,
                                tensor_class=cls, **distribution.fields()))
    total = sum(counts.values())
    assert total == 5891392, counts
    assert sum(len(r['rejected_rows']) for r in records) == min(a.limit, total)
    assert sum(r['rejected_rows_omitted'] for r in records) == total - min(a.limit, total)
    with (a.out/'synthetic-report.json').open('x') as f:
        json.dump(dict(synthetic=True, rows=total, classes=counts, tensors=records, native_or_ignored_tensors=native), f, indent=2)
    # ru_maxrss includes scanning and output serialization; Mac returns bytes,
    # Linux returns KiB. This process runs only one stress case.
    raw_rss = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
    summary = dict(synthetic=True, real_weights_read=False, geometry='full pinned K2.6 config',
                   source='virtual all-NaN BF16; actual row widths and KV-B transpose/split',
                   rows=total, classes=counts, rejected_row_count=total,
                   native_inventory_records=len(native),
                   semantic_components=len(records),
                   semantic_shapes=[dict(tensor=r['tensor'], layout=r['layout'], shape=r['semantic_shape']) for r in records],
                   output_size_bytes=(a.out/'synthetic-report.json').stat().st_size,
                   rejection_detail_limit=a.limit, retained=min(a.limit,total), omitted=total-min(a.limit,total),
                   peak_rss_bytes=int(raw_rss if sys.platform=='darwin' else raw_rss*1024),
                   elapsed_seconds=time.monotonic()-started,
                   virtual_bytes_scanned=source.bytes_read,largest_payload_read_bytes=source.max_read,
                   config_sha256=hashlib.sha256(config_path.read_bytes()).hexdigest(),
                   platform=sys.platform, caveat='Synthetic geometry, native inventory and flagged-output stress; no safetensors headers, hash I/O or real weights. Not a real-census RSS guarantee.')
    (a.out/'memory.json').write_text(json.dumps(summary, indent=2)+'\n')
    print(json.dumps(summary,indent=2),flush=True)


if __name__ == '__main__':
    main()
