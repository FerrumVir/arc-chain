"""Regressions run against records produced by two actual engines, never mocks."""
import copy
import json
import os
from pathlib import Path
import subprocess
import shutil
import tempfile
import unittest
from .compare import canonical, compare
from .official import execute


class ActualFixture(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.root = Path(os.environ['ARC_LAYER_FIXTURE']).resolve()
        cls.arc = json.loads((cls.root / 'arc.json').read_bytes())
        cls.ref = json.loads((cls.root / 'reference.json').read_bytes())
        cls.request_bytes = (cls.root / 'request.json').read_bytes()
        cls.request = json.loads(cls.request_bytes)

    def check(self, arc=None, ref=None):
        return compare(self.arc if arc is None else arc, self.ref if ref is None else ref,
                       self.request, self.request_bytes)

    def test_actual_aligned_values_noncertifying(self):
        result = self.check()
        self.assertFalse(result['certified'])
        self.assertEqual(result['status'], 'MEASURED_NON_CERTIFYING')
        self.assertEqual(result['metrics']['layer.0.output']['count'], 256)
        self.assertEqual(result['metrics']['logits']['count'], 1200)
        self.assertGreater(result['metrics']['logits']['max_absolute_error'], 0)

    def test_mismatched_alignment(self):
        changes = {'model_root': '0'*64, 'token_ids': [1,42,7,4],
                   'positions': [1,2,3,4], 'scope': {'kind':'full_kimi_k26'},
                   'mask': 'bidirectional', 'graph': {'executed_layers':[1]}}
        for engine in ('arc', 'ref'):
            for key, value in changes.items():
                with self.subTest(engine=engine, field=key):
                    doc = copy.deepcopy(getattr(self, engine))
                    doc['alignment'][key] = value
                    with self.assertRaises(ValueError): self.check(**{engine:doc})

    def test_missing_and_reordered_data(self):
        for mutate in (lambda d: d['tensors'].pop('logits'),
                       lambda d: d['tensors']['logits'].pop(),
                       lambda d: d['tensors']['logits'][0].pop(),
                       lambda d: d['alignment']['positions'].reverse()):
            doc=copy.deepcopy(self.ref);mutate(doc)
            with self.assertRaises(ValueError): self.check(ref=doc)

    def test_nonfinite_and_invalid_integer(self):
        for engine, values in [('arc', [float('nan'),float('inf'),0.5,True,2**64]),
                               ('ref', [float('nan'),float('inf'),-float('inf'),True])]:
            for value in values:
                with self.subTest(engine=engine,value=value):
                    doc=copy.deepcopy(getattr(self,engine));doc['tensors']['logits'][0][0]=value
                    with self.assertRaises(ValueError):self.check(**{engine:doc})

    def test_scale_and_provenance_rejection(self):
        for engine in ('arc','ref'):
            for key in ('request_sha256','package_sha256','source_manifest_sha256'):
                doc=copy.deepcopy(getattr(self,engine));doc['provenance'][key]='0'*64
                with self.assertRaises(ValueError): self.check(**{engine:doc})
            doc=copy.deepcopy(getattr(self,engine));doc['numeric']['unit_scale']=1e-9
            with self.assertRaises(ValueError):self.check(**{engine:doc})

    def test_request_bytes_and_invalid_positions(self):
        with self.assertRaises(ValueError):compare(self.arc,self.ref,self.request,b'{}')
        for field,value in [('positions',[0,2,3,4]),('token_ids',[True]),('scope',{'kind':'synthetic_fixture','layers':1})]:
            request=copy.deepcopy(self.request);request[field]=value
            a=copy.deepcopy(self.arc);b=copy.deepcopy(self.ref)
            a['alignment']=request;b['alignment']=request
            with self.assertRaises(ValueError):compare(a,b,request,canonical(request))

    def test_actual_cli_rejections_no_output(self):
        binary=os.environ['ARC_LAYER_DIAGNOSTIC']
        for field,value in [('model_root','0'*64),('scope',{'kind':'full_kimi_k26'}),
                            ('positions',[1,2,3,4]),('token_ids',[999999]),
                            ('shape',{'hidden_size':1,'vocab_size':300}),
                            ('source_manifest_sha256','0'*64)]:
            with self.subTest(field=field), tempfile.TemporaryDirectory(dir=self.root) as tmp:
                request=copy.deepcopy(self.request);request[field]=value
                request_path=Path(tmp)/'request.json';request_path.write_bytes(canonical(request))
                output=Path(tmp)/'out.json'
                result=subprocess.run([binary,str(self.root/'bundle/stage-0.arcspkg'),
                    str(self.root/'bundle/manifest.json'),str(self.root/'source/tiny-kimi-packed.source.json'),
                    str(request_path),str(output)],capture_output=True,text=True)
                self.assertNotEqual(result.returncode,0)
                self.assertIn('layer diagnostic:',result.stderr)
                self.assertFalse(output.exists())

    def test_reference_missing_corrupt_source_and_config(self):
        for fault in ('missing_weight', 'altered_weight', 'altered_config', 'altered_index'):
            with self.subTest(fault=fault), tempfile.TemporaryDirectory(dir=self.root) as tmp:
                source=Path(tmp)/'source';shutil.copytree(self.root/'source',source)
                weight=source/'model-00001-of-00006.safetensors'
                if fault=='missing_weight': weight.unlink()
                elif fault=='altered_weight':
                    raw=bytearray(weight.read_bytes());raw[-1]^=1;weight.write_bytes(raw)
                else:
                    path=source/('config.json' if fault=='altered_config' else 'model.safetensors.index.json')
                    path.write_bytes(path.read_bytes()+b' ')
                with self.assertRaises((ValueError,FileNotFoundError)):
                    execute(source,source/'tiny-kimi-packed.source.json',self.root/'original',
                            self.request,self.request_bytes)

    def test_actual_cli_truncated_package_no_output(self):
        with tempfile.TemporaryDirectory(dir=self.root) as tmp:
            package=Path(tmp)/'truncated.arcspkg'
            package.write_bytes((self.root/'bundle/stage-0.arcspkg').read_bytes()[:-1])
            output=Path(tmp)/'out.json'
            result=subprocess.run([os.environ['ARC_LAYER_DIAGNOSTIC'],str(package),
                str(self.root/'bundle/manifest.json'),str(self.root/'source/tiny-kimi-packed.source.json'),
                str(self.root/'request.json'),str(output)],capture_output=True,text=True)
            self.assertNotEqual(result.returncode,0)
            self.assertFalse(output.exists())

    def test_reference_reexecutes_and_preserves_original_weights(self):
        doc=execute(self.root/'source',self.root/'source/tiny-kimi-packed.source.json',
                    self.root/'original',self.request,self.request_bytes)
        self.assertEqual(doc['tensors'],self.ref['tensors'])
        self.assertIn('language_model.lm_head.weight',doc['provenance']['weight_tensors'])
        self.assertEqual(doc['provenance']['execution'], self.ref['provenance']['execution'])


if __name__ == '__main__': unittest.main()
