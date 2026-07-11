//! Release manifest signature verification.
//!
//! Release manifests (`latest.json`) are signed OFFLINE with
//! `ssh-keygen -Y sign -n forest-release` using hardware-backed keys that never
//! touch CI or the release bucket. The CLI refuses to act on any manifest whose
//! signature doesn't verify against one of the pinned keys below, so a
//! compromise of the release host (or CI) cannot push binaries to existing
//! installs.
//!
//! Key custody rules:
//! - The list may only grow/rotate through a release signed by an existing key.
//! - Multiple keys are pinned so losing one never strands the update channel:
//!   sign with a surviving key, ship a release that rotates the list.
//! - Rollback protection comes from the version gate in update.rs: an old,
//!   genuinely-signed manifest can at worst report "no update available",
//!   never install a downgrade.

use anyhow::{bail, Context, Result};
use ssh_key::{PublicKey, SshSig};

/// Signatures must carry this namespace (`ssh-keygen -Y sign -n <ns>`), which
/// binds them to Forest releases: a signature made for any other purpose (SSH
/// auth, git commits, another project) can never verify here, and vice versa.
pub const SIG_NAMESPACE: &str = "forest-release";

/// Trusted release signing keys, in OpenSSH `authorized_keys` format.
/// Any single key's signature is sufficient.
pub const RELEASE_SIGNING_KEYS: &[&str] = &[
    // release-key-1: primary YubiKey 5 (FIDO2, resident + verify-required), enrolled 2026-07-11
    "sk-ssh-ed25519@openssh.com AAAAGnNrLXNzaC1lZDI1NTE5QG9wZW5zc2guY29tAAAAIJ0Qzjt2zkF9sR4E/VyJ2zasxvOYoVhtsgEeYo0YwkuYAAAAEnNzaDpmb3Jlc3QtcmVsZWFzZQ== forest-release-1",
    // release-key-2: backup YubiKey (FIDO2, resident + verify-required), enrolled 2026-07-11
    "sk-ssh-ed25519@openssh.com AAAAGnNrLXNzaC1lZDI1NTE5QG9wZW5zc2guY29tAAAAIIIKCvwQiwQbIKSquU3EfGk3ZLI0prwrmNkvr+xf8NFaAAAAEnNzaDpmb3Jlc3QtcmVsZWFzZQ== forest-release-2",
    // release-key-3: offline break-glass key (paper backup), enrolled 2026-07-11
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGh1nyRIGAdD/LVB3nLT4b4SUun+oEySwSGf85XtpbgQ forest-release-3",
];

/// Verify `sig_pem` (a `-----BEGIN SSH SIGNATURE-----` block produced by
/// `ssh-keygen -Y sign`) over the exact bytes of the manifest, against the
/// pinned key list.
pub fn verify_manifest_signature(manifest_bytes: &[u8], sig_pem: &str) -> Result<()> {
    let keys: Vec<PublicKey> = RELEASE_SIGNING_KEYS
        .iter()
        .map(|line| line.parse().context("invalid pinned release key (build defect)"))
        .collect::<Result<_>>()?;
    verify_with_keys(&keys, manifest_bytes, sig_pem)
}

fn verify_with_keys(keys: &[PublicKey], manifest_bytes: &[u8], sig_pem: &str) -> Result<()> {
    let sig = SshSig::from_pem(sig_pem)
        .context("release signature is malformed — not an SSH signature block")?;

    for key in keys {
        if key.verify(SIG_NAMESPACE, manifest_bytes, &sig).is_ok() {
            return Ok(());
        }
    }
    bail!(
        "release manifest signature does not verify against any trusted Forest release key — \
         refusing to trust this release"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ssh_key::{Algorithm, EcdsaCurve, HashAlg, LineEnding, PrivateKey};

    fn keypair(alg: Algorithm) -> PrivateKey {
        PrivateKey::random(&mut rand_core::OsRng, alg).unwrap()
    }

    fn sign(key: &PrivateKey, namespace: &str, msg: &[u8]) -> String {
        let sig = key.sign(namespace, HashAlg::Sha512, msg).unwrap();
        sig.to_pem(LineEnding::LF).unwrap()
    }

    const MANIFEST: &[u8] = br#"{"version":"9.9.9","files":[]}"#;

    #[test]
    fn pinned_keys_parse() {
        for line in RELEASE_SIGNING_KEYS {
            let key: PublicKey = line.parse().expect("pinned key must parse");
            assert!(key.algorithm().to_string().contains("ecdsa") || key.algorithm().to_string().contains("ed25519"));
        }
    }

    #[test]
    fn accepts_signature_from_trusted_key() {
        let signer = keypair(Algorithm::Ed25519);
        let sig_pem = sign(&signer, SIG_NAMESPACE, MANIFEST);
        verify_with_keys(&[signer.public_key().clone()], MANIFEST, &sig_pem).unwrap();
    }

    #[test]
    fn accepts_ecdsa_p256_signature() {
        let signer = keypair(Algorithm::Ecdsa { curve: EcdsaCurve::NistP256 });
        let sig_pem = sign(&signer, SIG_NAMESPACE, MANIFEST);
        verify_with_keys(&[signer.public_key().clone()], MANIFEST, &sig_pem).unwrap();
    }

    #[test]
    fn rejects_untrusted_key() {
        let signer = keypair(Algorithm::Ed25519);
        let trusted = keypair(Algorithm::Ed25519);
        let sig_pem = sign(&signer, SIG_NAMESPACE, MANIFEST);
        let err = verify_with_keys(&[trusted.public_key().clone()], MANIFEST, &sig_pem).unwrap_err();
        assert!(err.to_string().contains("does not verify"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_tampered_manifest() {
        let signer = keypair(Algorithm::Ed25519);
        let sig_pem = sign(&signer, SIG_NAMESPACE, MANIFEST);
        let tampered = br#"{"version":"9.9.10","files":[]}"#;
        assert!(verify_with_keys(&[signer.public_key().clone()], tampered, &sig_pem).is_err());
    }

    #[test]
    fn rejects_wrong_namespace() {
        // A signature made for any other purpose must never validate a release.
        let signer = keypair(Algorithm::Ed25519);
        let sig_pem = sign(&signer, "git-commit", MANIFEST);
        assert!(verify_with_keys(&[signer.public_key().clone()], MANIFEST, &sig_pem).is_err());
    }

    /// End-to-end proof of the production chain: this signature was produced by
    /// the actual release YubiKey (`ssh-keygen -Y sign` + touch) and must verify
    /// against the pinned production key list — not a test key.
    #[test]
    fn accepts_real_hardware_signature_from_pinned_release_key() {
        let manifest = include_bytes!("../tests/fixtures-manifest.json");
        let sig = include_str!("../tests/fixtures-manifest.json.sig");
        verify_manifest_signature(manifest, sig)
            .expect("hardware-signed fixture must verify against the pinned release key");
    }

    #[test]
    fn real_hardware_signature_rejects_tampered_manifest() {
        let sig = include_str!("../tests/fixtures-manifest.json.sig");
        let tampered = br#"{"version":"6.6.6","files":[]}"#;
        assert!(verify_manifest_signature(tampered, sig).is_err());
    }

    #[test]
    fn rejects_garbage_signature() {
        let trusted = keypair(Algorithm::Ed25519);
        let err = verify_with_keys(&[trusted.public_key().clone()], MANIFEST, "not a pem").unwrap_err();
        assert!(err.to_string().contains("malformed"), "unexpected error: {err}");
    }
}
