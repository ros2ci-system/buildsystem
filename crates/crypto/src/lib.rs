// Copyright 2026 The ros2ci authors
// SPDX-License-Identifier: Apache-2.0

//! Hashing, message authentication, signatures, key derivation and random numbers,
//! plus the base64 and hex encodings that travel with them.
//!
//! The cryptography itself comes from `ring`, the provider that rustls is
//! built with. Nothing cryptographic is implemented here; this crate is
//! the one place the rest of the system reaches it through.

use std::num::NonZeroU32;

use ring::rand::SecureRandom;

pub const SHA256_LEN: usize = 32;

pub fn sha256(data: &[u8]) -> [u8; SHA256_LEN] {
    let digest = ring::digest::digest(&ring::digest::SHA256, data);
    let mut out = [0u8; SHA256_LEN];
    out.copy_from_slice(digest.as_ref());
    out
}

/// Hashes data that arrives in pieces.
pub struct Sha256(ring::digest::Context);

impl Sha256 {
    pub fn new() -> Sha256 {
        Sha256(ring::digest::Context::new(&ring::digest::SHA256))
    }

    pub fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    pub fn finish(self) -> [u8; SHA256_LEN] {
        let mut out = [0u8; SHA256_LEN];
        out.copy_from_slice(self.0.finish().as_ref());
        out
    }
}

impl Default for Sha256 {
    fn default() -> Sha256 {
        Sha256::new()
    }
}

pub const SHA512_LEN: usize = 64;

pub fn sha512(data: &[u8]) -> [u8; SHA512_LEN] {
    let digest = ring::digest::digest(&ring::digest::SHA512, data);
    let mut out = [0u8; SHA512_LEN];
    out.copy_from_slice(digest.as_ref());
    out
}

pub const SHA1_LEN: usize = 20;

/// SHA-1, which is broken as a secure hash. It is here only because an
/// OpenPGP version 4 key is named by the SHA-1 of its public part.
/// Nothing may rely on it for security.
pub fn sha1_for_key_names(data: &[u8]) -> [u8; SHA1_LEN] {
    let digest = ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, data);
    let mut out = [0u8; SHA1_LEN];
    out.copy_from_slice(digest.as_ref());
    out
}

pub const ED25519_SEED_LEN: usize = 32;
pub const ED25519_PUBLIC_LEN: usize = 32;
pub const ED25519_SIGNATURE_LEN: usize = 64;

/// An Ed25519 signing key, held as its 32-byte seed.
pub struct Ed25519Key(ring::signature::Ed25519KeyPair);

impl Ed25519Key {
    /// A new key from the operating system's random numbers. Returns the
    /// seed too, which is all that has to be stored to load it again.
    pub fn generate() -> (Ed25519Key, [u8; ED25519_SEED_LEN]) {
        let mut seed = [0u8; ED25519_SEED_LEN];
        random_bytes(&mut seed);
        let key = Ed25519Key::from_seed(&seed);
        (key, seed)
    }

    pub fn from_seed(seed: &[u8; ED25519_SEED_LEN]) -> Ed25519Key {
        Ed25519Key(
            ring::signature::Ed25519KeyPair::from_seed_unchecked(seed)
                .expect("a 32-byte seed is always a valid Ed25519 key"),
        )
    }

    pub fn public_key(&self) -> [u8; ED25519_PUBLIC_LEN] {
        use ring::signature::KeyPair;
        let mut out = [0u8; ED25519_PUBLIC_LEN];
        out.copy_from_slice(self.0.public_key().as_ref());
        out
    }

    pub fn sign(&self, message: &[u8]) -> [u8; ED25519_SIGNATURE_LEN] {
        let mut out = [0u8; ED25519_SIGNATURE_LEN];
        out.copy_from_slice(self.0.sign(message).as_ref());
        out
    }
}

/// Whether `signature` is this public key's Ed25519 signature of
/// `message`.
pub fn ed25519_verify(public_key: &[u8], message: &[u8], signature: &[u8]) -> bool {
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, public_key)
        .verify(message, signature)
        .is_ok()
}

pub fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; SHA256_LEN] {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key);
    let tag = ring::hmac::sign(&key, data);
    let mut out = [0u8; SHA256_LEN];
    out.copy_from_slice(tag.as_ref());
    out
}

/// PBKDF2 with HMAC-SHA-256, producing a 32-byte key.
pub fn pbkdf2_hmac_sha256(
    password: &[u8],
    salt: &[u8],
    iterations: NonZeroU32,
) -> [u8; SHA256_LEN] {
    let mut out = [0u8; SHA256_LEN];
    ring::pbkdf2::derive(
        ring::pbkdf2::PBKDF2_HMAC_SHA256,
        iterations,
        salt,
        password,
        &mut out,
    );
    out
}

/// Fills `out` with random bytes from the operating system.
///
/// # Panics
///
/// Panics if the operating system cannot supply random bytes. There is no
/// safe way to continue without them.
pub fn random_bytes(out: &mut [u8]) {
    ring::rand::SystemRandom::new()
        .fill(out)
        .expect("the operating system could not supply random bytes");
}

/// A random token of `bytes` bytes of entropy, as lowercase hex.
pub fn random_token(bytes: usize) -> String {
    let mut raw = vec![0u8; bytes];
    random_bytes(&mut raw);
    hex_encode(&raw)
}

/// Compares two byte strings in time that depends only on their lengths,
/// so that a secret cannot be guessed one byte at a time.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut difference = 0u8;
    for (x, y) in a.iter().zip(b) {
        difference |= x ^ y;
    }
    // Keep the compiler from turning the loop back into an early exit.
    std::hint::black_box(difference) == 0
}

pub fn hex_encode(data: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(data.len() * 2);
    for &byte in data {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0xf) as usize] as char);
    }
    out
}

pub fn hex_decode(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let digit = |b: u8| match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    };
    bytes
        .chunks(2)
        .map(|pair| Some(digit(pair[0])? << 4 | digit(pair[1])?))
        .collect()
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding.
pub fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        out.push(BASE64[(n >> 18) as usize & 63] as char);
        out.push(BASE64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            BASE64[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            BASE64[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Decodes standard base64. The padding must be present and correct, and
/// no whitespace or other characters are allowed.
pub fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let value = |b: u8| match b {
        b'A'..=b'Z' => Some(u32::from(b - b'A')),
        b'a'..=b'z' => Some(u32::from(b - b'a') + 26),
        b'0'..=b'9' => Some(u32::from(b - b'0') + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    };
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    let groups = bytes.len() / 4;
    for (index, group) in bytes.chunks(4).enumerate() {
        let padding = group.iter().rev().take_while(|&&b| b == b'=').count();
        if padding > 2 || (padding > 0 && index != groups - 1) {
            return None;
        }
        let mut n = 0u32;
        for &b in &group[..4 - padding] {
            n = n << 6 | value(b)?;
        }
        n <<= 6 * padding as u32;
        // Bits that the padding says are unused must be zero, so that each
        // byte string has exactly one encoding.
        let unused_bits = match padding {
            1 => n & 0xff,
            2 => n & 0xffff,
            _ => 0,
        };
        if unused_bits != 0 {
            return None;
        }
        out.push((n >> 16) as u8);
        if padding < 2 {
            out.push((n >> 8) as u8);
        }
        if padding < 1 {
            out.push(n as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_known_answers() {
        assert_eq!(
            hex_encode(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex_encode(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let mut pieces = Sha256::new();
        pieces.update(b"a");
        pieces.update(b"bc");
        assert_eq!(pieces.finish(), sha256(b"abc"));
    }

    #[test]
    fn other_hashes_known_answers() {
        assert_eq!(
            hex_encode(&sha1_for_key_names(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(
            hex_encode(&sha512(b"abc")),
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a\
             2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
        );
    }

    #[test]
    fn ed25519_known_answer() {
        // RFC 8032, section 7.1, test 2.
        let seed: [u8; 32] =
            hex_decode("4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb")
                .unwrap()
                .try_into()
                .unwrap();
        let key = Ed25519Key::from_seed(&seed);
        assert_eq!(
            hex_encode(&key.public_key()),
            "3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c"
        );
        let signature = key.sign(&[0x72]);
        assert_eq!(
            hex_encode(&signature),
            "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da\
             085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00"
        );
        assert!(ed25519_verify(&key.public_key(), &[0x72], &signature));
        assert!(!ed25519_verify(&key.public_key(), &[0x73], &signature));
        let (generated, stored) = Ed25519Key::generate();
        assert_eq!(
            Ed25519Key::from_seed(&stored).public_key(),
            generated.public_key()
        );
    }

    #[test]
    fn hmac_known_answer() {
        // RFC 4231, test case 2.
        assert_eq!(
            hex_encode(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn pbkdf2_known_answer() {
        // RFC 7914, section 11, truncated to the first 32 bytes.
        assert_eq!(
            hex_encode(&pbkdf2_hmac_sha256(
                b"passwd",
                b"salt",
                NonZeroU32::new(1).unwrap()
            )),
            "55ac046e56e3089fec1691c22544b605f94185216dde0465e68b9d57c20dacbc"
        );
    }

    #[test]
    fn random_is_not_constant() {
        let a = random_token(16);
        let b = random_token(16);
        assert_eq!(a.len(), 32);
        assert_ne!(a, b);
    }

    #[test]
    fn constant_time_comparison() {
        assert!(constant_time_eq(b"", b""));
        assert!(constant_time_eq(b"same", b"same"));
        assert!(!constant_time_eq(b"same", b"sama"));
        assert!(!constant_time_eq(b"short", b"longer"));
    }

    #[test]
    fn hex() {
        assert_eq!(hex_encode(&[0x00, 0x0f, 0xa5, 0xff]), "000fa5ff");
        assert_eq!(hex_decode("000FA5ff"), Some(vec![0x00, 0x0f, 0xa5, 0xff]));
        assert_eq!(hex_decode(""), Some(vec![]));
        assert_eq!(hex_decode("abc"), None);
        assert_eq!(hex_decode("zz"), None);
    }

    #[test]
    fn base64_known_answers() {
        // RFC 4648, section 10.
        for (plain, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64_encode(plain.as_bytes()), encoded);
            assert_eq!(base64_decode(encoded), Some(plain.as_bytes().to_vec()));
        }
        assert_eq!(base64_encode(&[0xfb, 0xff, 0xbf]), "+/+/");
    }

    #[test]
    fn base64_rejects_malformed_input() {
        for bad in [
            "Zg", "Zg=", "Z===", "====", "Zg==Zg==", "Zm9v\n", "Zm9 v", "Zm9-", "Zh==", "Zm9=",
        ] {
            assert_eq!(base64_decode(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn base64_round_trip() {
        for len in 0..70 {
            let data: Vec<u8> = (0..len).map(|i| (i * 37 + len) as u8).collect();
            assert_eq!(base64_decode(&base64_encode(&data)), Some(data));
        }
    }
}
