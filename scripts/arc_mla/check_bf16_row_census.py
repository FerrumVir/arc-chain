#!/usr/bin/env python3
"""Synthetic-only census counts checked against actual converter outcomes."""
import argparse
from fractions import Fraction
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


def check_distributions(report, source):
    """Independent fixture-only bulk oracle, unlike the streaming scanner."""
    cfg = json.loads((source / 'config.json').read_text())['text_config']
    heads, nope, value, rank = (cfg[k] for k in ('num_attention_heads', 'qk_nope_head_dim', 'v_head_dim', 'kv_lora_rank'))
    tensors = {}
    for shard in source.glob('*.safetensors'):
        raw = shard.read_bytes()
        length = struct.unpack_from('<Q', raw)[0]
        for name, info in json.loads(raw[8:8+length]).items():
            if name != '__metadata__' and info['dtype'] == 'BF16':
                begin, end = info['data_offsets']
                tensors[name] = np.frombuffer(raw[8+length+begin:8+length+end], dtype='<u2').reshape(info['shape'])
    maxima_by_class = {c: [] for c in report['classes']}
    maxima_by_tensor = {}
    def stats(maxima):
        finite = sorted(b for b in maxima if b < 0x7f80)
        if not finite:
            return dict(finite_rows=0, nonfinite_rows=len(maxima), min=None, median=None, max=None,
                        median_exact=None, median_bf16_pair=None)
        # IEEE decoding and full sorting provide an independent oracle to the
        # scanner's integer BF16 decoding and histogram order statistics.
        val = lambda b: Fraction.from_float(struct.unpack('<f', struct.pack('<I', b << 16))[0])
        lo, hi = finite[(len(finite)-1)//2], finite[len(finite)//2]
        med = (val(lo) + val(hi))/2
        return dict(finite_rows=len(finite), nonfinite_rows=len(maxima)-len(finite),
                    min=float(val(finite[0])), median=float(med), max=float(val(finite[-1])),
                    median_exact=dict(numerator=str(med.numerator), denominator=str(med.denominator)),
                    median_bf16_pair=[f'0x{lo:04x}', f'0x{hi:04x}'])
    def classified(b, precision):
        lower, upper = {'int8':(0x32fe,0x4a7e), 'int16':(0x3700,0x4e80),
                        'int16_power_of_two':(0x2780,0x4700), 'i64_q16':(1,0x5680)}[precision]
        return ('nonfinite' if b>=0x7f80 else 'zero' if b==0 else 'below_range' if b<lower else
                'at_or_above_range' if b>=upper else 'in_range')
    def window(maxima):
        return {key:sum(classified(b,'int16')==key for b in maxima) for key in census.CATEGORIES}
    def intersection(maxima):
        return sum(0x32fe <= b < 0x3700 for b in maxima)
    for r in report['tensors']:
        raw = tensors[r['tensor']]
        if r['layout'].startswith('key_'):
            rows = raw.reshape(heads,nope+value,rank)[:,:nope,:].transpose(0,2,1).reshape(heads*rank,nope)
            coordinates = [[h,c] for h in range(heads) for c in range(rank)]
        elif r['layout'].startswith('value_'):
            rows = raw.reshape(heads,nope+value,rank)[:,nope:,:].reshape(heads*value,rank)
            coordinates = [[h,v] for h in range(heads) for v in range(value)]
        else:
            rows = raw.reshape(r['semantic_shape'])
            coordinates = [[i] for i in range(len(rows))]
        maxima = [max(int(b)&0x7fff for b in row) for row in rows]
        maxima_by_class[r['tensor_class']].extend(maxima)
        maxima_by_tensor.setdefault(r['tensor'],[]).extend(maxima)
        assert r['row_max_abs'] == stats(maxima), r
        assert r['int16_window_counts'] == window(maxima), r
        assert r['int8_accepted_int16_rejected'] == intersection(maxima), r
        assert r['counts'] == {key:sum(classified(b,r['precision'])==key for b in maxima) for key in census.CATEGORIES}
        rejected = []
        for i,b in enumerate(maxima):
            actual, wide = classified(b,r['precision']), classified(b,'int16')
            bad = lambda c: c not in ('zero','in_range')
            if bad(actual) or bad(wide):
                rejected.append(dict(semantic_row=i,coordinates=coordinates[i],maximum_abs_bf16=f'0x{b:04x}',
                                     actual_admission=actual if bad(actual) else None,
                                     int16_matrix_window=wide if bad(wide) else None,
                                     int8_accepted_int16_rejected=bool(0x32fe<=b<0x3700)))
        assert r['rejected_rows']==rejected, (r,rejected)
    assert len(report['tensor_totals'])==len(maxima_by_tensor)
    for a in report['tensor_totals']:
        maxima=maxima_by_tensor[a['tensor']]
        assert a['rows']==len(maxima)
        assert a['row_max_abs']==stats(maxima)
        assert a['int16_window_counts']==window(maxima)
        assert a['int8_accepted_int16_rejected']==intersection(maxima)
        assert a['counts']=={k:sum(r['counts'][k] for r in report['tensors'] if r['tensor']==a['tensor']) for k in census.CATEGORIES}
    for cls,maxima in maxima_by_class.items():
        a=report['classes'][cls]
        assert a['rows']==len(maxima)
        assert a['row_max_abs']==stats(maxima), (cls,a)
        assert a['int16_window_counts']==window(maxima)
        assert a['int8_accepted_int16_rejected']==intersection(maxima)
        assert a['counts']=={k:sum(r['counts'][k] for r in report['tensors'] if r['tensor_class']==cls) for k in census.CATEGORIES}
    all_maxima = [b for maxima in maxima_by_class.values() for b in maxima]
    assert report['counts']=={k:sum(r['counts'][k] for r in report['tensors']) for k in census.CATEGORIES}
    assert report['row_max_abs']==stats(all_maxima)
    assert report['int16_window_counts']==window(all_maxima)
    assert report['int8_accepted_int16_rejected']==intersection(all_maxima)
    return {r['tensor']+':'+r['layout']: {k:r[k] for k in
            ('row_max_abs','int16_window_counts','int8_accepted_int16_rejected')} for r in report['tensors']}

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

    corpus=json.loads((Path(__file__).resolve().parents[2]/'docs/protocol/reference/int16-row-oracle.json').read_text())
    for section,precision in [('rows','int16'),('routers','int16_power_of_two'),('norms','i64_q16')]:
        for record in corpus[section]:
            bits=record['bits'] if isinstance(record['bits'],list) else [record['bits']]
            maximum=max(b&0x7fff for b in bits)
            assert (census.actual_category(maximum,precision) in ('zero','in_range'))==('ok' in record)
    for values in [[],[0x7fc1],[0,0x8000],[1,0x4e7f],[0x3700],[0x36ff,0x3700,0x3701]]:
        d=census.Distribution('int8')
        for i,b in enumerate(values):d.add(b&0x7fff,i,[i])
        assert sum(d.window.values())==len(values)
        assert d.fields()['row_max_abs']['finite_rows']==sum((b&0x7fff)<0x7f80 for b in values)
    even=census.Distribution('int8')
    for i,b in enumerate([1,0x4e7f]):even.add(b,i,[i])
    exact=even.fields()['row_max_abs']['median_exact']
    expected=(Fraction(1,2**133)+Fraction.from_float(struct.unpack('<f',struct.pack('<I',0x4e7f<<16))[0]))/2
    assert Fraction(int(exact['numerator']),int(exact['denominator']))==expected

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
            'native routed expert (including packed scales and shape)', 'native correction bias',
            'outside language conversion (ignored)'}
        check_distributions(report, source)
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

    # Native admission must stay distinct from diagnostic matrix windows.
    # Every case is checked against the real converter; each native tensor has
    # nonconstant values, including exact native and matrix threshold neighbors.
    for cls, short, values in [
        ('norm', 'model.norm.weight', [0,1,0x32fd,0x32fe,0x36ff,0x3700,0x3701,0x4e7f,0x4e80,0x567f]),
        ('router', 'model.layers.1.mlp.gate.weight', [0,0x2780,0x32fd,0x32fe,0x36ff,0x3700,0x46ff]),
    ]:
        tensor=fixture.PREFIX+short
        shape=tensors[tensor][1]['shape']
        for tag,bad in [('valid',None),('below',0x277f if cls=='router' else None),
                        ('above',0x4700 if cls=='router' else 0x5680),('nan',0x7fc1),('inf',0xff80)]:
            restore()
            data=np.full(shape,0x3f80,dtype=np.uint16)
            if cls=='norm':
                data[:len(values)]=values
                if bad is not None:data[-1]=bad
            else:
                for i,b in enumerate(values):data[i,:]=b
                if bad is not None:data[-1,:]=bad
            change(tensor,data)
            case=out/f'native-{cls}-{tag}';case.mkdir()
            before=snapshot(source)
            report=census.census(source,manifest_path,POLICIES['all-int8'],chunk_elements=7)
            fixed=check_distributions(report,source)
            assert fixed==check_distributions(census.census(source,manifest_path,POLICIES['all-int16']),source)
            record=next(r for r in report['tensors'] if r['tensor']==tensor)
            assert record['layout']==('norm_scalar_q16' if cls=='norm' else 'router_expert_row')
            accepted=not any(record['counts'][k] for k in ('below_range','at_or_above_range','nonfinite'))
            assert report['scanned_native_rows_supported']==accepted
            assert report['scanned_selectable_rows_supported']
            command=[str(binary),'convert','--source-dir',str(source),'--source-manifest',str(manifest_path),
                     '--experts','i4g32','--precision',str(policy_paths['all-int8']),'--out',str(case/'output.arcspkg'),'--threads','1']
            run=subprocess.run(command,capture_output=True,text=True)
            (case/'stdout.txt').write_text(run.stdout);(case/'stderr.txt').write_text(run.stderr)
            assert (run.returncode==0)==accepted,(cls,tag,run.stderr)
            assert snapshot(source)==before
            (case/'report.json').write_text(json.dumps(report,sort_keys=True,indent=2)+'\n')
            results.append(dict(tensor=tensor,case=f'native-{cls}-{tag}',accepted=accepted,returncode=run.returncode))
            if cls=='norm' and tag=='valid':
                (out/'sample-native-report.json').write_text(json.dumps(report,sort_keys=True,indent=2)+'\n')

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
