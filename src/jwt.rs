// RS256 JSON Web Tokens and the JWKS document that lets clients verify them.

use crate::base64;
use crate::json::{self, J, V};
use crate::rsa::RsaKey;
use crate::sha256::sha256;

#[derive(Debug, PartialEq)]
pub enum JwtError {
    Malformed,
    UnsupportedAlg,
    BadSignature,
    Expired,
    NotYetValid,
    WrongIssuer,
}

impl std::fmt::Display for JwtError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let s = match self {
            JwtError::Malformed => "malformed token",
            JwtError::UnsupportedAlg => "unsupported alg",
            JwtError::BadSignature => "bad signature",
            JwtError::Expired => "token expired",
            JwtError::NotYetValid => "token not yet valid",
            JwtError::WrongIssuer => "wrong issuer",
        };
        f.write_str(s)
    }
}

/// RFC 7638 JWK thumbprint: base64url(SHA-256(canonical JWK)). Stable for a given
/// key, so restarting minicloak with the same PEM keeps the same `kid`.
pub fn kid(key: &RsaKey) -> String {
    let n = base64::encode_url(&key.n.to_bytes_be());
    let e = base64::encode_url(&key.e.to_bytes_be());
    // Canonical form: only the required members, lexicographic key order, no whitespace.
    let canonical = format!("{{\"e\":\"{}\",\"kty\":\"RSA\",\"n\":\"{}\"}}", e, n);
    base64::encode_url(&sha256(canonical.as_bytes()))
}

/// The public half, as a JWKS `keys` array with exactly one entry.
pub fn jwks(key: &RsaKey, kid: &str) -> String {
    let jwk = J::obj(vec![
        ("kty", J::s("RSA")),
        ("use", J::s("sig")),
        ("alg", J::s("RS256")),
        ("kid", J::s(kid)),
        ("n", J::S(base64::encode_url(&key.n.to_bytes_be()))),
        ("e", J::S(base64::encode_url(&key.e.to_bytes_be()))),
    ]);
    J::obj(vec![("keys", J::A(vec![jwk]))]).to_string()
}

/// Sign a claim set. `payload` must be a `J::O`.
pub fn encode(payload: J, key: &RsaKey, kid: &str) -> String {
    let header = J::obj(vec![
        ("alg", J::s("RS256")),
        ("typ", J::s("JWT")),
        ("kid", J::s(kid)),
    ]);
    let signing_input = format!(
        "{}.{}",
        base64::encode_url(header.to_string().as_bytes()),
        base64::encode_url(payload.to_string().as_bytes())
    );
    let sig = key.sign_sha256(signing_input.as_bytes());
    format!("{}.{}", signing_input, base64::encode_url(&sig))
}

/// Verify signature, `alg`, `iss`, `exp` and `nbf`, then return the claims.
///
/// `leeway` seconds of clock skew are tolerated on the time checks.
pub fn decode(
    token: &str,
    key: &RsaKey,
    issuer: &str,
    now: u64,
    leeway: u64,
) -> Result<V, JwtError> {
    let mut parts = token.split('.');
    let (h, p, s) = match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(h), Some(p), Some(s), None) => (h, p, s),
        _ => return Err(JwtError::Malformed),
    };

    let header_raw = base64::decode_url(h).ok_or(JwtError::Malformed)?;
    let header = json::parse(std::str::from_utf8(&header_raw).map_err(|_| JwtError::Malformed)?)
        .ok_or(JwtError::Malformed)?;
    // Reject `alg: none` and anything else we did not issue.
    if header.str_field("alg") != Some("RS256") {
        return Err(JwtError::UnsupportedAlg);
    }

    let sig = base64::decode_url(s).ok_or(JwtError::Malformed)?;
    let signing_input = format!("{}.{}", h, p);
    if !key.verify_sha256(signing_input.as_bytes(), &sig) {
        return Err(JwtError::BadSignature);
    }

    let payload_raw = base64::decode_url(p).ok_or(JwtError::Malformed)?;
    let claims = json::parse(std::str::from_utf8(&payload_raw).map_err(|_| JwtError::Malformed)?)
        .ok_or(JwtError::Malformed)?;

    if claims.str_field("iss") != Some(issuer) {
        return Err(JwtError::WrongIssuer);
    }
    match claims.u64_field("exp") {
        Some(exp) if now >= exp + leeway => return Err(JwtError::Expired),
        None => return Err(JwtError::Malformed),
        _ => {}
    }
    if let Some(nbf) = claims.u64_field("nbf") {
        if now + leeway < nbf {
            return Err(JwtError::NotYetValid);
        }
    }
    Ok(claims)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ISS: &str = "http://localhost:9500/realms/dev";

    fn key() -> RsaKey {
        RsaKey::generate(512)
    }

    fn claims(exp: u64) -> J {
        J::obj(vec![
            ("iss", J::s(ISS)),
            ("sub", J::s("alice")),
            ("exp", J::N(exp)),
        ])
    }

    #[test]
    fn encode_decode_roundtrip() {
        let k = key();
        let kid = kid(&k);
        let t = encode(claims(1000), &k, &kid);
        assert_eq!(t.matches('.').count(), 2);
        let c = decode(&t, &k, ISS, 999, 0).expect("valid");
        assert_eq!(c.str_field("sub"), Some("alice"));
    }

    #[test]
    fn header_advertises_the_kid_and_alg() {
        let k = key();
        let kid = kid(&k);
        let t = encode(claims(1000), &k, &kid);
        let h = t.split('.').next().unwrap();
        let header =
            json::parse(std::str::from_utf8(&base64::decode_url(h).unwrap()).unwrap()).unwrap();
        assert_eq!(header.str_field("alg"), Some("RS256"));
        assert_eq!(header.str_field("typ"), Some("JWT"));
        assert_eq!(header.str_field("kid"), Some(kid.as_str()));
    }

    #[test]
    fn rejects_a_tampered_payload() {
        let k = key();
        let kid = kid(&k);
        let t = encode(claims(1000), &k, &kid);
        let mut parts: Vec<&str> = t.split('.').collect();
        let forged = base64::encode_url(
            J::obj(vec![
                ("iss", J::s(ISS)),
                ("sub", J::s("root")),
                ("exp", J::N(1000)),
            ])
            .to_string()
            .as_bytes(),
        );
        parts[1] = &forged;
        assert_eq!(
            decode(&parts.join("."), &k, ISS, 999, 0),
            Err(JwtError::BadSignature)
        );
    }

    #[test]
    fn rejects_alg_none() {
        // The classic JWT downgrade: strip the signature, claim alg=none.
        let header = base64::encode_url(br#"{"alg":"none","typ":"JWT"}"#);
        let payload = base64::encode_url(claims(1000).to_string().as_bytes());
        let t = format!("{}.{}.", header, payload);
        assert_eq!(
            decode(&t, &key(), ISS, 999, 0),
            Err(JwtError::UnsupportedAlg)
        );
    }

    #[test]
    fn rejects_another_keys_signature() {
        let (k1, k2) = (key(), key());
        let t = encode(claims(1000), &k1, &kid(&k1));
        assert_eq!(decode(&t, &k2, ISS, 999, 0), Err(JwtError::BadSignature));
    }

    #[test]
    fn enforces_exp_iss_and_nbf() {
        let k = key();
        let kid = kid(&k);
        let t = encode(claims(1000), &k, &kid);
        assert_eq!(decode(&t, &k, ISS, 1000, 0), Err(JwtError::Expired));
        assert!(
            decode(&t, &k, ISS, 1000, 30).is_ok(),
            "leeway should save it"
        );
        assert_eq!(
            decode(&t, &k, "http://evil", 999, 0),
            Err(JwtError::WrongIssuer)
        );

        let future = J::obj(vec![
            ("iss", J::s(ISS)),
            ("exp", J::N(9999)),
            ("nbf", J::N(5000)),
        ]);
        let t2 = encode(future, &k, &kid);
        assert_eq!(decode(&t2, &k, ISS, 4000, 0), Err(JwtError::NotYetValid));
        assert!(decode(&t2, &k, ISS, 5000, 0).is_ok());
    }

    #[test]
    fn rejects_malformed_shapes() {
        let k = key();
        for bad in ["", "a.b", "a.b.c.d", "!!.!!.!!"] {
            assert!(matches!(
                decode(bad, &k, ISS, 0, 0),
                Err(JwtError::Malformed) | Err(JwtError::UnsupportedAlg)
            ));
        }
        // A token with no `exp` is not something we ever mint.
        let no_exp = encode(J::obj(vec![("iss", J::s(ISS))]), &k, "x");
        assert_eq!(decode(&no_exp, &k, ISS, 0, 0), Err(JwtError::Malformed));
    }

    #[test]
    fn kid_is_stable_and_key_dependent() {
        let k = key();
        assert_eq!(kid(&k), kid(&k));
        assert_ne!(kid(&k), kid(&key()));
        // base64url, 32 bytes -> 43 chars, no padding
        assert_eq!(kid(&k).len(), 43);
        assert!(!kid(&k).contains('='));
    }

    #[test]
    fn jwks_exposes_the_public_key() {
        let k = key();
        let kid = kid(&k);
        let doc = json::parse(&jwks(&k, &kid)).expect("valid json");
        let keys = match doc.get("keys") {
            Some(V::Arr(a)) => a,
            _ => panic!("no keys array"),
        };
        assert_eq!(keys.len(), 1);
        let jwk = &keys[0];
        assert_eq!(jwk.str_field("kty"), Some("RSA"));
        assert_eq!(jwk.str_field("alg"), Some("RS256"));
        assert_eq!(jwk.str_field("kid"), Some(kid.as_str()));
        assert_eq!(
            base64::decode_url(jwk.str_field("e").unwrap()).unwrap(),
            vec![0x01, 0x00, 0x01]
        );
        assert_eq!(
            base64::decode_url(jwk.str_field("n").unwrap()).unwrap(),
            k.n.to_bytes_be()
        );
        // The private half must never appear.
        assert!(jwk.get("d").is_none() && jwk.get("p").is_none());
    }
}
