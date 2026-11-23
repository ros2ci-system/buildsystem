// Copyright 2026 The ros2ci authors
// SPDX-License-Identifier: Apache-2.0

//! The client side of SCRAM-SHA-256 (RFC 5802 and RFC 7677), the password
//! exchange PostgreSQL uses.
//!
//! The password never crosses the wire. The client proves it knows the
//! password, and the server proves it knows a verifier derived from it, so
//! a server that does not hold the verifier is detected.

use std::num::NonZeroU32;

use ci_crypto::{
    base64_decode, base64_encode, constant_time_eq, hmac_sha256, pbkdf2_hmac_sha256, sha256,
};

/// Servers choose the iteration count. Refuse counts so low that the
/// exchange gives little protection, or so high that a hostile server
/// could tie up the client.
const MIN_ITERATIONS: u32 = 4096;
const MAX_ITERATIONS: u32 = 10_000_000;

pub struct Client {
    password: String,
    nonce: String,
    first_bare: String,
}

/// What the client holds after sending its final message, to check the
/// server's reply against.
pub struct Pending {
    server_signature: [u8; 32],
}

impl Client {
    /// `nonce` must be freshly random for every exchange.
    pub fn new(username: &str, password: &str, nonce: &str) -> Client {
        // '=' and ',' are the two characters with meaning in the message.
        let escaped = username.replace('=', "=3D").replace(',', "=2C");
        Client {
            password: password.to_string(),
            nonce: nonce.to_string(),
            first_bare: format!("n={escaped},r={nonce}"),
        }
    }

    /// The first message. "n,," says channel binding is not used.
    pub fn first_message(&self) -> String {
        format!("n,,{}", self.first_bare)
    }

    /// Takes the server's first message and produces the client's final
    /// message.
    pub fn final_message(self, server_first: &str) -> Result<(String, Pending), &'static str> {
        let mut combined_nonce = None;
        let mut salt = None;
        let mut iterations = None;
        for field in server_first.split(',') {
            if let Some(value) = field.strip_prefix("r=") {
                combined_nonce = Some(value);
            } else if let Some(value) = field.strip_prefix("s=") {
                salt = Some(base64_decode(value).ok_or("the server's salt is not valid base64")?);
            } else if let Some(value) = field.strip_prefix("i=") {
                iterations = Some(
                    value
                        .parse::<u32>()
                        .map_err(|_| "the server's iteration count is not a number")?,
                );
            } else if field.starts_with("m=") {
                return Err("the server requires an unsupported SCRAM extension");
            }
        }
        let combined_nonce = combined_nonce.ok_or("the server's message has no nonce")?;
        let salt = salt.ok_or("the server's message has no salt")?;
        let iterations = iterations.ok_or("the server's message has no iteration count")?;

        // The server's nonce must extend ours; otherwise this reply
        // belongs to some other exchange.
        if !combined_nonce.starts_with(&self.nonce) || combined_nonce.len() <= self.nonce.len() {
            return Err("the server's nonce does not extend the client's");
        }
        if !(MIN_ITERATIONS..=MAX_ITERATIONS).contains(&iterations) {
            return Err("the server's iteration count is out of range");
        }
        let iterations =
            NonZeroU32::new(iterations).ok_or("the server's iteration count is out of range")?;

        let salted_password = pbkdf2_hmac_sha256(self.password.as_bytes(), &salt, iterations);
        let client_key = hmac_sha256(&salted_password, b"Client Key");
        let stored_key = sha256(&client_key);
        // "biws" is base64 for "n,,", repeating the channel-binding choice.
        let final_without_proof = format!("c=biws,r={combined_nonce}");
        let auth_message = format!(
            "{},{},{}",
            self.first_bare, server_first, final_without_proof
        );
        let client_signature = hmac_sha256(&stored_key, auth_message.as_bytes());
        let mut proof = client_key;
        for (p, s) in proof.iter_mut().zip(client_signature) {
            *p ^= s;
        }
        let server_key = hmac_sha256(&salted_password, b"Server Key");
        let server_signature = hmac_sha256(&server_key, auth_message.as_bytes());

        Ok((
            format!("{final_without_proof},p={}", base64_encode(&proof)),
            Pending { server_signature },
        ))
    }
}

impl Pending {
    /// Checks the server's final message, which proves the server knows
    /// the password's verifier.
    pub fn verify(self, server_final: &str) -> Result<(), &'static str> {
        if server_final.starts_with("e=") {
            return Err("the server rejected the password exchange");
        }
        let signature = server_final
            .split(',')
            .find_map(|field| field.strip_prefix("v="))
            .and_then(base64_decode)
            .ok_or("the server's final message has no signature")?;
        if constant_time_eq(&signature, &self.server_signature) {
            Ok(())
        } else {
            Err("the server's signature is wrong; it does not know the password")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The example exchange from RFC 7677, section 3.
    const NONCE: &str = "rOprNGfwEbeRWgbNEkqO";
    const SERVER_FIRST: &str =
        "r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096";

    #[test]
    fn rfc_7677_example() {
        let client = Client::new("user", "pencil", NONCE);
        assert_eq!(client.first_message(), "n,,n=user,r=rOprNGfwEbeRWgbNEkqO");
        let (final_message, pending) = client.final_message(SERVER_FIRST).unwrap();
        assert_eq!(
            final_message,
            "c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,p=dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ="
        );
        pending
            .verify("v=6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4=")
            .unwrap();
    }

    #[test]
    fn wrong_server_signature_is_rejected() {
        let client = Client::new("user", "pencil", NONCE);
        let (_, pending) = client.final_message(SERVER_FIRST).unwrap();
        assert!(pending
            .verify("v=6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G5=")
            .is_err());

        let client = Client::new("user", "wrong password", NONCE);
        let (_, pending) = client.final_message(SERVER_FIRST).unwrap();
        assert!(pending
            .verify("v=6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4=")
            .is_err());

        let client = Client::new("user", "pencil", NONCE);
        let (_, pending) = client.final_message(SERVER_FIRST).unwrap();
        assert!(pending.verify("e=invalid-proof").is_err());

        let client = Client::new("user", "pencil", NONCE);
        let (_, pending) = client.final_message(SERVER_FIRST).unwrap();
        assert!(pending.verify("").is_err());
    }

    #[test]
    fn bad_server_first_messages_are_rejected() {
        for bad in [
            "",
            "s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096",
            "r=rOprNGfwEbeRWgbNEkqOmore,i=4096",
            "r=rOprNGfwEbeRWgbNEkqOmore,s=W22ZaJ0SNY7soEsUEjb6gQ==",
            // A nonce that does not start with ours.
            "r=someoneElsesNonce,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096",
            // A nonce the server added nothing to.
            "r=rOprNGfwEbeRWgbNEkqO,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096",
            "r=rOprNGfwEbeRWgbNEkqOmore,s=not base64!,i=4096",
            "r=rOprNGfwEbeRWgbNEkqOmore,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=many",
            "r=rOprNGfwEbeRWgbNEkqOmore,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=1",
            "r=rOprNGfwEbeRWgbNEkqOmore,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4000000000",
            "m=ext,r=rOprNGfwEbeRWgbNEkqOmore,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096",
        ] {
            let client = Client::new("user", "pencil", NONCE);
            assert!(client.final_message(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn username_is_escaped() {
        let client = Client::new("a=b,c", "pw", "nonce");
        assert_eq!(client.first_message(), "n,,n=a=3Db=2Cc,r=nonce");
    }
}
