"""How much libm error can the Q16 RoPE table absorb before an entry changes?

`compute_rope_tables` builds the table with host `powf`, `cos` and `sin`,
none of which IEEE 754 requires to be correctly rounded. This recomputes
every entry from a 50-digit evaluation of the same f64 angles and reports:

* whether this host's libm reproduces the correctly-rounded table;
* the smallest distance of any exact value from a rounding boundary; and
* the libm error, in ulps, that the table provably tolerates.

    python3 -m arc_conformance.rope_margin [d_head max_seq base]
"""

from __future__ import annotations

import math
import sys
from decimal import Decimal, getcontext

import blake3

from arc_conformance import integer_reference as ref

getcontext().prec = 60


def machin_pi() -> Decimal:
    def arctan_inv(n: int) -> Decimal:
        x = Decimal(1) / n
        x2 = x * x
        term, total, k = x, x, 1
        while True:
            term *= -x2
            add = term / (2 * k + 1)
            if abs(add) < Decimal(10) ** -(getcontext().prec + 2):
                return total
            total += add
            k += 1
    return 4 * (4 * arctan_inv(5) - arctan_inv(239))


PI = machin_pi()
TWO_PI = 2 * PI


def exact_cos_sin(angle: float) -> tuple[Decimal, Decimal]:
    x = Decimal(angle)  # exact binary value of the f64 angle
    x = x - TWO_PI * (x / TWO_PI).to_integral_value()
    x2 = x * x
    cos_sum, sin_sum = Decimal(1), x
    cos_term, sin_term = Decimal(1), x
    n = 1
    eps = Decimal(10) ** -(getcontext().prec - 5)
    while True:
        cos_term = -cos_term * x2 / ((2 * n - 1) * (2 * n))
        sin_term = -sin_term * x2 / ((2 * n) * (2 * n + 1))
        cos_sum += cos_term
        sin_sum += sin_term
        if abs(cos_term) < eps and abs(sin_term) < eps:
            return cos_sum, sin_sum
        n += 1


def exact_pow(base: float, exponent: float) -> Decimal:
    return (Decimal(base).ln() * Decimal(exponent)).exp()


def round_half_away(value: float) -> int:
    whole = math.floor(abs(value))
    if abs(value) - whole >= 0.5:
        whole += 1
    return int(whole) if value >= 0 else -int(whole)


def round_decimal_half_away(value: Decimal) -> int:
    whole = int(abs(value))
    if abs(value) - whole >= Decimal("0.5"):
        whole += 1
    return whole if value >= 0 else -whole


def boundary_distance(value: Decimal) -> Decimal:
    """Distance from the nearest x.5 boundary, in Q16 units."""
    frac = abs(value) - int(abs(value))
    return abs(frac - Decimal("0.5"))


def ulp(x: float) -> float:
    return math.ulp(x)


def analyse(d_head: int, max_seq: int, base: float) -> dict:
    half = d_head // 2
    host_cos, host_sin = [], []
    exact_cos_tab, exact_sin_tab = [], []
    pow_mismatches = 0
    min_margin = Decimal(1)
    min_margin_at = None
    worst_cos_budget_ulps = math.inf  # tolerable cos/sin error, in ulps of the result
    worst_pow_budget_ulps = math.inf  # tolerable pow error, in ulps of pow
    freqs = []
    for i in range(half):
        exponent = 2.0 * i / d_head
        host_pow = base ** exponent
        cr_pow = float(exact_pow(base, exponent))  # nearest f64 to the exact power
        if host_pow != cr_pow:
            pow_mismatches += 1
        freqs.append((1.0 / host_pow, host_pow))
    for pos in range(max_seq):
        for i in range(half):
            freq, powv = freqs[i]
            angle = pos * freq
            c_exact, s_exact = exact_cos_sin(angle)
            for exact, host_value, host_tab, exact_tab, partner in (
                (c_exact, math.cos(angle), host_cos, exact_cos_tab, s_exact),
                (s_exact, math.sin(angle), host_sin, exact_sin_tab, c_exact),
            ):
                scaled = exact * ref.ONE
                host_tab.append(round_half_away(host_value * ref.ONE))
                exact_tab.append(round_decimal_half_away(scaled))
                margin = boundary_distance(scaled)
                if margin < min_margin:
                    min_margin, min_margin_at = margin, (pos, i)
                # Error budgets, in Q16 units: a cos/sin error of e moves the
                # scaled value by 65536*e; a relative pow error r moves the
                # angle by about angle*r and the value by 65536*|partner|*angle*r.
                result_ulp = ulp(float(abs(exact))) if exact != 0 else 5e-324
                cos_budget = float(margin) / (ref.ONE * result_ulp)
                worst_cos_budget_ulps = min(worst_cos_budget_ulps, cos_budget)
                sensitivity = ref.ONE * abs(float(partner)) * angle
                if sensitivity > 0:
                    pow_rel_budget = float(margin) / sensitivity
                    worst_pow_budget_ulps = min(worst_pow_budget_ulps,
                                                pow_rel_budget * powv / ulp(powv))
    table_bytes = ref.i64_le_bytes(host_cos) + ref.i64_le_bytes(host_sin)
    return {
        "shape": {"d_head": d_head, "max_seq": max_seq, "base": base},
        "entries": 2 * max_seq * half,
        "host_pow_not_correctly_rounded": pow_mismatches,
        "host_table_differs_from_exact_rounding": sum(
            a != b for a, b in zip(host_cos + host_sin, exact_cos_tab + exact_sin_tab)),
        "min_boundary_distance_q16": float(min_margin),
        "min_boundary_distance_at": min_margin_at,
        "tolerated_cos_sin_error_ulps": worst_cos_budget_ulps,
        "tolerated_pow_error_ulps": worst_pow_budget_ulps,
        "table_blake3": blake3.blake3(table_bytes).hexdigest(),
    }


def main(argv: list[str]) -> int:
    d_head, max_seq, base = 128, 4096, 10000.0
    if len(argv) == 4:
        d_head, max_seq, base = int(argv[1]), int(argv[2]), float(argv[3])
    for key, value in analyse(d_head, max_seq, base).items():
        print(f"{key}: {value}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
