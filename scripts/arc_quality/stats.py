"""Paired statistics for "is engine B as accurate as engine A on the same items".

Standard library only. Both engines answer the same items, so the comparison
uses the 2x2 table of per-item correctness:

                  B correct   B wrong
      A correct       a          b
      A wrong         c          d

  accuracy A = (a+b)/n, accuracy B = (a+c)/n, delta = B - A = (c-b)/n.

* Confidence interval of delta: Newcombe's hybrid score interval for paired
  proportions (Newcombe 1998, Statistics in Medicine 17:2635, method 10),
  built from Wilson intervals; it stays sensible at 0 discordant pairs.
* Exact McNemar test: two-sided binomial test of b vs c.
"""

from __future__ import annotations

import math
from statistics import NormalDist


def z_value(confidence: float) -> float:
    return NormalDist().inv_cdf(0.5 + confidence / 2.0)


def wilson(successes: int, n: int, z: float) -> tuple[float, float]:
    if n == 0:
        return (0.0, 1.0)
    p = successes / n
    denominator = 1.0 + z * z / n
    centre = (p + z * z / (2 * n)) / denominator
    half = z * math.sqrt(p * (1 - p) / n + z * z / (4 * n * n)) / denominator
    return (max(0.0, centre - half), min(1.0, centre + half))


def paired_table(pairs: list[tuple[bool, bool]]) -> dict:
    a = sum(1 for x, y in pairs if x and y)
    b = sum(1 for x, y in pairs if x and not y)
    c = sum(1 for x, y in pairs if not x and y)
    d = sum(1 for x, y in pairs if not x and not y)
    return {"n": len(pairs), "both": a, "only_a": b, "only_b": c, "neither": d}


def newcombe_paired(table: dict, confidence: float = 0.95) -> tuple[float, float]:
    """CI of accuracy(B) - accuracy(A) (method 10, no continuity correction)."""
    n = table["n"]
    if n == 0:
        return (-1.0, 1.0)
    a, b, c, d = table["both"], table["only_a"], table["only_b"], table["neither"]
    z = z_value(confidence)
    p1, p2 = (a + b) / n, (a + c) / n
    l1, u1 = wilson(a + b, n, z)
    l2, u2 = wilson(a + c, n, z)
    product = (a + b) * (c + d) * (a + c) * (b + d)
    phi = 0.0 if product == 0 else (a * d - b * c) / math.sqrt(product)
    delta = p2 - p1
    lower = delta - math.sqrt(max(0.0, (p2 - l2) ** 2 - 2 * phi * (p2 - l2) * (u1 - p1) + (u1 - p1) ** 2))
    upper = delta + math.sqrt(max(0.0, (u2 - p2) ** 2 - 2 * phi * (u2 - p2) * (p1 - l1) + (p1 - l1) ** 2))
    return (max(-1.0, lower), min(1.0, upper))


def mcnemar_exact(b: int, c: int) -> float:
    """Two-sided exact p-value that the discordant pairs split 50/50."""
    m = b + c
    if m == 0:
        return 1.0
    k = min(b, c)
    tail = sum(math.comb(m, i) for i in range(k + 1)) / 2.0**m
    return min(1.0, 2.0 * tail)


def summarize(pairs: list[tuple[bool, bool]], confidence: float = 0.95) -> dict:
    """Paired summary with A = reference, B = ARC. Rates are fractions."""
    table = paired_table(pairs)
    n = table["n"]
    lower, upper = newcombe_paired(table, confidence)
    return {
        **table,
        "reference_accuracy": (table["both"] + table["only_a"]) / n if n else None,
        "arc_accuracy": (table["both"] + table["only_b"]) / n if n else None,
        "delta": (table["only_b"] - table["only_a"]) / n if n else None,
        "delta_ci": [lower, upper],
        "confidence": confidence,
        "discordant_rate": (table["only_a"] + table["only_b"]) / n if n else None,
        "mcnemar_p": mcnemar_exact(table["only_a"], table["only_b"]),
    }


def items_needed(discordant_rate: float, margin: float, confidence: float = 0.95) -> int:
    """Items for the CI half-width of delta to fall below `margin`, assuming the
    discordant pairs split evenly (normal approximation: half-width ~ z*sqrt(r/n))."""
    if margin <= 0:
        raise ValueError("margin must be positive")
    z = z_value(confidence)
    return max(1, math.ceil(discordant_rate * (z / margin) ** 2))
