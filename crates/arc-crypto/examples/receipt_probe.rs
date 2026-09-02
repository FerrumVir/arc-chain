//! Print the full proof receipt for a fixed layer, for differential testing.
use arc_crypto::inference_proof::dense_forward_i64;
use arc_crypto::stwo_air::{try_prove_dense_packed, try_prove_dense_stark};

fn data(seed: &str, len: usize) -> Vec<i64> {
    let mut r: u64 = 0;
    for b in seed.bytes() {
        r = r.wrapping_mul(31).wrapping_add(b as u64);
    }
    (0..len)
        .map(|_| {
            r = r
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((r >> 33) as i64) % 10 - 5
        })
        .collect()
}

fn main() {
    for (o, i) in [(64usize, 256usize), (128, 512)] {
        let w = data("dw", o * i);
        let b = vec![0i64; o];
        let x = data("dx", i);
        let y = dense_forward_i64(&w, &b, &x, i, o);
        let (u, _, _) = try_prove_dense_stark(&w, &x, &y, &b, i, o).unwrap();
        let (p, _, _) = try_prove_dense_packed(&w, &x, &y, &b, i, o).unwrap();
        println!("{o}x{i} unpacked {}", hex::encode(&u));
        println!("{o}x{i} packed   {}", hex::encode(&p));
    }
}
