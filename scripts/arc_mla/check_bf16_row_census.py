#!/usr/bin/env python3
"""Synthetic-only census counts checked against actual converter outcomes."""
import argparse
import hashlib
import json
from pathlib import Path
import struct
import subprocess
import sys

import numpy as np

import bf16_row_census as census
import make_tiny_kimi_packed as fixture


POLICIES = {
    'all-int8': dict(version=1, **dict.fromkeys(census.CLASSES, 'int8')),
    'all-int16': dict(version=1, **dict.fromkeys(census.CLASSES, 'int16')),
    'mixed-156': dict(version=1, attention='int16', dense='int8', shared='int16', embedding='int16', head='int8'),
    'mixed-168': dict(version=1, attention='int16', dense='int16', shared='int8', embedding='int8', head='int16'),
}
TARGETS = {
    'attention': 'model.layers.0.self_attn.q_a_proj.weight',
    'dense': 'model.layers.0.mlp.up_proj.weight',
    'shared': 'model.layers.1.mlp.shared_experts.up_proj.weight',
    'embedding': 'model.embed_tokens.weight',
    'head': 'lm_head.weight',
}


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def snapshot(source):
    return {p.name: digest(p) for p in source.iterdir() if p.is_file()}


def expected_counts(matrix, precision):
    # Independent explicit BF16 boundaries, versus census's dyadic-k derivation.
    lower, upper = (0x32fe, 0x4a7e) if precision == 'int8' else (0x3700, 0x4e80)
    out = dict.fromkeys(census.CATEGORIES, 0)
    for row in matrix:
        maximum = max(int(b) & 0x7fff for b in row)
        key = ('nonfinite' if maximum >= 0x7f80 else 'zero' if maximum == 0 else
               'below_range' if maximum < lower else 'at_or_above_range' if maximum >= upper else 'in_range')
        out[key] += 1
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('binary', type=Path)
    ap.add_argument('out', type=Path)
    a = ap.parse_args()
    binary, out = a.binary.resolve(), a.out.resolve()
    out.mkdir(parents=True, exist_ok=False)
    source = out / 'source'
    manifest = fixture.write(source)
    manifest_path = source / 'tiny-kimi-packed.source.json'
    original = {p.name: p.read_bytes() for p in source.iterdir() if p.is_file()}
    tensors = {}
    for name, raw in original.items():
        if name.endswith('.safetensors'):
            n = struct.unpack_from('<Q', raw)[0]
            for tensor, info in json.loads(raw[8:8+n]).items():
                if tensor != '__metadata__':
                    tensors[tensor] = name, info, 8 + n
    results = []
    metadata_controls = []
    policy_paths = {}
    for label, policy in POLICIES.items():
        p = out / (label + '.json')
        p.write_text(json.dumps(policy))
        policy_paths[label] = p
    # Exhaustive finite encoding check derives the two exact acceptance windows.
    for precision in ('int8', 'int16'):
        matrix = np.arange(0x7f80, dtype=np.uint16).reshape(-1, 1)
        expected = expected_counts(matrix, precision)
        actual = census.empty_counts()
        for value in range(0x7f80):
            actual[census.category(value, precision)] += 1
        assert actual == expected, (precision, actual, expected)
    (out / 'exhaustive-windows.json').write_text(json.dumps({p: expected_counts(np.arange(0x7f80).reshape(-1, 1), p)
                                                           for p in ('int8', 'int16')}, indent=2) + '\n')

    def restore():
        for name, data in original.items():
            (source / name).write_bytes(data)

    def change(tensor, values):
        shard, info, start = tensors[tensor]
        raw = original[shard]
        begin, end = info['data_offsets']
        blob = np.array(values, dtype='<u2').tobytes()
        assert len(blob) == end - begin
        (source / shard).write_bytes(raw[:start+begin] + blob + raw[start+end:])
        updated = json.loads(json.dumps(manifest))
        for entry in updated['files']:
            entry['sha256'] = digest(source / entry['name'])
        manifest_path.write_text(json.dumps(updated))

    def run_case(label, cls, tag, values, tensor=None):
        restore()
        tensor = tensor or fixture.PREFIX + TARGETS[cls]
        change(tensor, values)
        case = out / f'{label}-{cls}-{tag}'
        case.mkdir()
        before = snapshot(source)
        selected = POLICIES[label]
        report = census.census(source, manifest_path, selected, chunk_elements=7)
        # Tile-size invariance exercises KV rank tiling and rows spanning chunks.
        assert report == census.census(source, manifest_path, selected, chunk_elements=32768)
        rows = [r for r in report['tensors'] if r['tensor'] == tensor]
        expected = []
        if tensor.endswith('.kv_b_proj.weight'):
            cfg = json.loads(original['config.json'])['text_config']
            h, n, v, rank = (cfg[k] for k in ('num_attention_heads', 'qk_nope_head_dim', 'v_head_dim', 'kv_lora_rank'))
            split = values.reshape(h, n + v, rank)
            expected = [expected_counts(split[:, :n, :].transpose(0, 2, 1).reshape(h * rank, n), selected[cls]),
                        expected_counts(split[:, n:, :].reshape(h * v, rank), selected[cls])]
        else:
            expected = [expected_counts(values, selected[cls])]
        assert [r['counts'] for r in rows] == expected, (tag, rows, expected)
        assert report['rows'] == sum(r['rows'] for r in report['tensors']) == sum(report['counts'].values())
        for c in census.CLASSES:
            assert all(r['precision'] == selected[c] for r in report['tensors'] if r['tensor_class'] == c)
        assert report['complete_language_tensor_inventory']
        assert {r['reason'] for r in report['native_or_ignored_tensors']} == {
            'native routed expert (including packed scales and shape)', 'native router or correction bias',
            'native norm or vector', 'outside language conversion (ignored)'}
        assert snapshot(source) == before, 'census modified source'
        (case / 'report.json').write_text(json.dumps(report, sort_keys=True, indent=2) + '\n')
        command = [str(binary), 'convert', '--source-dir', str(source), '--source-manifest', str(manifest_path),
                   '--experts', 'i4g32', '--precision', str(policy_paths[label]), '--out', str(case / 'output.arcspkg'), '--threads', '1']
        run = subprocess.run(command, capture_output=True, text=True)
        (case / 'stdout.txt').write_text(run.stdout)
        (case / 'stderr.txt').write_text(run.stderr)
        accepted = not any(counts[k] for counts in expected for k in ('below_range', 'at_or_above_range', 'nonfinite'))
        assert report['scanned_selectable_rows_supported'] == accepted
        assert (run.returncode == 0) == accepted, (label, cls, tag, run.stderr)
        if not accepted:
            assert tensor in run.stderr, run.stderr
            if tag == 'kv-key-below':
                assert 'transposed key rows [head,rank,nope]' in run.stderr
                if selected[cls] == 'int16':
                    assert 'conversion row 1' in run.stderr
            if tag == 'kv-value-above':
                assert 'value rows [head,value,rank]' in run.stderr
                if selected[cls] == 'int16':
                    assert 'conversion row 33' in run.stderr
        assert snapshot(source) == before, 'converter modified source'
        results.append(dict(policy=selected, tensor=tensor, case=tag, expected=expected,
                            actual=[r['counts'] for r in rows], accepted=accepted,
                            returncode=run.returncode, source_hashes=before))
        return report

    for label, selected in POLICIES.items():
        for cls, short in TARGETS.items():
            shape = tensors[fixture.PREFIX + short][1]['shape']
            low, high = (0x32fe, 0x4a7e) if selected[cls] == 'int8' else (0x3700, 0x4e80)
            cases = [('zero', 0x8000), ('below', low-1), ('lower', low), ('lower-next', low+1),
                     ('upper-prev', high-1), ('upper', high), ('upper-next', high+1),
                     ('nan', 0x7fc1), ('positive-inf', 0x7f80), ('negative-inf', 0xff80)]
            if label.startswith('mixed'):
                cases = [('between-windows', 0x3600), ('between-upper-windows', 0x4b00), ('zero', 0)]
            for tag, bits in cases:
                values = np.full(shape, 0x3f80, dtype=np.uint16)
                values[0, :] = bits
                # Accepted maxima must ignore much smaller and negative elements.
                if bits not in (0, 0x8000):
                    values[0, -1] = bits | 0x8000
                run_case(label, cls, tag, values)

    kv_name = fixture.PREFIX + 'model.layers.0.self_attn.kv_b_proj.weight'
    shape = tensors[kv_name][1]['shape']
    cfg = json.loads(original['config.json'])['text_config']
    h, n, v, rank = (cfg[k] for k in ('num_attention_heads', 'qk_nope_head_dim', 'v_head_dim', 'kv_lora_rank'))
    for label in ('all-int8', 'all-int16', 'mixed-156', 'mixed-168'):
        low, high = (0x32fe, 0x4a7e) if POLICIES[label]['attention'] == 'int8' else (0x3700, 0x4e80)
        for tag in ('key-below', 'value-above', 'heterogeneous-accepted', 'all-categories'):
            values = np.full((h, n + v, rank), 0x3f80, dtype=np.uint16)
            # Nonconstant key block: two semantic key rows are zero, despite
            # every source row having nonzero values in the other columns.
            values[0, :n, 0] = 0
            values[1, :n, 2] = 0x8000
            values[2, :n, 3] = low
            values[3, n, :] = 0  # Exactly one semantic value row is zero.
            if tag == 'key-below':
                values[0, :n, 1] = low - 1
            elif tag == 'value-above':
                values[2, n + 1, :] = high
            elif tag == 'all-categories':
                values[0, :n, 1] = low - 1
                values[1, :n, 1] = high
                values[2, 0, 1] = 0x7fc1
                values[0, n, :] = low - 1
                values[1, n, :] = high
                values[2, n, 0] = 0xff80
            report = run_case(label, 'attention', 'kv-' + tag, values.reshape(shape), kv_name)
            if label == 'mixed-168' and tag == 'all-categories':
                (out / 'sample-synthetic-report.json').write_text(json.dumps(report, sort_keys=True, indent=2) + '\n', newline='\n')
                # Make this exact source available for independent reproduction.
                sample = out / 'sample-source'
                sample.mkdir()
                for p in source.iterdir():
                    if p.is_file():
                        (sample / p.name).write_bytes(p.read_bytes())

    restore()
    cli = Path(census.__file__).resolve()
    def cli_control(tag, policy_path=None, output=None):
        before = snapshot(source)
        output = output or out / (tag + '.json')
        command = [sys.executable, str(cli), '--source-dir', str(source), '--source-manifest', str(manifest_path),
                   '--precision', str(policy_path or policy_paths['all-int16']), '--out', str(output)]
        result = subprocess.run(command, capture_output=True, text=True)
        assert result.returncode == 2, (tag, result.stdout, result.stderr)
        assert snapshot(source) == before, tag
        if tag != 'existing-output':
            assert not output.exists(), tag
        (out / (tag + '.stderr.txt')).write_text(result.stderr)
        metadata_controls.append(dict(case=tag, returncode=result.returncode, stderr=result.stderr))

    for tag, invalid in [('null-policy', None), ('missing-class', {'version': 1}),
                         ('unknown-class', dict(POLICIES['all-int16'], experts='int16')),
                         ('bad-version', dict(POLICIES['all-int16'], version=2)),
                         ('bool-version', dict(POLICIES['all-int16'], version=True)),
                         ('bad-selection', dict(POLICIES['all-int16'], head='fp16'))]:
        path = out / (tag + '-policy.json')
        path.write_text(json.dumps(invalid))
        cli_control(tag, path)
    duplicate = out / 'duplicate-policy.json'
    duplicate.write_text('{"version":1,"version":1}')
    cli_control('duplicate-json', duplicate)
    cli_control('source-output', output=source / 'report.json')
    existing = out / 'existing.json'
    existing.write_text('retain me')
    cli_control('existing-output', output=existing)
    assert existing.read_text() == 'retain me'
    for name in ('config.json', manifest['files'][1]['name'], manifest['index']['name']):
        restore()
        path = source / name
        data = path.read_bytes()
        path.write_bytes(data[:-1] + bytes([data[-1] ^ 1]))
        cli_control('bad-hash-' + name)
    # Malformed metadata with a matching hash must still fail structurally.
    for tag in ('wrong-shape', 'wrong-dtype', 'overlapping-payload', 'oversized-header'):
        restore()
        name = manifest['files'][1]['name']
        raw = original[name]
        length = struct.unpack_from('<Q', raw)[0]
        header = json.loads(raw[8:8+length])
        tensor = fixture.PREFIX + TARGETS['attention']
        if tag == 'wrong-shape':
            header[tensor]['shape'].reverse()  # Same bytes, wrong semantic shape.
        elif tag == 'wrong-dtype':
            header[tensor]['dtype'] = 'I32'
            header[tensor]['shape'][1] //= 2
        elif tag == 'overlapping-payload':
            span = header[tensor]['data_offsets']
            header[tensor]['data_offsets'] = [span[0] + 2, span[1] + 2]
        encoded = json.dumps(header).encode()
        changed = struct.pack('<Q', len(encoded)) + encoded + raw[8+length:]
        if tag == 'oversized-header':
            changed = struct.pack('<Q', census.LIMIT + 1) + raw[8:]
        (source / name).write_bytes(changed)
        updated = json.loads(json.dumps(manifest))
        entry = next(e for e in updated['files'] if e['name'] == name)
        entry.update(bytes=len(changed), sha256=digest(source / name))
        manifest_path.write_text(json.dumps(updated))
        cli_control(tag)
    restore()
    # A retained subset is reported as incomplete, without fetching the absent
    # shard. Renaming here is synthetic fixture setup, never census behaviour.
    shard = source / manifest['files'][1]['name']
    saved = source / 'retained.fixture'
    shard.rename(saved)
    before = snapshot(source)
    partial = census.census(source, manifest_path, POLICIES['all-int16'])
    assert not partial['complete_language_tensor_inventory'] and partial['absent_shards'] == [shard.name]
    assert snapshot(source) == before
    saved.rename(shard)
    (out / 'partial-report.json').write_text(json.dumps(partial, indent=2) + '\n')
    before = snapshot(source)
    for repeat in ('a', 'b'):
        subprocess.run([sys.executable, str(cli), '--source-dir', str(source), '--source-manifest', str(manifest_path),
                        '--precision', str(policy_paths['all-int16']), '--out', str(out / f'deterministic-{repeat}.json')], check=True)
    assert (out / 'deterministic-a.json').read_bytes() == (out / 'deterministic-b.json').read_bytes()
    assert snapshot(source) == before == {name: hashlib.sha256(raw).hexdigest() for name, raw in original.items()}
    summary = dict(engine_dependency=census.ENGINE, synthetic_only=True, converter_controls=results,
                   input_controls=metadata_controls, exhaustive_finite_maxima_per_precision=32640,
                   source_unchanged=True, deterministic=True, tile_size_invariance=[7, 32768])
    (out / 'results.json').write_text(json.dumps(summary, sort_keys=True, indent=2) + '\n')
    print(f'{len(results)} synthetic converter/count controls, {len(metadata_controls)} input controls PASS', flush=True)


if __name__ == '__main__':
    main()
