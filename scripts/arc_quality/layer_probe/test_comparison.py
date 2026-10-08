"""Regressions run against records produced by two actual engines, never mocks."""
import copy
import json
import os
from pathlib import Path
import subprocess
import shutil
import tempfile
import unittest
from .compare import canonical, compare, sha
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

    def test_valid_but_substituted_policy_cli_rejected(self):
        from .policies import policy
        for value in (None, policy('int16'), policy('attention')):
            if value==self.request['precision']:continue
            with tempfile.TemporaryDirectory(dir=self.root) as tmp:
                req=copy.deepcopy(self.request);req['precision']=value
                path=Path(tmp)/'request.json';path.write_bytes(canonical(req));output=Path(tmp)/'out.json'
                result=subprocess.run([os.environ['ARC_LAYER_DIAGNOSTIC'],str(self.root/'bundle/stage-0.arcspkg'),
                    str(self.root/'bundle/manifest.json'),str(self.root/'source/tiny-kimi-packed.source.json'),str(path),str(output)],capture_output=True,text=True)
                self.assertNotEqual(result.returncode,0)
                self.assertIn('layer diagnostic:',result.stderr)
                self.assertFalse(output.exists())

    def test_precision_schema_rejection(self):
        from .compare import validate_request
        for value in ({}, {'version':1}, {'version':True, **{k:'int8' for k in ('attention','dense','shared','embedding','head')}},
                      {'version':1, **{k:'int8' for k in ('attention','dense','shared','embedding','head')}, 'experts':'int16'}):
            request=copy.deepcopy(self.request);request['precision']=value
            with self.assertRaises(ValueError):validate_request(request,canonical(request))

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
                   'mask': 'bidirectional', 'graph': {'executed_layers':[1]}, 'precision':{'version':999}}
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
                            ('source_manifest_sha256','0'*64), ('precision',{'version':999})]:
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

    def test_missing_reordered_layer_captures(self):
        for key in ('arc','ref'):
            for fault in ('missing','layer_order','swapped_rows','swapped_layers'):
                doc=copy.deepcopy(getattr(self,key))
                if fault=='missing':doc['tensors'].pop('layer.0.output')
                elif fault=='layer_order':doc['layer_order']=[99]+doc['layer_order'][1:]
                elif fault=='swapped_rows':doc['tensors']['layer.0.output'].reverse()
                elif len(doc['layer_order'])>1:
                    doc['tensors']['layer.0.output'],doc['tensors']['layer.1.output']=doc['tensors']['layer.1.output'],doc['tensors']['layer.0.output']
                else:continue
                with self.subTest(engine=key,fault=fault),self.assertRaises(ValueError):self.check(**{key:doc})

    def test_multiple_routes_and_shared_experts(self):
        for doc in (self.arc,self.ref):
            for layer,rows in doc['routing'].items():
                self.assertEqual(len(rows),len(self.request['token_ids']))
                for row in rows:
                    self.assertEqual(len(set(row['experts'])),3)
                    self.assertTrue(all(w>0 for w in row['weights']))
                    self.assertTrue(any(x!=0 for x in row['shared']))
        self.assertTrue(self.arc['observer']['verified_against_unmodified_engine'])
        if len(self.arc['layer_order'])==1 and self.request['precision'] is None:
            self.assertEqual(sha(canonical(self.arc['tensors'])),'fd1309caa28fdc3e58f7b12f3f9ccff5d897100d1e9252061e3e3e97573f3596')
        elif len(self.arc['layer_order'])>1:
            self.assertIn('layer.1',self.check()['routing_comparison'])
            self.assertTrue(any(v['source_dtype']=='torch.int32' for v in self.ref['provenance']['weight_tensors'].values()))

    def test_routing_missing_reordered_nonfinite(self):
        if not self.arc['routing']:return
        for key in ('arc','ref'):
            for fault in ('missing','order','duplicate_expert','nonfinite'):
                doc=copy.deepcopy(getattr(self,key));rows=doc['routing']['layer.1']
                if fault=='missing':rows.pop()
                elif fault=='order':rows.reverse()
                elif fault=='duplicate_expert':rows[0]['experts'][0]=rows[0]['experts'][1]
                else:rows[0]['weights'][0]=float('nan')
                with self.subTest(engine=key,fault=fault),self.assertRaises(ValueError):self.check(**{key:doc})

    def test_graph_depth_mismatch(self):
        for layers in ([1], [0,2], [0,1,2,3]):
            doc=copy.deepcopy(self.ref);doc['alignment']['graph']['executed_layers']=layers
            with self.assertRaises(ValueError):self.check(ref=doc)

    def test_official_moe_adds_shared_experts_to_multiple_routes(self):
        import torch
        import types
        from .official import definitions
        config=json.loads((self.root/'source/config.json').read_bytes())['text_config']
        for shared_count in (1,2):
            config['n_shared_experts']=shared_count
            moe=definitions()['DeepseekV3MoE'](types.SimpleNamespace(**config)).eval()
            with torch.no_grad():
                for parameter in moe.parameters():parameter.fill_(0.1)
                x=torch.ones((1,2,config['hidden_size']))
                ids,weights=moe.gate(x)
                self.assertEqual(ids.shape,(2,3))
                routed=moe.moe_infer(x.view(-1,x.shape[-1]),ids,weights).view_as(x)
                shared=moe.shared_experts(x)
                self.assertTrue(torch.equal(moe(x),routed+shared))
                self.assertFalse(torch.equal(moe(x),routed))
                self.assertEqual(moe.shared_experts.gate_proj.out_features,shared_count*config['moe_intermediate_size'])

    def test_packed_int4_offset_nibbles_and_zero_scale(self):
        import torch
        from .official import unpack_int4
        # I32 packs low-to-high offset nibbles: 0 means -8, 8 means 0.
        words=torch.tensor([[0x76543210,-19088744,0x76543210,-19088744]],dtype=torch.int32)
        shape=torch.tensor([1,32],dtype=torch.int32)
        actual=unpack_int4(words,torch.tensor([[0.5]],dtype=torch.bfloat16),shape)
        self.assertEqual(actual.tolist(),[[x*0.5 for x in list(range(-8,8))*2]])
        self.assertTrue((unpack_int4(words,torch.zeros((1,1),dtype=torch.bfloat16),shape)==0).all())
        with self.assertRaises(ValueError):unpack_int4(words,torch.tensor([[float('nan')]],dtype=torch.bfloat16),shape)

    def test_reference_reexecutes_and_preserves_original_weights(self):
        doc=execute(self.root/'source',self.root/'source/tiny-kimi-packed.source.json',
                    self.root/'original',self.request,self.request_bytes)
        self.assertEqual(doc['tensors'],self.ref['tensors'])
        self.assertIn('language_model.lm_head.weight',doc['provenance']['weight_tensors'])
        self.assertEqual(doc['provenance']['execution'], self.ref['provenance']['execution'])


if __name__ == '__main__': unittest.main()
