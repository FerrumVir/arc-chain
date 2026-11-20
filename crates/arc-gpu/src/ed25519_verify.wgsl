@group(0) @binding(0) var<storage, read> sig_data: array<u32>;

@group(0) @binding(1) var<storage, read_write> results: array<u32>;

struct Params { num_items: u32 }
@group(0) @binding(2) var<uniform> params: Params;

@group(0) @binding(3) var<storage, read> base_table: array<u32>;

fn mul_wide(a: u32, b: u32) -> vec2<u32> {
    let a0 = a & 0xFFFFu;
    let a1 = a >> 16u;
    let b0 = b & 0xFFFFu;
    let b1 = b >> 16u;

    let p00 = a0 * b0;
    let p01 = a0 * b1;
    let p10 = a1 * b0;
    let p11 = a1 * b1;

    let mid = p01 + (p00 >> 16u);
    let mid_hi = mid >> 16u;
    let mid_lo = mid & 0xFFFFu;

    let mid2 = (mid_lo + p10);
    let mid2_carry = mid2 >> 16u;

    let lo = (p00 & 0xFFFFu) | (mid2 << 16u);
    let hi = p11 + mid_hi + mid2_carry;

    return vec2<u32>(lo, hi);
}

fn add64(a: vec2<u32>, b: vec2<u32>) -> vec2<u32> {
    let lo = a.x + b.x;
    let carry = select(0u, 1u, lo < a.x);
    let hi = a.y + b.y + carry;
    return vec2<u32>(lo, hi);
}

fn shr64(v: vec2<u32>, n: u32) -> vec2<u32> {
    if (n == 0u) { return v; }
    let lo = (v.x >> n) | (v.y << (32u - n));
    let hi = v.y >> n;
    return vec2<u32>(lo, hi);
}

fn fe_frombytes(base: u32) -> array<u32, 10> {
    var b: array<u32, 8>;
    for (var i = 0u; i < 8u; i++) {
        b[i] = sig_data[base + i];
    }

    var f: array<u32, 10>;
    f[0] = b[0] & 0x3FFFFFFu;
    f[1] = ((b[0] >> 26u) | (b[1] << 6u)) & 0x1FFFFFFu;
    f[2] = ((b[1] >> 19u) | (b[2] << 13u)) & 0x3FFFFFFu;
    f[3] = ((b[2] >> 13u) | (b[3] << 19u)) & 0x1FFFFFFu;
    f[4] = (b[3] >> 6u) & 0x3FFFFFFu;
    f[5] = b[4] & 0x1FFFFFFu;
    f[6] = ((b[4] >> 25u) | (b[5] << 7u)) & 0x3FFFFFFu;
    f[7] = ((b[5] >> 19u) | (b[6] << 13u)) & 0x1FFFFFFu;
    f[8] = ((b[6] >> 12u) | (b[7] << 20u)) & 0x3FFFFFFu;
    f[9] = (b[7] >> 6u) & 0x1FFFFFFu;

    return f;
}

fn fe_tobytes(f_in: array<u32, 10>) -> array<u32, 8> {
    var f = fe_freeze(f_in);

    var out: array<u32, 8>;
    out[0] = f[0] | (f[1] << 26u);
    out[1] = (f[1] >> 6u) | (f[2] << 19u);
    out[2] = (f[2] >> 13u) | (f[3] << 13u);
    out[3] = (f[3] >> 19u) | (f[4] << 6u);
    out[4] = f[5] | (f[6] << 25u);
    out[5] = (f[6] >> 7u) | (f[7] << 19u);
    out[6] = (f[7] >> 13u) | (f[8] << 12u);
    out[7] = (f[8] >> 20u) | (f[9] << 6u);

    return out;
}

fn fe_zero() -> array<u32, 10> {
    return array<u32, 10>(0u,0u,0u,0u,0u,0u,0u,0u,0u,0u);
}

fn fe_one() -> array<u32, 10> {
    return array<u32, 10>(1u,0u,0u,0u,0u,0u,0u,0u,0u,0u);
}

fn fe_add(a: array<u32, 10>, b: array<u32, 10>) -> array<u32, 10> {
    var r: array<u32, 10>;
    for (var i = 0u; i < 10u; i++) {
        r[i] = a[i] + b[i];
    }
    return r;
}

fn fe_sub(a: array<u32, 10>, b: array<u32, 10>) -> array<u32, 10> {
    var r: array<u32, 10>;
    let bias = array<u32, 10>(
        0x7FFFFDAu, 0x3FFFFFEu, 0x7FFFFFEu, 0x3FFFFFEu, 0x7FFFFFEu,
        0x3FFFFFEu, 0x7FFFFFEu, 0x3FFFFFEu, 0x7FFFFFEu, 0x3FFFFFEu
    );
    for (var i = 0u; i < 10u; i++) {
        r[i] = (a[i] + bias[i]) - b[i];
    }
    return fe_reduce(r);
}

fn fe_neg(a: array<u32, 10>) -> array<u32, 10> {
    return fe_sub(fe_zero(), a);
}

fn fe_reduce(f: array<u32, 10>) -> array<u32, 10> {
    var r = f;

    for (var round = 0u; round < 2u; round++) {
        for (var i = 0u; i < 9u; i++) {
            if (i % 2u == 0u) {
                let carry = r[i] >> 26u;
                r[i] = r[i] & 0x3FFFFFFu;
                r[i + 1u] = r[i + 1u] + carry;
            } else {
                let carry = r[i] >> 25u;
                r[i] = r[i] & 0x1FFFFFFu;
                r[i + 1u] = r[i + 1u] + carry;
            }
        }
        let carry9 = r[9] >> 25u;
        r[9] = r[9] & 0x1FFFFFFu;
        r[0] = r[0] + carry9 * 19u;
    }

    return r;
}

fn fe_mul(a: array<u32, 10>, b: array<u32, 10>) -> array<u32, 10> {
    var b19: array<u32, 10>;
    for (var i = 0u; i < 10u; i++) {
        b19[i] = b[i] * 19u;
    }

    var a2: array<u32, 10>;
    for (var i = 0u; i < 10u; i++) {
        a2[i] = a[i] * 2u;
    }

    var h: array<vec2<u32>, 10>;
    for (var i = 0u; i < 10u; i++) {
        h[i] = vec2<u32>(0u, 0u);
    }

    for (var i = 0u; i < 10u; i++) {
        let i_odd = (i & 1u) == 1u;
        for (var j = 0u; j < 10u; j++) {
            let k = i + j;
            let both_odd = i_odd && ((j & 1u) == 1u);
            let ai = select(a[i], a2[i], both_odd);
            let bj = select(b[j], b19[j], k >= 10u);
            let idx = k % 10u;
            let prod = mul_wide(ai, bj);
            h[idx] = add64(h[idx], prod);
        }
    }

    var r: array<u32, 10>;
    var carry = vec2<u32>(0u, 0u);
    for (var i = 0u; i < 10u; i++) {
        let sum = add64(h[i], carry);
        if (i % 2u == 0u) {
            r[i] = sum.x & 0x3FFFFFFu;
            carry = shr64(sum, 26u);
        } else {
            r[i] = sum.x & 0x1FFFFFFu;
            carry = shr64(sum, 25u);
        }
    }
    let carry19 = mul_wide(carry.x, 19u);
    let carry19_hi = carry.y * 19u;
    let carry19_full = add64(carry19, vec2<u32>(0u, carry19_hi));
    var r0_wide = add64(vec2<u32>(r[0], 0u), carry19_full);
    r[0] = r0_wide.x & 0x3FFFFFFu;
    let extra_carry = shr64(r0_wide, 26u);
    r[1] = r[1] + extra_carry.x;

    return fe_reduce(r);
}

fn fe_sq(a: array<u32, 10>) -> array<u32, 10> {
    return fe_mul(a, a);
}

fn fe_sq_n(a: array<u32, 10>, n: u32) -> array<u32, 10> {
    var r = a;
    for (var i = 0u; i < n; i++) {
        r = fe_sq(r);
    }
    return r;
}

fn fe_invert(z: array<u32, 10>) -> array<u32, 10> {
    let z2 = fe_sq(z);
    let z9 = fe_mul(fe_sq_n(z2, 2u), z);
    let z11 = fe_mul(z9, z2);
    let z_5_0 = fe_mul(fe_sq(z11), z9);
    let z_10_0 = fe_mul(fe_sq_n(z_5_0, 5u), z_5_0);
    let z_20_0 = fe_mul(fe_sq_n(z_10_0, 10u), z_10_0);
    let z_40_0 = fe_mul(fe_sq_n(z_20_0, 20u), z_20_0);
    let z_50_0 = fe_mul(fe_sq_n(z_40_0, 10u), z_10_0);
    let z_100_0 = fe_mul(fe_sq_n(z_50_0, 50u), z_50_0);
    let z_200_0 = fe_mul(fe_sq_n(z_100_0, 100u), z_100_0);
    let z_250_0 = fe_mul(fe_sq_n(z_200_0, 50u), z_50_0);
    return fe_mul(fe_sq_n(z_250_0, 5u), z11);
}

fn fe_pow2523(z: array<u32, 10>) -> array<u32, 10> {
    let z2 = fe_sq(z);
    let z9 = fe_mul(fe_sq_n(z2, 2u), z);
    let z11 = fe_mul(z9, z2);
    let z_5_0 = fe_mul(fe_sq(z11), z9);
    let z_10_0 = fe_mul(fe_sq_n(z_5_0, 5u), z_5_0);
    let z_20_0 = fe_mul(fe_sq_n(z_10_0, 10u), z_10_0);
    let z_40_0 = fe_mul(fe_sq_n(z_20_0, 20u), z_20_0);
    let z_50_0 = fe_mul(fe_sq_n(z_40_0, 10u), z_10_0);
    let z_100_0 = fe_mul(fe_sq_n(z_50_0, 50u), z_50_0);
    let z_200_0 = fe_mul(fe_sq_n(z_100_0, 100u), z_100_0);
    let z_250_0 = fe_mul(fe_sq_n(z_200_0, 50u), z_50_0);
    return fe_mul(fe_sq_n(z_250_0, 2u), z);
}

fn fe_freeze(f_in: array<u32, 10>) -> array<u32, 10> {
    var h = fe_reduce(f_in);

    var q = (h[0] + 19u) >> 26u;
    q = (h[1] + q) >> 25u;
    q = (h[2] + q) >> 26u;
    q = (h[3] + q) >> 25u;
    q = (h[4] + q) >> 26u;
    q = (h[5] + q) >> 25u;
    q = (h[6] + q) >> 26u;
    q = (h[7] + q) >> 25u;
    q = (h[8] + q) >> 26u;
    q = (h[9] + q) >> 25u;

    h[0] += 19u * q;

    var carry: u32;
    carry = h[0] >> 26u; h[0] &= 0x3FFFFFFu; h[1] += carry;
    carry = h[1] >> 25u; h[1] &= 0x1FFFFFFu; h[2] += carry;
    carry = h[2] >> 26u; h[2] &= 0x3FFFFFFu; h[3] += carry;
    carry = h[3] >> 25u; h[3] &= 0x1FFFFFFu; h[4] += carry;
    carry = h[4] >> 26u; h[4] &= 0x3FFFFFFu; h[5] += carry;
    carry = h[5] >> 25u; h[5] &= 0x1FFFFFFu; h[6] += carry;
    carry = h[6] >> 26u; h[6] &= 0x3FFFFFFu; h[7] += carry;
    carry = h[7] >> 25u; h[7] &= 0x1FFFFFFu; h[8] += carry;
    carry = h[8] >> 26u; h[8] &= 0x3FFFFFFu; h[9] += carry;
    h[9] &= 0x1FFFFFFu;

    return h;
}

fn fe_is_zero(a: array<u32, 10>) -> bool {
    let r = fe_freeze(a);
    var acc = 0u;
    for (var i = 0u; i < 10u; i++) {
        acc = acc | r[i];
    }
    return acc == 0u;
}

fn fe_is_negative(a: array<u32, 10>) -> bool {
    let r = fe_freeze(a);
    return (r[0] & 1u) == 1u;
}

fn fe_eq(a: array<u32, 10>, b: array<u32, 10>) -> bool {
    return fe_is_zero(fe_sub(a, b));
}

struct GePoint {
    x: array<u32, 10>,
    y: array<u32, 10>,
    z: array<u32, 10>,
    t: array<u32, 10>,
}

fn ge_zero() -> GePoint {
    return GePoint(
        fe_zero(),
        fe_one(),
        fe_one(),
        fe_zero()
    );
}

fn fe_d() -> array<u32, 10> {
    return array<u32, 10>(
        0x35978A3u, 0x0D37284u, 0x3156EBDu, 0x06A0A0Eu, 0x001C029u,
        0x179E898u, 0x3A03CBBu, 0x1CE7198u, 0x2E2B6FFu, 0x1480DB3u
    );
}

fn fe_2d() -> array<u32, 10> {
    return array<u32, 10>(
        0x2B2F159u, 0x1A6E509u, 0x22ADD7Au, 0x0D4141Du, 0x0038052u,
        0x0F3D130u, 0x3407977u, 0x19CE331u, 0x1C56DFFu, 0x0901B67u
    );
}

fn fe_sqrtm1() -> array<u32, 10> {
    return array<u32, 10>(
        0x20EA0B0u, 0x186C9D2u, 0x08F189Du, 0x035697Fu, 0x0BD0C60u,
        0x1FBD7A7u, 0x2804C9Eu, 0x1E16569u, 0x004FC1Du, 0x0AE0C92u
    );
}

fn bt_read_fe(offset: u32) -> array<u32, 10> {
    return array<u32, 10>(
        base_table[offset], base_table[offset + 1u], base_table[offset + 2u],
        base_table[offset + 3u], base_table[offset + 4u], base_table[offset + 5u],
        base_table[offset + 6u], base_table[offset + 7u], base_table[offset + 8u],
        base_table[offset + 9u]
    );
}

fn b_table_lookup(idx: u32) -> GePoint {
    let base = idx * 30u;
    return GePoint(
        bt_read_fe(base),
        bt_read_fe(base + 10u),
        fe_one(),
        bt_read_fe(base + 20u)
    );
}

fn ge_frombytes(base: u32) -> GePoint {
    var y_bytes: array<u32, 8>;
    for (var i = 0u; i < 8u; i++) {
        y_bytes[i] = sig_data[base + i];
    }
    let x_sign = (y_bytes[7] >> 31u) & 1u;
    y_bytes[7] = y_bytes[7] & 0x7FFFFFFFu;

    var y_raw: array<u32, 10>;
    y_raw[0] = y_bytes[0] & 0x3FFFFFFu;
    y_raw[1] = ((y_bytes[0] >> 26u) | (y_bytes[1] << 6u)) & 0x1FFFFFFu;
    y_raw[2] = ((y_bytes[1] >> 19u) | (y_bytes[2] << 13u)) & 0x3FFFFFFu;
    y_raw[3] = ((y_bytes[2] >> 13u) | (y_bytes[3] << 19u)) & 0x1FFFFFFu;
    y_raw[4] = (y_bytes[3] >> 6u) & 0x3FFFFFFu;
    y_raw[5] = y_bytes[4] & 0x1FFFFFFu;
    y_raw[6] = ((y_bytes[4] >> 25u) | (y_bytes[5] << 7u)) & 0x3FFFFFFu;
    y_raw[7] = ((y_bytes[5] >> 19u) | (y_bytes[6] << 13u)) & 0x1FFFFFFu;
    y_raw[8] = ((y_bytes[6] >> 12u) | (y_bytes[7] << 20u)) & 0x3FFFFFFu;
    y_raw[9] = (y_bytes[7] >> 6u) & 0x1FFFFFFu;

    let y = fe_reduce(y_raw);

    let y2 = fe_sq(y);
    let u = fe_sub(y2, fe_one());
    let v = fe_add(fe_mul(fe_d(), y2), fe_one());

    let v3 = fe_mul(fe_sq(v), v);
    let v7 = fe_mul(fe_sq(v3), v);
    let uv7 = fe_mul(u, v7);
    let x_candidate = fe_mul(fe_mul(u, v3), fe_pow2523(uv7));

    let vx2 = fe_mul(v, fe_sq(x_candidate));
    var x = x_candidate;

    if (!fe_eq(vx2, u)) {
        x = fe_mul(x, fe_sqrtm1());
        let vx2_neg = fe_mul(v, fe_sq(x));
        if (!fe_eq(vx2_neg, u)) {
            return ge_zero();
        }
    }

    if (fe_is_negative(x) != (x_sign == 1u)) {
        x = fe_neg(x);
    }

    let t = fe_mul(x, y);

    return GePoint(x, y, fe_one(), t);
}

fn ge_add(p: GePoint, q: GePoint) -> GePoint {
    let a = fe_mul(fe_sub(p.y, p.x), fe_sub(q.y, q.x));
    let b = fe_mul(fe_add(p.y, p.x), fe_add(q.y, q.x));
    let c = fe_mul(fe_mul(p.t, q.t), fe_2d());
    let d = fe_mul(p.z, fe_add(q.z, q.z));

    let e = fe_sub(b, a);
    let f = fe_sub(d, c);
    let g = fe_add(d, c);
    let h = fe_add(b, a);

    return GePoint(
        fe_mul(e, f),
        fe_mul(g, h),
        fe_mul(f, g),
        fe_mul(e, h)
    );
}

fn ge_double(p: GePoint) -> GePoint {
    let a = fe_sq(p.x);
    let b = fe_sq(p.y);
    let c_inner = fe_sq(p.z);
    let c = fe_add(c_inner, c_inner);
    let d = fe_neg(a);

    let e = fe_sub(fe_sq(fe_add(p.x, p.y)), fe_add(a, b));
    let g = fe_add(d, b);
    let f = fe_sub(g, c);
    let h = fe_sub(d, b);

    return GePoint(
        fe_mul(e, f),
        fe_mul(g, h),
        fe_mul(f, g),
        fe_mul(e, h)
    );
}

fn load_scalar(base: u32) -> array<u32, 8> {
    var s: array<u32, 8>;
    for (var i = 0u; i < 8u; i++) {
        s[i] = sig_data[base + i];
    }
    return s;
}

fn scalar_bit(s: array<u32, 8>, bit: u32) -> u32 {
    let word = bit / 32u;
    let pos = bit % 32u;
    return (s[word] >> pos) & 1u;
}

fn ge_double_scalarmult(a: array<u32, 8>, b: array<u32, 8>, p: GePoint) -> GePoint {
    let bp = b_table_lookup(1u);
    let bpp = ge_add(bp, p);

    var result = ge_zero();
    var found_one = false;
    for (var i_rev = 0u; i_rev < 256u; i_rev++) {
        let i = 255u - i_rev;
        if (found_one) {
            result = ge_double(result);
        }
        let ab = scalar_bit(a, i);
        let bb = scalar_bit(b, i);

        if (ab == 1u && bb == 1u) {
            if (found_one) { result = ge_add(result, bpp); }
            else { result = bpp; found_one = true; }
        } else if (ab == 1u) {
            if (found_one) { result = ge_add(result, bp); }
            else { result = bp; found_one = true; }
        } else if (bb == 1u) {
            if (found_one) { result = ge_add(result, p); }
            else { result = p; found_one = true; }
        }
    }
    return result;
}

fn ge_eq(a: GePoint, b: GePoint) -> bool {
    let lhs_x = fe_mul(a.x, b.z);
    let rhs_x = fe_mul(b.x, a.z);
    let lhs_y = fe_mul(a.y, b.z);
    let rhs_y = fe_mul(b.y, a.z);
    return fe_eq(lhs_x, rhs_x) && fe_eq(lhs_y, rhs_y);
}

@compute @workgroup_size(64)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let idx = gid.x;
    if (idx >= params.num_items) {
        return;
    }

    let base = idx * 32u;
    let point_R = ge_frombytes(base + 0u);
    let point_A = ge_frombytes(base + 24u);

    let r_is_id = fe_is_zero(point_R.x) && fe_eq(point_R.y, fe_one());
    let a_is_id = fe_is_zero(point_A.x) && fe_eq(point_A.y, fe_one());
    if (r_is_id || a_is_id) {
        results[idx] = 0u;
        return;
    }

    let scalar_S = load_scalar(base + 8u);
    let scalar_k = load_scalar(base + 16u);

    let neg_A = GePoint(fe_neg(point_A.x), point_A.y, point_A.z, fe_neg(point_A.t));
    let check = ge_double_scalarmult(scalar_S, scalar_k, neg_A);

    let valid = ge_eq(check, point_R);
    results[idx] = select(0u, 1u, valid);
}
