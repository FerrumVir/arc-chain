"""Render measured regional budgets; never infer Kimi speed from a toy model."""
import json
import math
import sys


def render(data):
    if data['schema'] != 'arc-regional-budget-v1':
        raise ValueError('unknown budget schema')
    lines = [
        '# Regional swarm latency budget', '', data['label'], '', data['scope'], '',
        'RTT, uplink and payload size below are **emulation assumptions**. All timing columns are **measured**.', '',
        '| RTT ms | Stages | Streams | Compute ms/output token | Network residence ms/output token | Overlap lower bound ms/output token | Residual ms/output token | Wall ms/output token | Per-answer tok/s | Aggregate tok/s | Exact |',
        '|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|:---:|',
    ]
    for row in data['rows']:
        if not row['bit_exact_vs_single_process']:
            raise ValueError('digest mismatch')
        a, m = row['assumed'], row['measured']
        fields = ['compute_ms_per_output_token', 'network_residence_ms_per_output_token',
                  'overlap_lower_bound_ms_per_output_token', 'unattributed_ms_per_output_token',
                  'wall_ms_per_output_token', 'per_answer_decode_tok_s_mean', 'aggregate_tok_s']
        values = [m[k] for k in fields]
        if not all(math.isfinite(v) and v >= 0 for v in values):
            raise ValueError('invalid measurement')
        compute, network, overlap, residual, wall = values[:5]
        if not math.isclose(compute + network - overlap + residual, wall, abs_tol=1e-6):
            raise ValueError('latency budget does not balance')
        if len(m['stage_statistics']) != a['stages']:
            raise ValueError('missing stage measurements')
        lines.append(f"| {a['regional_rtt_ms']:g} | {a['stages']} | {a['concurrency']} | "
                     + ' | '.join(f'{v:.3f}' for v in values) + ' | yes |')
    lines.extend(['', '## Per-stage compute and per-hop network residence', '',
                  'Compute is elapsed forward work per input position. Network residence is measured per data frame on each shaped outgoing hop (including the return). Frames include exact activations and accumulated commitments. The first prefill frame holds two positions; these are not RTT estimates.', ''])
    for row in data['rows']:
        a, m = row['assumed'], row['measured']
        lines.extend([f"### {a['regional_rtt_ms']:g} ms RTT, {a['concurrency']} streams", '',
                      '| Stage layers | Compute ms/position | Hop residence ms/frame | Data frames | Wire bytes |',
                      '|---|---:|---:|---:|---:|'])
        for stage in m['stage_statistics']:
            hop = stage['emulated_data_hop']
            if stage['positions'] <= 0 or hop['frames'] <= 0:
                raise ValueError('missing compute or hop telemetry')
            lines.append(f"| {stage['stage']} | {1000 * stage['compute_seconds'] / stage['positions']:.3f} | "
                         f"{1000 * hop['residence_seconds'] / hop['frames']:.3f} | {hop['frames']} | {hop['bytes']} |")
    return '\n'.join(lines) + '\n'


if __name__ == '__main__':
    with open(sys.argv[1]) as source:
        result = render(json.load(source))
    with open(sys.argv[2], 'w') as out:
        out.write(result)
