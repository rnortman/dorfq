//! The GitHub App credential: the private key, and the JWT it signs.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ring::rand::SystemRandom;
use ring::rsa::KeyPair;
use ring::signature::RSA_PKCS1_SHA256;
use serde_json::json;

/// How far before `now` a JWT's `iat` is set, so a local clock up to this
/// far ahead of GitHub's still issues a JWT GitHub accepts.
const JWT_BACKDATE: Duration = Duration::from_secs(60);
/// How far after `now` a JWT's `exp` is set. GitHub refuses more than ten
/// minutes; a JWT is used within seconds of being signed and never kept.
const JWT_LIFETIME: Duration = Duration::from_secs(5 * 60);

/// The JWT header GitHub requires: RS256.
const JWT_HEADER: &str = r#"{"alg":"RS256","typ":"JWT"}"#;

/// A GitHub App's signing credential. A clone shares the one parsed key.
#[derive(Clone)]
pub struct AppKey {
    /// The app's Client ID (or numeric App ID; GitHub accepts either as the
    /// JWT issuer).
    client_id: String,
    /// Behind an `Arc`: a key pair is a few hundred bytes and not `Clone`,
    /// and the enums that carry an `AppKey` beside a token string would
    /// otherwise all be that size.
    key: Arc<KeyPair>,
}

impl AppKey {
    /// Parses the PEM GitHub issues for an app (`-----BEGIN RSA PRIVATE
    /// KEY-----`, PKCS#1) or its PKCS#8 form (`-----BEGIN PRIVATE
    /// KEY-----`). The message names what was wrong: not PEM, an unexpected
    /// label, header lines (a passphrase-protected key), a body that is not
    /// base64, or ring's reason for rejecting the key.
    pub fn from_pem(client_id: &str, pem: &str) -> Result<AppKey, String> {
        let (label, body) = pem_block(pem)?;
        // Base64 has no ':'; one in the block is an RFC 1421 header line,
        // which only an encrypted key (`Proc-Type: 4,ENCRYPTED`) carries.
        if body.contains(':') {
            return Err(
                "the PEM block has header lines (Proc-Type, DEK-Info): the key is \
                 passphrase-protected; dorfq needs it unencrypted"
                    .to_owned(),
            );
        }
        let der = STANDARD
            .decode(body)
            .map_err(|e| format!("the PEM body is not base64: {e}"))?;
        let key = match label {
            "RSA PRIVATE KEY" => KeyPair::from_der(&der),
            "PRIVATE KEY" => KeyPair::from_pkcs8(&der),
            other => {
                return Err(format!(
                    "unexpected PEM label {other:?}; expected \"RSA PRIVATE KEY\" or \"PRIVATE KEY\""
                ));
            }
        }
        .map_err(|e| format!("not a usable RSA private key: {e}"))?;
        Ok(AppKey {
            client_id: client_id.to_owned(),
            key: Arc::new(key),
        })
    }

    /// A JWT GitHub accepts as this app, valid from `now - 60 s` to
    /// `now + 5 min`: header `{"alg":"RS256","typ":"JWT"}`, claims
    /// `{"iat","exp","iss"}`, RS256 signature, each segment base64url
    /// without padding.
    pub fn jwt(&self, now: SystemTime) -> Result<String, String> {
        let issued = now
            .duration_since(UNIX_EPOCH + JWT_BACKDATE)
            .map_err(|_| "the local clock is before 1970-01-01T00:01:00".to_owned())?;
        let claims = json!({
            "iat": issued.as_secs(),
            "exp": (issued + JWT_BACKDATE + JWT_LIFETIME).as_secs(),
            "iss": self.client_id,
        });
        let signed = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(JWT_HEADER),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        );
        let mut signature = vec![0; self.key.public().modulus_len()];
        self.key
            .sign(
                &RSA_PKCS1_SHA256,
                &SystemRandom::new(),
                signed.as_bytes(),
                &mut signature,
            )
            .map_err(|_| "signing the app's JWT failed".to_owned())?;
        Ok(format!("{signed}.{}", URL_SAFE_NO_PAD.encode(signature)))
    }
}

/// The label and the base64 body (whitespace removed) of the first PEM
/// block in `text`.
fn pem_block(text: &str) -> Result<(&str, String), String> {
    const BEGIN: &str = "-----BEGIN ";
    const DASHES: &str = "-----";
    let start = text
        .find(BEGIN)
        .ok_or_else(|| "not PEM: no \"-----BEGIN\" line".to_owned())?;
    let rest = &text[start + BEGIN.len()..];
    let (label, rest) = rest
        .split_once(DASHES)
        .ok_or_else(|| "not PEM: the \"-----BEGIN\" line is not closed".to_owned())?;
    let end = format!("-----END {label}-----");
    let body = rest
        .find(&end)
        .map(|at| &rest[..at])
        .ok_or_else(|| format!("not PEM: no {end:?} line"))?;
    Ok((label, body.split_whitespace().collect()))
}

#[cfg(test)]
pub(crate) mod tests;
