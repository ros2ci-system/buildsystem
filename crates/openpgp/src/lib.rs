// Copyright 2026 The ros2ci authors
// SPDX-License-Identifier: Apache-2.0

//! OpenPGP signatures, as much as signing a package repository needs.
//!
//! apt and dnf check a repository's index against an OpenPGP signature.
//! This crate writes the three things that takes: the public key in the
//! form both tools import, a detached signature of a file, and a
//! cleartext-signed copy of a text file (apt's `InRelease`).
//!
//! It writes and never reads: there is no parser for keys or signatures
//! here, so nothing in this crate handles untrusted OpenPGP data.
//!
//! Only one kind of key is supported: a version 4 Ed25519 key that both
//! certifies and signs, with SHA-512 as the hash. The formats are those
//! of RFC 4880 with the EdDSA additions of RFC 9580, section 5.5.5.9
//! ("EdDSALegacy"). The signing itself is done by `ci_crypto`.

use ci_crypto::{
    base64_encode, hex_decode, hex_encode, sha1_for_key_names, sha512, Ed25519Key,
    ED25519_PUBLIC_LEN, ED25519_SEED_LEN,
};

/// Public-key algorithm 22: EdDSA with a curve named by an identifier.
const ALGORITHM_EDDSA: u8 = 22;
/// Hash algorithm 10: SHA-512.
const HASH_SHA512: u8 = 10;
/// The identifier of the Ed25519 curve, 1.3.6.1.4.1.11591.15.1.
const ED25519_OID: [u8; 9] = [0x2b, 0x06, 0x01, 0x04, 0x01, 0xda, 0x47, 0x0f, 0x01];

const SIGNATURE_BINARY: u8 = 0x00;
const SIGNATURE_TEXT: u8 = 0x01;
const SIGNATURE_POSITIVE_CERTIFICATION: u8 = 0x13;

/// The first line of a key file, so that a wrong file is refused.
const KEY_FILE_HEADER: &str = "ros2ci-signing-key 1";

/// A signing key with what makes it an OpenPGP key: when it was created
/// and whose it is. Both are part of the key's identity; changing either
/// gives a different fingerprint.
pub struct SigningKey {
    key: Ed25519Key,
    seed: [u8; ED25519_SEED_LEN],
    /// Seconds since 1970.
    created: u32,
    user_id: String,
}

impl SigningKey {
    /// Makes a new key. `user_id` is conventionally `Name <address>`.
    pub fn generate(user_id: &str, created: u32) -> Result<SigningKey, String> {
        check_user_id(user_id)?;
        let (key, seed) = Ed25519Key::generate();
        Ok(SigningKey {
            key,
            seed,
            created,
            user_id: user_id.to_string(),
        })
    }

    /// Reads a key back from the text `to_file_text` wrote.
    pub fn from_file_text(text: &str) -> Result<SigningKey, String> {
        let mut lines = text.lines();
        if lines.next() != Some(KEY_FILE_HEADER) {
            return Err("this is not a signing key file".to_string());
        }
        let mut created = None;
        let mut user_id = None;
        let mut seed = None;
        for line in lines {
            let (name, value) = line
                .split_once(": ")
                .ok_or("a line of the signing key file is malformed")?;
            match name {
                "created" => created = value.parse::<u32>().ok(),
                "user-id" => user_id = Some(value.to_string()),
                "seed" => {
                    seed =
                        hex_decode(value).and_then(|b| <[u8; ED25519_SEED_LEN]>::try_from(b).ok())
                }
                _ => return Err("a line of the signing key file is not understood".to_string()),
            }
        }
        let created = created.ok_or("the signing key file has no valid \"created\" line")?;
        let user_id = user_id.ok_or("the signing key file has no \"user-id\" line")?;
        let seed = seed.ok_or("the signing key file has no valid \"seed\" line")?;
        check_user_id(&user_id)?;
        Ok(SigningKey {
            key: Ed25519Key::from_seed(&seed),
            seed,
            created,
            user_id,
        })
    }

    /// The key as text to keep in a private file. Whoever reads this
    /// text can sign as this key.
    pub fn to_file_text(&self) -> String {
        format!(
            "{KEY_FILE_HEADER}\ncreated: {}\nuser-id: {}\nseed: {}\n",
            self.created,
            self.user_id,
            hex_encode(&self.seed)
        )
    }

    pub fn user_id(&self) -> &str {
        &self.user_id
    }

    /// The body of the public-key packet.
    fn public_key_body(&self) -> Vec<u8> {
        let mut body = vec![4];
        body.extend_from_slice(&self.created.to_be_bytes());
        body.push(ALGORITHM_EDDSA);
        body.push(ED25519_OID.len() as u8);
        body.extend_from_slice(&ED25519_OID);
        // The point travels as one number: a 0x40 prefix, which says
        // "compressed", then the 32 bytes of the public key.
        let mut point = Vec::with_capacity(1 + ED25519_PUBLIC_LEN);
        point.push(0x40);
        point.extend_from_slice(&self.key.public_key());
        push_mpi(&mut body, &point);
        body
    }

    /// What is hashed to name or certify the key: the public-key packet
    /// in its old-style framing with a two-byte length.
    fn public_key_hash_input(&self) -> Vec<u8> {
        let body = self.public_key_body();
        let mut out = vec![0x99];
        out.extend_from_slice(&(body.len() as u16).to_be_bytes());
        out.extend_from_slice(&body);
        out
    }

    /// The key's 20-byte fingerprint.
    pub fn fingerprint(&self) -> [u8; 20] {
        sha1_for_key_names(&self.public_key_hash_input())
    }

    /// The fingerprint as 40 upper-case hex digits, as tools print it.
    pub fn fingerprint_hex(&self) -> String {
        hex_encode(&self.fingerprint()).to_ascii_uppercase()
    }

    /// A signature packet of the given type over `hashed_data`, which is
    /// everything the signature covers before the signature's own fields.
    fn signature_packet(&self, kind: u8, hashed_data: &[u8], time: u32) -> Vec<u8> {
        let fingerprint = self.fingerprint();

        let mut hashed = Vec::new();
        // Subpacket 33: the fingerprint of the key that signed.
        hashed.extend_from_slice(&[22, 33, 4]);
        hashed.extend_from_slice(&fingerprint);
        // Subpacket 2: when the signature was made.
        hashed.extend_from_slice(&[5, 2]);
        hashed.extend_from_slice(&time.to_be_bytes());
        if kind == SIGNATURE_POSITIVE_CERTIFICATION {
            // Subpacket 27: the key may certify (0x01) and sign (0x02).
            hashed.extend_from_slice(&[2, 27, 0x03]);
            // Subpacket 21: the hashes the owner prefers; SHA-512, SHA-256.
            hashed.extend_from_slice(&[3, 21, HASH_SHA512, 8]);
        }

        let mut fields = vec![4, kind, ALGORITHM_EDDSA, HASH_SHA512];
        fields.extend_from_slice(&(hashed.len() as u16).to_be_bytes());
        fields.extend_from_slice(&hashed);

        let mut message = hashed_data.to_vec();
        message.extend_from_slice(&fields);
        message.extend_from_slice(&[4, 0xff]);
        message.extend_from_slice(&(fields.len() as u32).to_be_bytes());
        let digest = sha512(&message);
        let signature = self.key.sign(&digest);

        let mut body = fields;
        // Unhashed subpacket 16: the key id, the fingerprint's last eight
        // bytes, for tools that look the key up by it.
        body.extend_from_slice(&10u16.to_be_bytes());
        body.extend_from_slice(&[9, 16]);
        body.extend_from_slice(&fingerprint[12..]);
        body.extend_from_slice(&digest[..2]);
        push_mpi(&mut body, &signature[..32]);
        push_mpi(&mut body, &signature[32..]);
        packet(2, &body)
    }

    /// The public key with its user id and the self-signature that binds
    /// them, armored. This is the file a user gives to apt or dnf.
    pub fn public_key_armored(&self) -> String {
        let user_id = self.user_id.as_bytes();
        let mut certified = self.public_key_hash_input();
        certified.push(0xb4);
        certified.extend_from_slice(&(user_id.len() as u32).to_be_bytes());
        certified.extend_from_slice(user_id);

        let mut packets = packet(6, &self.public_key_body());
        packets.extend_from_slice(&packet(13, user_id));
        packets.extend_from_slice(&self.signature_packet(
            SIGNATURE_POSITIVE_CERTIFICATION,
            &certified,
            self.created,
        ));
        armor("PGP PUBLIC KEY BLOCK", &packets)
    }

    /// An armored signature of `data`, to publish beside it.
    pub fn sign_detached(&self, data: &[u8], time: u32) -> String {
        armor(
            "PGP SIGNATURE",
            &self.signature_packet(SIGNATURE_BINARY, data, time),
        )
    }

    /// `text` with its signature wrapped around it, in the cleartext
    /// signature framework. The text must end with a newline, and is
    /// shown unchanged apart from lines that start with a dash, which
    /// the format escapes.
    pub fn sign_cleartext(&self, text: &str, time: u32) -> Result<String, String> {
        let body = text
            .strip_suffix('\n')
            .ok_or("the text to sign must end with a newline")?;
        if body.contains('\r') {
            return Err("the text to sign must not contain carriage returns".to_string());
        }
        // What is signed is the text with trailing blanks removed from
        // each line and lines joined by CR LF. The last line's ending is
        // not part of it.
        let canonical = body
            .split('\n')
            .map(|line| line.trim_end_matches([' ', '\t']))
            .collect::<Vec<_>>()
            .join("\r\n");
        let mut out = String::from("-----BEGIN PGP SIGNED MESSAGE-----\nHash: SHA512\n\n");
        for line in body.split('\n') {
            if line.starts_with('-') {
                out.push_str("- ");
            }
            out.push_str(line);
            out.push('\n');
        }
        out.push_str(&armor(
            "PGP SIGNATURE",
            &self.signature_packet(SIGNATURE_TEXT, canonical.as_bytes(), time),
        ));
        Ok(out)
    }
}

/// A user id goes into a one-line file and into a packet with a short
/// length field.
fn check_user_id(user_id: &str) -> Result<(), String> {
    if user_id.is_empty() || user_id.len() > 200 || user_id.chars().any(char::is_control) {
        return Err("the user id must be one line of at most 200 bytes".to_string());
    }
    Ok(())
}

/// Appends a multiprecision integer: its length in bits, then its bytes
/// without leading zeros.
fn push_mpi(out: &mut Vec<u8>, bytes: &[u8]) {
    let start = bytes.iter().position(|&b| b != 0).unwrap_or(bytes.len());
    let bytes = &bytes[start..];
    let bits = match bytes.first() {
        Some(first) => bytes.len() * 8 - first.leading_zeros() as usize,
        None => 0,
    };
    out.extend_from_slice(&(bits as u16).to_be_bytes());
    out.extend_from_slice(bytes);
}

/// Frames a packet in the old style with a two-byte length.
fn packet(tag: u8, body: &[u8]) -> Vec<u8> {
    assert!(tag < 16 && body.len() <= usize::from(u16::MAX));
    let mut out = vec![0x80 | tag << 2 | 1];
    out.extend_from_slice(&(body.len() as u16).to_be_bytes());
    out.extend_from_slice(body);
    out
}

/// The checksum that ends an armored block.
fn crc24(data: &[u8]) -> u32 {
    let mut crc: u32 = 0x00b7_04ce;
    for &byte in data {
        crc ^= u32::from(byte) << 16;
        for _ in 0..8 {
            crc <<= 1;
            if crc & 0x0100_0000 != 0 {
                crc ^= 0x0186_4cfb;
            }
        }
    }
    crc & 0x00ff_ffff
}

fn armor(label: &str, data: &[u8]) -> String {
    let encoded = base64_encode(data);
    let mut out = format!("-----BEGIN {label}-----\n\n");
    for line in encoded.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(line).expect("base64 is ASCII"));
        out.push('\n');
    }
    let crc = crc24(data).to_be_bytes();
    out.push('=');
    out.push_str(&base64_encode(&crc[1..]));
    out.push('\n');
    out.push_str(&format!("-----END {label}-----\n"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ci_crypto::{base64_decode, ed25519_verify};

    const KEY_FILE: &str = "ros2ci-signing-key 1\n\
        created: 1795000000\n\
        user-id: Test Repository <test@example.invalid>\n\
        seed: 4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb\n";

    fn key() -> SigningKey {
        SigningKey::from_file_text(KEY_FILE).unwrap()
    }

    fn dearmor(text: &str, label: &str) -> Vec<u8> {
        let begin = format!("-----BEGIN {label}-----\n\n");
        let start = text.find(&begin).unwrap() + begin.len();
        let end = text.find(&format!("-----END {label}-----")).unwrap();
        let mut lines: Vec<&str> = text[start..end].lines().collect();
        let checksum = lines.pop().unwrap();
        let data = base64_decode(&lines.concat()).unwrap();
        let crc = crc24(&data).to_be_bytes();
        assert_eq!(checksum, format!("={}", base64_encode(&crc[1..])));
        data
    }

    /// Pulls the two halves of the Ed25519 signature out of a signature
    /// packet, and the hashed fields it covers.
    fn parse_signature(packet: &[u8]) -> (Vec<u8>, Vec<u8>) {
        assert_eq!(packet[0], 0x89);
        let body = &packet[3..];
        assert_eq!(
            body.len(),
            usize::from(u16::from_be_bytes([packet[1], packet[2]]))
        );
        let hashed_len = usize::from(u16::from_be_bytes([body[4], body[5]]));
        let fields = body[..6 + hashed_len].to_vec();
        let mut at = 6 + hashed_len;
        let unhashed_len = usize::from(u16::from_be_bytes([body[at], body[at + 1]]));
        at += 2 + unhashed_len + 2;
        let mut signature = Vec::new();
        for _ in 0..2 {
            let bits = usize::from(u16::from_be_bytes([body[at], body[at + 1]]));
            let len = bits.div_ceil(8);
            signature.extend(std::iter::repeat_n(0, 32 - len));
            signature.extend_from_slice(&body[at + 2..at + 2 + len]);
            at += 2 + len;
        }
        assert_eq!(at, body.len());
        (fields, signature)
    }

    fn verifies(key: &SigningKey, data: &[u8], packet: &[u8]) -> bool {
        let (fields, signature) = parse_signature(packet);
        let mut message = data.to_vec();
        message.extend_from_slice(&fields);
        message.extend_from_slice(&[4, 0xff]);
        message.extend_from_slice(&(fields.len() as u32).to_be_bytes());
        ed25519_verify(&key.key.public_key(), &sha512(&message), &signature)
    }

    #[test]
    fn key_file_round_trip() {
        assert_eq!(key().to_file_text(), KEY_FILE);
        let generated = SigningKey::generate("A <a@example.invalid>", 1).unwrap();
        let reread = SigningKey::from_file_text(&generated.to_file_text()).unwrap();
        assert_eq!(reread.fingerprint(), generated.fingerprint());
        assert!(SigningKey::from_file_text("something else\n").is_err());
        assert!(SigningKey::from_file_text("ros2ci-signing-key 1\ncreated: 1\n").is_err());
        assert!(SigningKey::generate("two\nlines", 1).is_err());
    }

    #[test]
    fn fingerprint_depends_on_creation_time() {
        let a = key();
        let b = SigningKey::from_file_text(&KEY_FILE.replace("1795000000", "1795000001")).unwrap();
        assert_ne!(a.fingerprint(), b.fingerprint());
        assert_eq!(a.fingerprint_hex().len(), 40);
    }

    #[test]
    fn mpi_lengths() {
        let mut out = Vec::new();
        push_mpi(&mut out, &[0x00, 0x01, 0xff]);
        assert_eq!(out, [0x00, 0x09, 0x01, 0xff]);
        out.clear();
        push_mpi(&mut out, &[0x40, 0x00]);
        assert_eq!(out, [0x00, 0x0f, 0x40, 0x00]);
        out.clear();
        push_mpi(&mut out, &[0, 0]);
        assert_eq!(out, [0, 0]);
    }

    #[test]
    fn crc24_known_answer() {
        // The check value of CRC-24/OPENPGP.
        assert_eq!(crc24(b"123456789"), 0x21cf02);
    }

    #[test]
    fn detached_signature_verifies() {
        let key = key();
        let packet = dearmor(
            &key.sign_detached(b"some data\n", 1795000100),
            "PGP SIGNATURE",
        );
        assert!(verifies(&key, b"some data\n", &packet));
        assert!(!verifies(&key, b"other data\n", &packet));
    }

    #[test]
    fn cleartext_signature_covers_the_canonical_text() {
        let key = key();
        let signed = key
            .sign_cleartext("Origin: test  \n-dash\nlast\n", 1795000100)
            .unwrap();
        assert!(signed.starts_with(
            "-----BEGIN PGP SIGNED MESSAGE-----\nHash: SHA512\n\nOrigin: test  \n- -dash\nlast\n-----BEGIN PGP SIGNATURE-----\n"
        ));
        let packet = dearmor(&signed, "PGP SIGNATURE");
        assert!(verifies(&key, b"Origin: test\r\n-dash\r\nlast", &packet));
        assert!(key.sign_cleartext("no newline", 1).is_err());
    }

    #[test]
    fn public_key_block_is_stable_and_self_signed() {
        let key = key();
        let armored = key.public_key_armored();
        assert_eq!(armored, key.public_key_armored());
        let packets = dearmor(&armored, "PGP PUBLIC KEY BLOCK");
        let key_len = 3 + key.public_key_body().len();
        let user_id_len = 3 + key.user_id().len();
        assert_eq!(packets[0], 0x99);
        assert_eq!(packets[key_len], 0xb5);
        let mut certified = key.public_key_hash_input();
        certified.push(0xb4);
        certified.extend_from_slice(&(key.user_id().len() as u32).to_be_bytes());
        certified.extend_from_slice(key.user_id().as_bytes());
        assert!(verifies(
            &key,
            &certified,
            &packets[key_len + user_id_len..]
        ));
    }
}
