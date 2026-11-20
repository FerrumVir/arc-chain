use arc_crypto::{Hash256, KeyPair};

pub fn signing_keypairs(count: usize) -> Vec<(KeyPair, Hash256)> {
    (0..count)
        .map(|_| {
            let keypair = KeyPair::generate_ed25519();
            let address = keypair.address();
            (keypair, address)
        })
        .collect()
}

pub fn addresses(count: usize) -> Vec<Hash256> {
    signing_keypairs(count)
        .into_iter()
        .map(|(_, address)| address)
        .collect()
}
