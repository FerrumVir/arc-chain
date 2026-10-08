"""Compare ARC exactness, keeping FP32 tensors/metrics host-specific."""
import argparse
import json
from pathlib import Path
from .compare import canonical, sha
from .policies import NAMES


def compare_hosts(a,b):
    evidence={}
    for depth in (1,2,3):
        for name in NAMES:
            key=f'depth-{depth}/{name}';left=json.loads((a/key/'arc.json').read_bytes());right=json.loads((b/key/'arc.json').read_bytes())
            # ARC raw data includes alignment, model/package/source/policy pins, tensors,
            # native routing, and observer identity; no timing field is ignored here.
            assert left==right,(key,'ARC raw records differ')
            lr=json.loads((a/key/'reference.json').read_bytes());rr=json.loads((b/key/'reference.json').read_bytes())
            assert lr['alignment']==rr['alignment'],(key,'reference input differs')
            assert lr['provenance']['weight_tensors']==rr['provenance']['weight_tensors'],(key,'reference weights differ')
            evidence[key]={'arc_raw_sha256':sha(canonical(left)),'tensor_sha256':left['tensor_sha256'],
                'routing_sha256':sha(canonical(left['routing'])),
                'reference_left_tensor_sha256':lr['tensor_sha256'],'reference_right_tensor_sha256':rr['tensor_sha256'],
                'reference_tensors_equal':lr['tensors']==rr['tensors']}
    return {'arc_exact':True,'runs':evidence,'reference_notice':'Separate host FP32 results; equality is descriptive, never a golden expectation. See each host matrix.md/json for errors and process resources.'}


if __name__=='__main__':
    p=argparse.ArgumentParser(description=__doc__);p.add_argument('left',type=Path);p.add_argument('right',type=Path);p.add_argument('output',type=Path);args=p.parse_args()
    args.output.write_bytes(canonical(compare_hosts(args.left,args.right)))
