"""Compare an already verified one-layer bundle with retained BF16 source shards.

No downloads. The reference remains the declared dense layer-0 experiment.
"""
import argparse
import json
import subprocess
from pathlib import Path
from .compare import canonical, compare, sha, sha_file
from .official import execute, REFERENCE_SHA
from .run import ENGINE_SHA


def main():
    p = argparse.ArgumentParser(description=__doc__)
    for name in ('bundle', 'source-dir', 'source-manifest', 'original-source-dir',
                 'original-source-manifest', 'tokens', 'diagnostic', 'out'):
        p.add_argument('--' + name, required=True)
    args = p.parse_args()
    bundle = Path(args.bundle)
    source = Path(args.source_dir)
    original = Path(args.original_source_dir)
    pin = Path(args.source_manifest)
    original_pin = Path(args.original_source_manifest)
    cfg = json.loads((source / 'config.json').read_bytes())['text_config']
    original_cfg = json.loads((original / 'config.json').read_bytes())['text_config']
    manifest = json.loads((bundle / 'manifest.json').read_bytes())
    package = bundle / 'stage-0.arcspkg'
    tokens = json.loads(Path(args.tokens).read_bytes())
    request = dict(engine_sha=ENGINE_SHA, model_root=manifest['model_root'],
                   scope=manifest['model']['preparation']['scope'],
                   graph=dict(embedding='original', executed_layers=[0], head='original',
                              source_layer_count=original_cfg['num_hidden_layers']),
                   shape=dict(hidden_size=cfg['hidden_size'], vocab_size=cfg['vocab_size']),
                   token_ids=tokens, positions=list(range(len(tokens))), mask='causal_no_padding',
                   source_manifest_sha256=sha(pin.read_bytes()),
                   original_source_manifest_sha256=sha(original_pin.read_bytes()),
                   config_sha256=sha((source / 'config.json').read_bytes()),
                   package_sha256=sha_file(package), reference_sha256=REFERENCE_SHA)
    data = canonical(request)
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=False)
    (out / 'request.json').write_bytes(data)
    subprocess.run([args.diagnostic, str(package), str(bundle / 'manifest.json'), str(pin),
                    str(out / 'request.json'), str(out / 'arc.json')], check=True)
    reference = execute(source, pin, original, request, data, original_pin)
    (out / 'reference.json').write_bytes(canonical(reference))
    result = compare(json.loads((out / 'arc.json').read_bytes()), reference, request, data)
    (out / 'comparison.json').write_bytes(canonical(result))
    print(json.dumps(result, indent=2))


if __name__ == '__main__':
    main()
