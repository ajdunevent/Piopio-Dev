//! XEdDSA signatures with Curve25519 keys (Signal's specification), as used
//! by legacy OMEMO to sign the signed pre key with the identity key.

use curve25519_dalek::constants::ED25519_BASEPOINT_TABLE;
use curve25519_dalek::edwards::CompressedEdwardsY;
use curve25519_dalek::montgomery::MontgomeryPoint;
use curve25519_dalek::scalar::Scalar;
use sha2::{Digest, Sha512};

use crate::random;

/// Clamps a Curve25519 private key the way X25519 does.
pub(crate) fn clamp(mut k: [u8; 32]) -> [u8; 32] {
    k[0] &= 248;
    k[31] &= 127;
    k[31] |= 64;
    k
}

/// Signs `message` with the Curve25519 private key `private`.
pub fn sign(private: &[u8; 32], message: &[u8]) -> [u8; 64] {
    let k = Scalar::from_bytes_mod_order(clamp(*private));
    let e = (ED25519_BASEPOINT_TABLE * &k).compress();
    // Use the private key whose Edwards public key has sign bit 0.
    let a = if e.0[31] & 0x80 != 0 { -k } else { k };
    let mut a_pub = e.0;
    a_pub[31] &= 0x7f;

    let z: [u8; 64] = random();
    let mut h1 = Sha512::new();
    let mut prefix = [0xffu8; 32];
    prefix[0] = 0xfe; // hash_1: 2^256 - 2, little endian
    h1.update(prefix);
    h1.update(a.as_bytes());
    h1.update(message);
    h1.update(z);
    let r = Scalar::from_bytes_mod_order_wide(&h1.finalize().into());
    let r_point = (ED25519_BASEPOINT_TABLE * &r).compress();

    let mut h = Sha512::new();
    h.update(r_point.0);
    h.update(a_pub);
    h.update(message);
    let h = Scalar::from_bytes_mod_order_wide(&h.finalize().into());
    let s = r + h * a;

    let mut signature = [0u8; 64];
    signature[..32].copy_from_slice(&r_point.0);
    signature[32..].copy_from_slice(s.as_bytes());
    signature
}

/// Verifies an XEdDSA signature by the Curve25519 public key `public`.
///
/// Also accepts the older libsignal scheme that stored the Edwards sign bit
/// in the top bit of the signature.
pub fn verify(public: &[u8; 32], message: &[u8], signature: &[u8; 64]) -> bool {
    if verify_with_sign(public, message, signature, 0) {
        return true;
    }
    if signature[63] & 0x80 != 0 {
        let mut sig = *signature;
        sig[63] &= 0x7f;
        return verify_with_sign(public, message, &sig, 1);
    }
    false
}

fn verify_with_sign(public: &[u8; 32], message: &[u8], signature: &[u8; 64], sign: u8) -> bool {
    let Some(a) = MontgomeryPoint(*public).to_edwards(sign) else {
        return false;
    };
    let mut r_bytes = [0u8; 32];
    r_bytes.copy_from_slice(&signature[..32]);
    let mut s_bytes = [0u8; 32];
    s_bytes.copy_from_slice(&signature[32..]);
    if s_bytes[31] & 0xe0 != 0 {
        return false;
    }
    let Some(s) = Option::<Scalar>::from(Scalar::from_canonical_bytes(s_bytes)) else {
        return false;
    };
    if CompressedEdwardsY(r_bytes).decompress().is_none() {
        return false;
    }
    let a_compressed = a.compress();
    let mut h = Sha512::new();
    h.update(r_bytes);
    h.update(a_compressed.0);
    h.update(message);
    let h = Scalar::from_bytes_mod_order_wide(&h.finalize().into());
    let check = curve25519_dalek::EdwardsPoint::vartime_double_scalar_mul_basepoint(&h, &(-a), &s);
    check.compress().0 == r_bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use x25519_dalek::{PublicKey, StaticSecret};

    #[test]
    fn sign_and_verify() {
        for _ in 0..50 {
            let private: [u8; 32] = random();
            let public = PublicKey::from(&StaticSecret::from(private)).to_bytes();
            let sig = sign(&private, b"hello");
            assert!(verify(&public, b"hello", &sig));
            assert!(!verify(&public, b"hellO", &sig));
            let mut bad = sig;
            bad[5] ^= 1;
            assert!(!verify(&public, b"hello", &bad));
        }
    }
}
