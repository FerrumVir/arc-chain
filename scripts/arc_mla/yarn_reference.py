#!/usr/bin/env python3
"""Independent high-precision K2.6 YaRN preparation reference; no weights/network.

Formula source: pinned moonshotai/Kimi-K2.6 modeling_deepseek.py, SHA in output.
Uses Decimal(90), analytic sin/cos, and round-half-away Q62 frequencies / Q16
outputs. ARC canonical vectors differ from the reference's device float32
rounding; float64 formula samples are retained to quantify numerical error.
"""
from decimal import Decimal as D, localcontext, ROUND_HALF_UP, ROUND_FLOOR, ROUND_CEILING
from pathlib import Path
import hashlib, json, math, sys

PI = D('3.1415926535897932384626433832795028841971693993751058209749445923078164062862089986280348253421170679')
Q62 = 1 << 62


def rounded(x):
    return int(x.to_integral_value(rounding=ROUND_HALF_UP))


def sincos(x):
    x %= 2*PI
    if x > PI: x -= 2*PI
    c, s, term = D(1), D(0), D(1)
    for n in range(1, 600):
        term *= x / n
        if n % 2: s += term if (n//2) % 2 == 0 else -term
        else: c += term if (n//2) % 2 == 0 else -term
        if abs(term) < D('1e-85'): break
    return c, s


def vectors():
    with localcontext() as ctx:
        ctx.prec = 90
        raw=(Path(__file__).resolve().parents[2]/'docs/protocol/reference/kimi-k26/config.json').read_bytes()
        assert hashlib.sha256(raw).hexdigest()=='85825ca6e18cbe539eb83ee09eedfb3f4222265929f06e9f535a6d9364f55899'
        text=json.loads(raw)['text_config']; yarn=text['rope_scaling']
        assert yarn=={'beta_fast':32.,'beta_slow':1.,'factor':64.,'mscale':1.,'mscale_all_dim':1.,
                      'original_max_position_embeddings':4096,'type':'yarn'}
        dim=text['qk_rope_head_dim']; base=D(str(text['rope_theta']))
        factor=D(str(yarn['factor'])); original=D(yarn['original_max_position_embeddings'])
        corr=lambda rotations: D(dim)*(original/(D(rotations)*2*PI)).ln()/(2*base.ln())
        low=int(corr(32).to_integral_value(rounding=ROUND_FLOOR))
        high=int(corr(1).to_integral_value(rounding=ROUND_CEILING))
        freqs=[]
        for i in range(dim//2):
            ramp=max(D(0),min(D(1),D(i-low)/D(high-low)))
            extra=(-D(2*i)/dim*base.ln()).exp()
            freqs.append(rounded(extra*(1-ramp+ramp/factor)*Q62))
        scale=1+factor.ln()/10
        lam=int((D(1<<30)*scale*scale/D(192).sqrt()).to_integral_value(rounding=ROUND_FLOOR))
        samples=[]
        for p in [0,1,4095,4096,8191,32768,262143]:
            for i,w in enumerate(freqs):
                c,s=sincos(D(p)*D(w)/Q62)
                # Independent binary64 evaluation of the official formula.
                ramp=max(0.,min(1.,(i-low)/(high-low)))
                w_float=50000.**(-2*i/64)*(1-ramp+ramp/64)
                samples.append({'position':p,'frequency':i,'cos_q16':rounded(c*65536),
                                'sin_q16':rounded(s*65536),'reference_cos_f64':math.cos(p*w_float),
                                'reference_sin_f64':math.sin(p*w_float)})
        return {'schema':'arc.kimi-k26-yarn-reference.v1','revision':'7eb5002f6aadc958aed6a9177b7ed26bb94011bb',
                'config_sha256':'85825ca6e18cbe539eb83ee09eedfb3f4222265929f06e9f535a6d9364f55899',
                'modeling_deepseek_sha256':'1fd8d198ff6ad69a5aec6fd85bf489d91ae2c432560b1e8ba7e34f710463c80a',
                'correction_range':[low,high],'frequencies_q62':freqs,'rotary_magnitude_ratio':1,
                'attention_lambda_q30':lam,'attention_scale_f64':float(scale*scale/D(192).sqrt()),'samples':samples}

if __name__=='__main__':
    out=Path(sys.argv[1]); data=vectors();text=json.dumps(data,indent=2)+'\n'
    if '--check' in sys.argv[2:]:
        pinned=json.loads(out.read_text())
        for a,b in zip(pinned['samples'],data['samples']):
            for k in ['reference_cos_f64','reference_sin_f64']:
                assert abs(a.pop(k)-b.pop(k)) < 1e-12, 'binary64 formula drift'
        assert pinned==data, 'canonical reference vectors drifted' 
        print('pinned Decimal vectors reproduce exactly')
    else: out.write_text(text)
    print('range',data['correction_range'],'lambda',data['attention_lambda_q30'])
