//! Minimal OpenSSH `SSHSIG` verification for the release-manifest signature.
//!
//! This is the check `install.sh` performs with
//! `ssh-keygen -Y verify -f <allowed signers> -I arc-release -n arc-release-manifest-v1`,
//! where the allowed-signers file holds exactly one Ed25519 key. Accepting a
//! signature therefore means: the blob is a version-1 SSHSIG, it was made by
//! that exact key, for that exact namespace, over SHA-512 or SHA-256 of the
//! message (PROTOCOL.sshsig), and the Ed25519 signature verifies strictly.

use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::{Signature, VerifyingKey};
use sha2::{Digest, Sha256, Sha512};

const MAGIC: &[u8] = b"SSHSIG";
const ARMOR_BEGIN: &str = "-----BEGIN SSH SIGNATURE-----";
const ARMOR_END: &str = "-----END SSH SIGNATURE-----";
const KEY_TYPE: &str = "ssh-ed25519";
const MAX_ARMORED_BYTES: usize = 16 * 1024;

/// Parse an OpenSSH `ssh-ed25519 AAAA...` public key into its 32 key bytes.
pub fn parse_ed25519_public_key(openssh: &str) -> Result<[u8; 32]> {
    let mut parts = openssh.split_whitespace();
    let kind = parts
        .next()
        .ok_or_else(|| anyhow!("empty OpenSSH public key"))?;
    ensure!(
        kind == KEY_TYPE,
        "only ssh-ed25519 release keys are supported"
    );
    let encoded = parts
        .next()
        .ok_or_else(|| anyhow!("OpenSSH public key has no key data"))?;
    let blob = STANDARD
        .decode(encoded)
        .context("OpenSSH public key is not base64")?;
    let mut reader = Reader::new(&blob);
    ensure!(
        reader.string()? == KEY_TYPE.as_bytes(),
        "OpenSSH public key blob has the wrong key type"
    );
    let key = key_bytes(reader.string()?)?;
    reader.finish()?;
    Ok(key)
}

/// Verify an armored SSHSIG over `message` for `namespace` and `trusted_key`.
pub fn verify(
    armored: &str,
    message: &[u8],
    namespace: &str,
    trusted_key: &[u8; 32],
) -> Result<()> {
    ensure!(
        armored.len() <= MAX_ARMORED_BYTES,
        "SSH signature is unreasonably large"
    );
    let blob = dearmor(armored)?;
    let mut reader = Reader::new(&blob);
    ensure!(
        reader.bytes(MAGIC.len())? == MAGIC,
        "signature is not an SSHSIG blob"
    );
    ensure!(reader.u32()? == 1, "unsupported SSHSIG version");
    let public_key_blob = reader.string()?;
    let signed_namespace = reader.string()?;
    let reserved = reader.string()?;
    let hash_algorithm = reader.string()?;
    let signature_blob = reader.string()?;
    reader.finish()?;

    let mut key_reader = Reader::new(public_key_blob);
    ensure!(
        key_reader.string()? == KEY_TYPE.as_bytes(),
        "SSHSIG signer is not an ssh-ed25519 key"
    );
    let signer = key_bytes(key_reader.string()?)?;
    key_reader.finish()?;
    ensure!(
        &signer == trusted_key,
        "SSHSIG was made by a key that is not the pinned release key"
    );
    ensure!(
        signed_namespace == namespace.as_bytes(),
        "SSHSIG namespace does not match the release-manifest namespace"
    );

    let digest = match hash_algorithm {
        b"sha512" => Sha512::digest(message).to_vec(),
        b"sha256" => Sha256::digest(message).to_vec(),
        _ => bail!("unsupported SSHSIG hash algorithm"),
    };

    let mut signature_reader = Reader::new(signature_blob);
    ensure!(
        signature_reader.string()? == KEY_TYPE.as_bytes(),
        "SSHSIG signature is not ssh-ed25519"
    );
    let raw_signature = signature_reader.string()?;
    signature_reader.finish()?;
    let raw_signature = <[u8; 64]>::try_from(raw_signature)
        .map_err(|_| anyhow!("Ed25519 signature must be 64 bytes"))?;

    let mut signed = Vec::with_capacity(
        MAGIC.len()
            + 16
            + signed_namespace.len()
            + reserved.len()
            + hash_algorithm.len()
            + digest.len(),
    );
    signed.extend_from_slice(MAGIC);
    put_string(&mut signed, signed_namespace);
    put_string(&mut signed, reserved);
    put_string(&mut signed, hash_algorithm);
    put_string(&mut signed, &digest);

    let verifying_key = VerifyingKey::from_bytes(trusted_key)
        .map_err(|_| anyhow!("pinned release key is not a valid Ed25519 point"))?;
    verifying_key
        .verify_strict(&signed, &Signature::from_bytes(&raw_signature))
        .map_err(|_| anyhow!("release-manifest signature does not verify"))
}

fn key_bytes(bytes: &[u8]) -> Result<[u8; 32]> {
    <[u8; 32]>::try_from(bytes).map_err(|_| anyhow!("Ed25519 public key must be 32 bytes"))
}

fn dearmor(armored: &str) -> Result<Vec<u8>> {
    let mut lines = armored
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    ensure!(
        lines.next() == Some(ARMOR_BEGIN),
        "missing SSH signature header"
    );
    let mut body = String::new();
    let mut ended = false;
    for line in lines.by_ref() {
        if line == ARMOR_END {
            ended = true;
            break;
        }
        body.push_str(line);
    }
    ensure!(ended, "missing SSH signature footer");
    ensure!(
        lines.next().is_none(),
        "unexpected data after the SSH signature"
    );
    STANDARD
        .decode(body.as_bytes())
        .context("SSH signature body is not base64")
}

fn put_string(out: &mut Vec<u8>, value: &[u8]) {
    let len = u32::try_from(value.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(value);
}

/// Reader for SSH wire-format `uint32` and length-prefixed `string` fields.
struct Reader<'a> {
    data: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Reader { data, offset: 0 }
    }

    fn bytes(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self
            .offset
            .checked_add(len)
            .ok_or_else(|| anyhow!("SSH field length overflows"))?;
        let slice = self
            .data
            .get(self.offset..end)
            .ok_or_else(|| anyhow!("truncated SSH structure"))?;
        self.offset = end;
        Ok(slice)
    }

    fn u32(&mut self) -> Result<u32> {
        let raw = self.bytes(4)?;
        Ok(u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]))
    }

    fn string(&mut self) -> Result<&'a [u8]> {
        let len = usize::try_from(self.u32()?).map_err(|_| anyhow!("SSH field is too long"))?;
        self.bytes(len)
    }

    fn finish(&self) -> Result<()> {
        ensure!(
            self.offset == self.data.len(),
            "trailing bytes in SSH structure"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &[u8] = include_bytes!("../fixtures/v0.8.11/SHA256SUMS");
    const SIGNATURE: &str = include_str!("../fixtures/v0.8.11/SHA256SUMS.sig");
    const RELEASE_KEY: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPs2NAiDRXit9EM96A2GdXZgRqvXtl0lvryEAEAEjQfY";
    const NAMESPACE: &str = "arc-release-manifest-v1";

    fn release_key() -> [u8; 32] {
        parse_ed25519_public_key(RELEASE_KEY).unwrap()
    }

    #[test]
    fn real_v0810_release_manifest_signature_verifies() {
        verify(SIGNATURE, MANIFEST, NAMESPACE, &release_key()).unwrap();
    }

    #[test]
    fn any_manifest_byte_change_is_rejected() {
        for index in [0, 40, MANIFEST.len() / 2, MANIFEST.len() - 2] {
            let mut tampered = MANIFEST.to_vec();
            tampered[index] ^= 0x01;
            assert!(
                verify(SIGNATURE, &tampered, NAMESPACE, &release_key()).is_err(),
                "byte {index}"
            );
        }
        let mut appended = MANIFEST.to_vec();
        appended.extend_from_slice(
            b"0000000000000000000000000000000000000000000000000000000000000000  evil\n",
        );
        assert!(verify(SIGNATURE, &appended, NAMESPACE, &release_key()).is_err());
    }

    #[test]
    fn wrong_namespace_or_key_is_rejected() {
        assert!(verify(SIGNATURE, MANIFEST, "file", &release_key()).is_err());
        let mut other_key = release_key();
        other_key[31] ^= 0x80;
        assert!(verify(SIGNATURE, MANIFEST, NAMESPACE, &other_key).is_err());
    }

    #[test]
    fn malformed_armor_is_rejected() {
        assert!(verify("", MANIFEST, NAMESPACE, &release_key()).is_err());
        let no_footer = SIGNATURE.replace(ARMOR_END, "");
        assert!(verify(&no_footer, MANIFEST, NAMESPACE, &release_key()).is_err());
        let trailing = format!("{SIGNATURE}\nextra\n");
        assert!(verify(&trailing, MANIFEST, NAMESPACE, &release_key()).is_err());
        let truncated = SIGNATURE.replacen("Bg==", "", 1);
        assert!(verify(&truncated, MANIFEST, NAMESPACE, &release_key()).is_err());
    }

    #[test]
    fn public_key_parser_rejects_other_types() {
        assert!(parse_ed25519_public_key("ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQ").is_err());
        assert!(parse_ed25519_public_key("ssh-ed25519").is_err());
        assert!(parse_ed25519_public_key("ssh-ed25519 !!!").is_err());
    }
}
