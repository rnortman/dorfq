use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ring::signature::{RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};
use serde_json::{Value, json};

use super::*;

pub(crate) const PKCS1: &str = include_str!("../../testdata/app-key.pkcs1.pem");
const PKCS8: &str = include_str!("../../testdata/app-key.pkcs8.pem");

/// A PEM block's opening line.
fn begin(label: &str) -> String {
    format!("-----BEGIN {label}-----\n")
}

/// A PEM block with `body` between its armour lines.
fn armour(label: &str, body: &str) -> String {
    format!("{}{body}\n-----END {label}-----\n", begin(label))
}

fn error(pem: &str) -> String {
    match AppKey::from_pem("Iv1.test", pem) {
        Ok(_) => panic!("accepted {pem:?}"),
        Err(e) => e,
    }
}

/// Decodes a JWT's header and claims and checks its signature against
/// `key`; panics when any of it fails.
pub(crate) fn verified(key: &AppKey, jwt: &str) -> (Value, Value) {
    let segments: Vec<&str> = jwt.split('.').collect();
    assert_eq!(segments.len(), 3, "{jwt}");
    let decode = |segment: &str| URL_SAFE_NO_PAD.decode(segment).expect(segment);
    let signed = format!("{}.{}", segments[0], segments[1]);
    UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, key.key.public().as_ref())
        .verify(signed.as_bytes(), &decode(segments[2]))
        .expect("the signature verifies");
    let json = |segment: &str| serde_json::from_slice(&decode(segment)).expect(segment);
    (json(segments[0]), json(segments[1]))
}

#[test]
fn fixtures_are_one_key_in_both_forms() {
    assert!(PKCS1.starts_with(&begin("RSA PRIVATE KEY")));
    assert!(PKCS8.starts_with(&begin("PRIVATE KEY")));
    let one = AppKey::from_pem("Iv1.test", PKCS1).unwrap();
    let eight = AppKey::from_pem("Iv1.test", PKCS8).unwrap();
    assert_eq!(one.key.public().as_ref(), eight.key.public().as_ref());
}

#[test]
fn text_without_armour_is_not_pem() {
    let text = error("MIIEowIBAAKCAQEA");
    assert!(text.contains("not PEM"), "{text}");
    let text = error(&format!("{}MIIE\n", begin("RSA PRIVATE KEY")));
    assert!(text.contains("not PEM"), "{text}");
    assert!(text.contains("-----END RSA PRIVATE KEY-----"), "{text}");
}

#[test]
fn another_label_is_named() {
    let text = error(&armour("CERTIFICATE", "MIIB"));
    assert!(
        text.contains("unexpected PEM label \"CERTIFICATE\""),
        "{text}"
    );
}

#[test]
fn a_body_that_is_not_base64_is_named() {
    let text = error(&armour("RSA PRIVATE KEY", "!!!!"));
    assert!(text.contains("not base64"), "{text}");
}

#[test]
fn a_passphrase_protected_key_is_named() {
    let text = error(&armour(
        "RSA PRIVATE KEY",
        "Proc-Type: 4,ENCRYPTED\nDEK-Info: AES-128-CBC,00112233445566778899AABBCCDDEEFF\n\nMIIB",
    ));
    assert!(text.contains("passphrase-protected"), "{text}");
}

#[test]
fn base64_of_junk_is_rejected_with_rings_reason() {
    let junk = STANDARD.encode(b"this is not a DER-encoded key");
    for label in ["RSA PRIVATE KEY", "PRIVATE KEY"] {
        let text = error(&armour(label, &junk));
        assert!(text.contains("not a usable RSA private key"), "{text}");
    }
}

#[test]
fn jwt_is_signed_rs256_with_github_claims() {
    let key = AppKey::from_pem("Iv1.test", PKCS1).unwrap();
    let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    let (header, claims) = verified(&key, &key.jwt(now).unwrap());
    assert_eq!(header, json!({ "alg": "RS256", "typ": "JWT" }));
    assert_eq!(
        claims,
        json!({ "iat": 1_800_000_000 - 60, "exp": 1_800_000_000 + 300, "iss": "Iv1.test" })
    );
}

#[test]
fn jwt_before_the_epoch_is_refused() {
    let key = AppKey::from_pem("Iv1.test", PKCS8).unwrap();
    let error = key.jwt(UNIX_EPOCH - Duration::from_secs(1)).unwrap_err();
    assert!(error.contains("1970"), "{error}");
    assert!(key.jwt(SystemTime::now()).is_ok());
}

#[test]
fn jwt_in_the_first_minute_after_the_epoch_is_refused() {
    let key = AppKey::from_pem("Iv1.test", PKCS1).unwrap();
    let error = key.jwt(UNIX_EPOCH + Duration::from_secs(30)).unwrap_err();
    assert!(error.contains("1970-01-01T00:01:00"), "{error}");
    let (_, claims) = verified(&key, &key.jwt(UNIX_EPOCH + JWT_BACKDATE).unwrap());
    assert_eq!(claims["iat"], 0);
}
