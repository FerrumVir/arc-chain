pub mod book;
pub mod certificate;
pub mod lease;
pub mod link;
pub mod placement;
pub mod queue;
pub mod reservation;
pub mod resident;
pub mod verify;

pub use arc_crypto::Hash256;

pub type Address = Hash256;

pub(crate) fn digest<T: serde::Serialize>(domain: &str, value: &T) -> Hash256 {
    let mut hasher = blake3::Hasher::new_derive_key(domain);
    hasher.update(&bincode::serialize(value).expect("in-memory value serialises"));
    Hash256(*hasher.finalize().as_bytes())
}
