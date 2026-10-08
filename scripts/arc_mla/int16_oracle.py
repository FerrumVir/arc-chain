#!/usr/bin/env python3
"""Independent exact-rational INT16 oracle. No Rust import or floating arithmetic.
Regenerate/check corpus consumed by Rust tests on every OS; not a quality oracle.
"""
import argparse
from fractions import Fraction as F
import json
from pathlib import Path
import random


def pow2(e):
    return F(2)**e


def bf16(b):
    exponent, mantissa = (b >> 7) & 255, b & 127
    if exponent == 255:
        raise ValueError('nonfinite')
    v = F(mantissa if exponent == 0 else 128 + mantissa) * pow2(-133 if exponent == 0 else exponent - 134)
    return -v if b & 32768 else v


def rha(x):
    a = abs(x) + F(1, 2)
    return (-1 if x < 0 else 1) * (a.numerator // a.denominator)


def quantize(bits):
    if not bits:
        raise ValueError('shape')
    values = list(map(bf16, bits))
    maximum = max(map(abs, values))
    if not maximum:
        return dict(q=[0]*len(bits), mu=0, k=16)
    if not pow2(-17) <= maximum < pow2(30):
        raise ValueError('window')
    scale = maximum / 32767
    # Search scale interval, independent of Rust's BF16 significand/shift code.
    k = next(k for k in range(256) if scale*pow2(k) >= pow2(30))
    mu = rha(scale*pow2(k))
    if mu == 2**31:
        mu, k = 2**30, k-1
    assert 16 <= k <= 62
    return dict(q=[rha(v*32767/maximum) for v in values], mu=mu, k=k)


def norm(b):
    x = bf16(b)
    if abs(x) >= pow2(46):
        raise ValueError('norm domain')
    return rha(x*65536)


def router(bits):
    values = list(map(bf16, bits))
    maximum = max(map(abs, values))
    if not maximum:
        return dict(q=[0]*len(bits), k=16)
    # Router row maximum in [2^14, 2^15) after scaling.
    k = next((k for k in range(63) if pow2(14) <= maximum*pow2(k) < pow2(15)), None)
    if k is None:
        raise ValueError('router domain')
    return dict(q=[rha(v*pow2(k)) for v in values], k=k)


def project(q, mu, k, x):
    if sum(map(abs,x)) >= 2**63//32767:
        raise ValueError('accumulator')
    if any(abs(w)>32767 for w in q) or not ((mu==0 and k==16) or (2**30<=mu<2**31 and 16<=k<=62)):
        raise ValueError('scale')
    y = sum(w*a for w,a in zip(q,x))*mu//2**k
    if abs(y)>2**62:
        raise ValueError('output')
    return y


def result(fn, *args, **kwargs):
    try:
        return dict(ok=fn(*args, **kwargs))
    except ValueError:
        return dict(error=True)


def corpus():
    rng = random.Random(660817)
    maxima = [0,1,0x36ff,0x3700,0x3701,0x4e7f,0x4e80,0x4e81,0x7f7f,0x7f80,0x7fc1]
    # Every exponent/sign incl subnormals; representative significand boundaries.
    maxima += [(e<<7)|m for e in range(255) for m in (0,1,127)]
    rows = [[b,b|0x8000,0,0x8000] for b in maxima]
    rows += [[0x3f80,0xbf80,0x3f00,0xbf00,1,0x8001]]
    rows += [[rng.randrange(0x4e80)|(rng.randrange(2)<<15) for _ in range(33)] for _ in range(40)]
    dots=[]
    for row in rows[-41:]:
        q=quantize(row)
        for x in [[(i-16)*456789 for i in range(len(row))], [2**36]*len(row)]:
            item=dict(q=q['q'],mu=q['mu'],k=q['k'],x=x)
            dots.append(dict(item,**result(project,**item)))
    for q,mu,k,x in [([32767],2**30,45,[2**63//32767-1]),([32767],2**30,45,[2**63//32767]),
                     ([32767],2**31-1,16,[2**40]),([-1],2**30,62,[1]),
                     ([-32768],2**30,45,[1]),([1],2**30,63,[1]),([0],0,16,[1])]:
        item=dict(q=q,mu=mu,k=k,x=x)
        dots.append(dict(item,**result(project,**item)))
    scalars=[b for b in maxima]+[0x567f,0x5680,0x56ff]
    return dict(schema='arc.int16-rational-oracle.v1',
                rows=[dict(bits=r,**result(quantize,r)) for r in rows],
                norms=[dict(bits=b,**result(norm,b)) for b in scalars for b in (b,b|0x8000)],
                routers=[dict(bits=r,**result(router,r)) for r in rows], projections=dots)


def main():
    ap=argparse.ArgumentParser(description=__doc__)
    ap.add_argument('out',type=Path)
    ap.add_argument('--check',action='store_true')
    a=ap.parse_args()
    data=corpus()
    encoded=json.dumps(data,sort_keys=True,separators=(',',':'))+'\n'
    if a.check:
        assert a.out.read_text()==encoded, 'oracle corpus drift'
    else:
        a.out.write_text(encoded)
    print(json.dumps({k:len(v) for k,v in data.items() if isinstance(v,list)},sort_keys=True))


if __name__=='__main__':main()
