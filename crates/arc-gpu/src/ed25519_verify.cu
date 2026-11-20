#include <stdint.h>

typedef struct { uint64_t l[5]; } fe;

__constant__ fe FE_P = {{ 0x7FFFFFFFFFFED, 0x7FFFFFFFFFFFF, 0x7FFFFFFFFFFFF,
                          0x7FFFFFFFFFFFF, 0x7FFFFFFFFFFFF }};

__constant__ fe FE_2P = {{ 0xFFFFFFFFFFDA, 0xFFFFFFFFFFFFE, 0xFFFFFFFFFFFFE,
                           0xFFFFFFFFFFFFE, 0xFFFFFFFFFFFFE }};

__device__ void fe_zero(fe *r) {
    r->l[0] = r->l[1] = r->l[2] = r->l[3] = r->l[4] = 0;
}

__device__ void fe_one(fe *r) {
    r->l[0] = 1;
    r->l[1] = r->l[2] = r->l[3] = r->l[4] = 0;
}

__device__ void fe_copy(fe *r, const fe *a) {
    r->l[0] = a->l[0]; r->l[1] = a->l[1]; r->l[2] = a->l[2];
    r->l[3] = a->l[3]; r->l[4] = a->l[4];
}

__device__ void fe_add(fe *r, const fe *a, const fe *b) {
    r->l[0] = a->l[0] + b->l[0];
    r->l[1] = a->l[1] + b->l[1];
    r->l[2] = a->l[2] + b->l[2];
    r->l[3] = a->l[3] + b->l[3];
    r->l[4] = a->l[4] + b->l[4];
}

__device__ void fe_sub(fe *r, const fe *a, const fe *b) {
    r->l[0] = a->l[0] + FE_2P.l[0] - b->l[0];
    r->l[1] = a->l[1] + FE_2P.l[1] - b->l[1];
    r->l[2] = a->l[2] + FE_2P.l[2] - b->l[2];
    r->l[3] = a->l[3] + FE_2P.l[3] - b->l[3];
    r->l[4] = a->l[4] + FE_2P.l[4] - b->l[4];
}

__device__ void fe_reduce(fe *r) {
    uint64_t c;
    c = r->l[0] >> 51; r->l[1] += c; r->l[0] &= 0x7FFFFFFFFFFFF;
    c = r->l[1] >> 51; r->l[2] += c; r->l[1] &= 0x7FFFFFFFFFFFF;
    c = r->l[2] >> 51; r->l[3] += c; r->l[2] &= 0x7FFFFFFFFFFFF;
    c = r->l[3] >> 51; r->l[4] += c; r->l[3] &= 0x7FFFFFFFFFFFF;
    c = r->l[4] >> 51; r->l[0] += c * 19; r->l[4] &= 0x7FFFFFFFFFFFF;
    c = r->l[0] >> 51; r->l[1] += c; r->l[0] &= 0x7FFFFFFFFFFFF;
}

__device__ void fe_mul(fe *r, const fe *a, const fe *b) {
    uint64_t b19[5];
    b19[0] = b->l[0] * 19;
    b19[1] = b->l[1] * 19;
    b19[2] = b->l[2] * 19;
    b19[3] = b->l[3] * 19;
    b19[4] = b->l[4] * 19;

    uint64_t lo, hi, carry;

    uint64_t c0 = a->l[0] * b->l[0]
                + a->l[1] * b19[4]
                + a->l[2] * b19[3]
                + a->l[3] * b19[2]
                + a->l[4] * b19[1];

    uint64_t c1 = a->l[0] * b->l[1]
                + a->l[1] * b->l[0]
                + a->l[2] * b19[4]
                + a->l[3] * b19[3]
                + a->l[4] * b19[2];

    uint64_t c2 = a->l[0] * b->l[2]
                + a->l[1] * b->l[1]
                + a->l[2] * b->l[0]
                + a->l[3] * b19[4]
                + a->l[4] * b19[3];

    uint64_t c3 = a->l[0] * b->l[3]
                + a->l[1] * b->l[2]
                + a->l[2] * b->l[1]
                + a->l[3] * b->l[0]
                + a->l[4] * b19[4];

    uint64_t c4 = a->l[0] * b->l[4]
                + a->l[1] * b->l[3]
                + a->l[2] * b->l[2]
                + a->l[3] * b->l[1]
                + a->l[4] * b->l[0];

    r->l[0] = c0; r->l[1] = c1; r->l[2] = c2;
    r->l[3] = c3; r->l[4] = c4;
    fe_reduce(r);
}

__device__ void fe_sq(fe *r, const fe *a) {
    uint64_t a2_0 = 2 * a->l[0];
    uint64_t a2_1 = 2 * a->l[1];
    uint64_t a2_2 = 2 * a->l[2];
    uint64_t a19_3 = 19 * a->l[3];
    uint64_t a19_4 = 19 * a->l[4];
    uint64_t a2_19_4 = 2 * a19_4;

    r->l[0] = a->l[0] * a->l[0] + a2_1 * a19_4 + a->l[2] * (2 * a19_3);
    r->l[1] = a2_0 * a->l[1] + a->l[2] * a2_19_4 + a19_3 * a19_3;
    r->l[2] = a2_0 * a->l[2] + a->l[1] * a->l[1] + a->l[3] * a2_19_4;
    r->l[3] = a2_0 * a->l[3] + a2_1 * a->l[2] + a->l[4] * a19_4;
    r->l[4] = a2_0 * a->l[4] + a2_1 * a->l[3] + a->l[2] * a->l[2];
    fe_reduce(r);
}

__device__ void fe_sq_n(fe *r, const fe *a, int n) {
    fe_sq(r, a);
    for (int i = 1; i < n; i++) {
        fe_sq(r, r);
    }
}

__device__ void fe_invert(fe *r, const fe *a) {
    fe t0, t1, t2, t3;

    fe_sq(&t0, a);
    fe_sq(&t1, &t0);
    fe_sq(&t1, &t1);
    fe_mul(&t1, a, &t1);
    fe_mul(&t0, &t0, &t1);
    fe_sq(&t2, &t0);
    fe_mul(&t1, &t1, &t2);
    fe_sq_n(&t2, &t1, 5);  fe_mul(&t1, &t1, &t2);
    fe_sq_n(&t2, &t1, 10); fe_mul(&t2, &t1, &t2);
    fe_sq_n(&t3, &t2, 20); fe_mul(&t2, &t2, &t3);
    fe_sq_n(&t2, &t2, 10); fe_mul(&t1, &t1, &t2);
    fe_sq_n(&t2, &t1, 50); fe_mul(&t2, &t1, &t2);
    fe_sq_n(&t3, &t2, 100); fe_mul(&t2, &t2, &t3);
    fe_sq_n(&t2, &t2, 50); fe_mul(&t1, &t1, &t2);
    fe_sq_n(&t1, &t1, 5);  fe_mul(r, &t0, &t1);
}

__device__ void fe_neg(fe *r, const fe *a) {
    fe zero; fe_zero(&zero);
    fe_sub(r, &zero, a);
    fe_reduce(r);
}

typedef struct { fe X, Y, Z, T; } ge_p3;
typedef struct { fe X, Y, Z, T; } ge_p1p1;
typedef struct { fe yminusx, yplusx, xy2d; } ge_precomp;

__constant__ uint64_t D_LIMBS[5] = {
    0x34DCA135978A3, 0x1A8283B156EBD, 0x5E7A26001C029,
    0x739C663A03CBB, 0x52036CDC1B169
};

__constant__ uint64_t D2_LIMBS[5] = {
    0x69B9426B2F159, 0x35050762ADD7A, 0x3CF44C0038052,
    0x6738CC7407977, 0x2406D9DC56DFF
};

__device__ void ge_p3_0(ge_p3 *r) {
    fe_zero(&r->X); fe_one(&r->Y); fe_one(&r->Z); fe_zero(&r->T);
}

__device__ void ge_p3_dbl(ge_p1p1 *r, const ge_p3 *p) {
    fe A, B, C, D, E, F, G, H;
    fe_sq(&A, &p->X);
    fe_sq(&B, &p->Y);
    fe_sq(&C, &p->Z); fe_add(&C, &C, &C);
    fe_neg(&D, &A);

    fe_add(&E, &p->X, &p->Y); fe_sq(&E, &E);
    fe_sub(&E, &E, &A); fe_sub(&E, &E, &B);

    fe_add(&G, &D, &B);
    fe_sub(&F, &G, &C);
    fe_sub(&H, &D, &B);

    fe_mul(&r->X, &E, &F);
    fe_mul(&r->Y, &G, &H);
    fe_mul(&r->Z, &F, &G);
    fe_mul(&r->T, &E, &H);
}

__device__ void ge_p1p1_to_p3(ge_p3 *r, const ge_p1p1 *p) {
    fe_mul(&r->X, &p->X, &p->T);
    fe_mul(&r->Y, &p->Y, &p->Z);
    fe_mul(&r->Z, &p->Z, &p->T);
    fe_mul(&r->T, &p->X, &p->Y);
}

__device__ void ge_add(ge_p1p1 *r, const ge_p3 *p, const ge_p3 *q) {
    fe d_fe; d_fe.l[0] = D_LIMBS[0]; d_fe.l[1] = D_LIMBS[1];
    d_fe.l[2] = D_LIMBS[2]; d_fe.l[3] = D_LIMBS[3]; d_fe.l[4] = D_LIMBS[4];

    fe A, B, C, D, E, F, G, H, t0;
    fe_mul(&A, &p->X, &q->X);
    fe_mul(&B, &p->Y, &q->Y);
    fe_mul(&t0, &p->T, &q->T); fe_mul(&C, &t0, &d_fe);
    fe_mul(&D, &p->Z, &q->Z);

    fe_add(&t0, &p->X, &p->Y);
    fe tmp; fe_add(&tmp, &q->X, &q->Y);
    fe_mul(&E, &t0, &tmp); fe_sub(&E, &E, &A); fe_sub(&E, &E, &B);

    fe_sub(&F, &D, &C);
    fe_add(&G, &D, &C);
    fe_neg(&t0, &A);
    fe_add(&H, &B, &t0);

    fe_mul(&r->X, &E, &F);
    fe_mul(&r->Y, &G, &H);
    fe_mul(&r->Z, &F, &G);
    fe_mul(&r->T, &E, &H);
}

__device__ void ge_scalarmult_vartime(ge_p3 *r, const uint8_t scalar[32],
                                       const ge_p3 *point) {
    ge_p3_0(r);

    for (int byte = 31; byte >= 0; byte--) {
        for (int bit = 7; bit >= 0; bit--) {
            ge_p1p1 dbl; ge_p3_dbl(&dbl, r);
            ge_p1p1_to_p3(r, &dbl);

            if ((scalar[byte] >> bit) & 1) {
                ge_p1p1 sum; ge_add(&sum, r, point);
                ge_p1p1_to_p3(r, &sum);
            }
        }
    }
}

__device__ void fe_from_bytes(fe *r, const uint8_t bytes[32]) {
    uint64_t *b = (uint64_t *)bytes;
    r->l[0] =  b[0]                           & 0x7FFFFFFFFFFFF;
    r->l[1] = (b[0] >> 51 | b[1] << 13)       & 0x7FFFFFFFFFFFF;
    r->l[2] = (b[1] >> 38 | b[2] << 26)       & 0x7FFFFFFFFFFFF;
    r->l[3] = (b[2] >> 25 | b[3] << 39)       & 0x7FFFFFFFFFFFF;
    r->l[4] = (b[3] >> 12)                     & 0x7FFFFFFFFFFFF;
}

__device__ void fe_to_bytes(uint8_t bytes[32], const fe *a) {
    fe t; fe_copy(&t, a);
    fe_reduce(&t);
    fe_reduce(&t);

    uint64_t *b = (uint64_t *)bytes;
    b[0] = t.l[0] | (t.l[1] << 51);
    b[1] = (t.l[1] >> 13) | (t.l[2] << 38);
    b[2] = (t.l[2] >> 26) | (t.l[3] << 25);
    b[3] = (t.l[3] >> 39) | (t.l[4] << 12);
}

__device__ int ge_decompress(ge_p3 *r, const uint8_t compressed[32]) {
    fe d_fe; d_fe.l[0] = D_LIMBS[0]; d_fe.l[1] = D_LIMBS[1];
    d_fe.l[2] = D_LIMBS[2]; d_fe.l[3] = D_LIMBS[3]; d_fe.l[4] = D_LIMBS[4];

    int x_sign = (compressed[31] >> 7) & 1;
    uint8_t y_bytes[32];
    for (int i = 0; i < 32; i++) y_bytes[i] = compressed[i];
    y_bytes[31] &= 0x7F;

    fe_from_bytes(&r->Y, y_bytes);
    fe_one(&r->Z);

    fe y2, u, v;
    fe_sq(&y2, &r->Y);
    fe one; fe_one(&one);
    fe_sub(&u, &y2, &one);
    fe_mul(&v, &y2, &d_fe);
    fe_add(&v, &v, &one);

    fe v3, v7, uv7, x;
    fe_sq(&v3, &v); fe_mul(&v3, &v3, &v);
    fe_sq(&v7, &v3); fe_mul(&v7, &v7, &v);
    fe_mul(&uv7, &u, &v7);

    fe p58;
    fe_sq_n(&p58, &uv7, 1);
    fe_sq_n(&p58, &p58, 1);
    fe_mul(&p58, &p58, &uv7);
    fe_sq_n(&p58, &p58, 1);
    fe_mul(&p58, &p58, &uv7);

    fe_mul(&x, &u, &v3);
    fe_mul(&x, &x, &p58);

    uint8_t x_bytes[32];
    fe_to_bytes(x_bytes, &x);
    if ((x_bytes[0] & 1) != x_sign) {
        fe_neg(&x, &x);
    }

    fe_copy(&r->X, &x);
    fe_mul(&r->T, &r->X, &r->Y);

    return 1;
}

__constant__ uint8_t BASEPOINT_COMPRESSED[32] = {
    0x58, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66,
    0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66,
    0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66,
    0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66
};

__device__ int ge_p3_equal(const ge_p3 *p, const ge_p3 *q) {
    fe lhs, rhs;

    fe_mul(&lhs, &p->X, &q->Z);
    fe_mul(&rhs, &q->X, &p->Z);
    fe_sub(&lhs, &lhs, &rhs);
    fe_reduce(&lhs);

    uint8_t x_diff[32];
    fe_to_bytes(x_diff, &lhs);
    for (int i = 0; i < 32; i++) {
        if (x_diff[i] != 0) return 0;
    }

    fe_mul(&lhs, &p->Y, &q->Z);
    fe_mul(&rhs, &q->Y, &p->Z);
    fe_sub(&lhs, &lhs, &rhs);
    fe_reduce(&lhs);

    uint8_t y_diff[32];
    fe_to_bytes(y_diff, &lhs);
    for (int i = 0; i < 32; i++) {
        if (y_diff[i] != 0) return 0;
    }

    return 1;
}

extern "C" __global__ void ed25519_verify_kernel(
    const uint32_t *input,
    uint32_t       *output,
    const uint32_t *params
) {
    uint32_t idx = blockIdx.x * blockDim.x + threadIdx.x;
    uint32_t N = params[0];

    if (idx >= N) return;

    const uint32_t *sig_data = &input[idx * 32];

    uint8_t R_bytes[32], S_bytes[32], k_bytes[32], A_bytes[32];

    for (int i = 0; i < 8; i++) {
        uint32_t w;
        w = sig_data[i];
        R_bytes[i*4+0] = w & 0xFF; R_bytes[i*4+1] = (w>>8) & 0xFF;
        R_bytes[i*4+2] = (w>>16) & 0xFF; R_bytes[i*4+3] = (w>>24) & 0xFF;

        w = sig_data[8+i];
        S_bytes[i*4+0] = w & 0xFF; S_bytes[i*4+1] = (w>>8) & 0xFF;
        S_bytes[i*4+2] = (w>>16) & 0xFF; S_bytes[i*4+3] = (w>>24) & 0xFF;

        w = sig_data[16+i];
        k_bytes[i*4+0] = w & 0xFF; k_bytes[i*4+1] = (w>>8) & 0xFF;
        k_bytes[i*4+2] = (w>>16) & 0xFF; k_bytes[i*4+3] = (w>>24) & 0xFF;

        w = sig_data[24+i];
        A_bytes[i*4+0] = w & 0xFF; A_bytes[i*4+1] = (w>>8) & 0xFF;
        A_bytes[i*4+2] = (w>>16) & 0xFF; A_bytes[i*4+3] = (w>>24) & 0xFF;
    }

    ge_p3 R_point, A_point;
    if (!ge_decompress(&R_point, R_bytes) || !ge_decompress(&A_point, A_bytes)) {
        output[idx] = 0;
        return;
    }

    ge_p3 B;
    ge_decompress(&B, BASEPOINT_COMPRESSED);

    ge_p3 sB;
    ge_scalarmult_vartime(&sB, S_bytes, &B);

    ge_p3 kA;
    ge_scalarmult_vartime(&kA, k_bytes, &A_point);

    ge_p1p1 sum_p1p1;
    ge_add(&sum_p1p1, &R_point, &kA);
    ge_p3 R_plus_kA;
    ge_p1p1_to_p3(&R_plus_kA, &sum_p1p1);

    output[idx] = ge_p3_equal(&sB, &R_plus_kA) ? 1 : 0;
}
