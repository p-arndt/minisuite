// RSA key generation, PKCS#1 v1.5 signing (RS256), and PKCS#1 DER/PEM serialization.
// Pure std on top of `bigint`. Enough to be a real OIDC signing key, not a toy:
// the on-disk PEM is a standard RSAPrivateKey that `openssl rsa -check` accepts.

use crate::bigint::{self, Big};
use crate::sha256::sha256;

// Public exponent. 65537 is prime, which lets us test coprimality with p-1 by a
// single remainder instead of a gcd.
const E: u64 = 65537;

// DigestInfo prefix for SHA-256, RFC 8017 §9.2 notes.
const SHA256_DIGEST_INFO: [u8; 19] = [
    0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01, 0x05,
    0x00, 0x04, 0x20,
];

#[derive(Clone, Debug)]
pub struct RsaKey {
    pub n: Big,
    pub e: Big,
    d: Big,
    p: Big,
    q: Big,
    dp: Big,
    dq: Big,
    qinv: Big,
}

#[derive(Debug)]
pub enum KeyError {
    Pem(&'static str),
    Der(&'static str),
}

impl std::fmt::Display for KeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            KeyError::Pem(m) => write!(f, "bad PEM: {}", m),
            KeyError::Der(m) => write!(f, "bad DER: {}", m),
        }
    }
}

impl RsaKey {
    /// Modulus size in bytes — also the signature size.
    pub fn size(&self) -> usize {
        self.n.bit_len().div_ceil(8)
    }

    pub fn generate(bits: usize) -> RsaKey {
        assert!(
            bits >= 512 && bits.is_multiple_of(2),
            "key size must be even and >= 512"
        );
        let e = Big::from_u64(E);
        let half = bits / 2;
        loop {
            let p = gen_prime(half);
            let q = gen_prime(half);
            if p == q {
                continue;
            }
            let (p, q) = if p > q { (p, q) } else { (q, p) }; // qinv is mod p, so p > q
            let n = bigint::mul(&p, &q);
            if n.bit_len() != bits {
                continue;
            }
            let p1 = bigint::sub(&p, &Big::one());
            let q1 = bigint::sub(&q, &Big::one());
            let phi = bigint::mul(&p1, &q1);
            let d = match bigint::modinv(&e, &phi) {
                Some(d) => d,
                None => continue,
            };
            return RsaKey {
                dp: bigint::rem(&d, &p1),
                dq: bigint::rem(&d, &q1),
                qinv: bigint::modinv(&q, &p).expect("p, q distinct primes"),
                n,
                e,
                d,
                p,
                q,
            };
        }
    }

    /// RS256: sign SHA-256(msg) with PKCS#1 v1.5 padding. Returns exactly `size()` bytes.
    pub fn sign_sha256(&self, msg: &[u8]) -> Vec<u8> {
        let em = pkcs1v15_pad(&sha256(msg), self.size());
        let m = Big::from_bytes_be(&em);

        // CRT: ~4x faster than m^d mod n, and we already store the factors.
        let m1 = bigint::modpow(&m, &self.dp, &self.p);
        let m2 = bigint::modpow(&m, &self.dq, &self.q);
        let m2p = bigint::rem(&m2, &self.p);
        let h = bigint::mulmod(&self.qinv, &bigint::submod(&m1, &m2p, &self.p), &self.p);
        let s = bigint::add(&m2, &bigint::mul(&self.q, &h));

        s.to_bytes_be_padded(self.size())
    }

    pub fn verify_sha256(&self, msg: &[u8], sig: &[u8]) -> bool {
        if sig.len() != self.size() {
            return false;
        }
        let s = Big::from_bytes_be(sig);
        if s >= self.n {
            return false;
        }
        let m = bigint::modpow(&s, &self.e, &self.n);
        m.to_bytes_be_padded(self.size()) == pkcs1v15_pad(&sha256(msg), self.size())
    }

    // --- serialization ---

    pub fn to_pkcs1_der(&self) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&der_int(&Big::zero())); // version
        for v in [
            &self.n, &self.e, &self.d, &self.p, &self.q, &self.dp, &self.dq, &self.qinv,
        ] {
            body.extend_from_slice(&der_int(v));
        }
        der_tlv(0x30, &body)
    }

    pub fn to_pkcs1_pem(&self) -> String {
        pem_wrap("RSA PRIVATE KEY", &self.to_pkcs1_der())
    }

    pub fn from_pkcs1_der(der: &[u8]) -> Result<RsaKey, KeyError> {
        let (tag, body) = der_read(der, 0)?;
        if tag != 0x30 {
            return Err(KeyError::Der("expected SEQUENCE"));
        }
        let mut pos = 0;
        let mut ints = Vec::with_capacity(9);
        while pos < body.len() && ints.len() < 9 {
            let (tag, content) = der_read(body, pos)?;
            if tag != 0x02 {
                return Err(KeyError::Der("expected INTEGER"));
            }
            pos += der_tlv_len(body, pos)?;
            ints.push(Big::from_bytes_be(content));
        }
        if ints.len() != 9 {
            return Err(KeyError::Der("expected 9 integers"));
        }
        if !ints[0].is_zero() {
            return Err(KeyError::Der("unsupported RSAPrivateKey version"));
        }
        let k = RsaKey {
            n: ints[1].clone(),
            e: ints[2].clone(),
            d: ints[3].clone(),
            p: ints[4].clone(),
            q: ints[5].clone(),
            dp: ints[6].clone(),
            dq: ints[7].clone(),
            qinv: ints[8].clone(),
        };
        if k.n.is_zero() || k.p.is_zero() || k.q.is_zero() {
            return Err(KeyError::Der("zero modulus or factor"));
        }
        if bigint::mul(&k.p, &k.q) != k.n {
            return Err(KeyError::Der("p*q != n"));
        }
        Ok(k)
    }

    pub fn from_pkcs1_pem(pem: &str) -> Result<RsaKey, KeyError> {
        let der = pem_unwrap(pem, "RSA PRIVATE KEY")?;
        RsaKey::from_pkcs1_der(&der)
    }
}

// EM = 0x00 || 0x01 || 0xFF...0xFF || 0x00 || DigestInfo || H   (RFC 8017 §9.2)
fn pkcs1v15_pad(hash: &[u8; 32], k: usize) -> Vec<u8> {
    let t_len = SHA256_DIGEST_INFO.len() + hash.len();
    assert!(k >= t_len + 11, "modulus too small for a SHA-256 signature");
    let mut em = Vec::with_capacity(k);
    em.push(0x00);
    em.push(0x01);
    em.resize(k - t_len - 1, 0xff);
    em.push(0x00);
    em.extend_from_slice(&SHA256_DIGEST_INFO);
    em.extend_from_slice(hash);
    debug_assert_eq!(em.len(), k);
    em
}

// --- prime generation ---

fn small_primes(limit: usize) -> Vec<u64> {
    let mut sieve = vec![true; limit + 1];
    let mut out = Vec::new();
    for i in 2..=limit {
        if sieve[i] {
            out.push(i as u64);
            let mut j = i * i;
            while j <= limit {
                sieve[j] = false;
                j += i;
            }
        }
    }
    out
}

/// A random `bits`-bit prime p with p ≡ 3 (mod 4) not required, but with the top two
/// bits set so that p*q always lands on exactly 2*bits bits, and with gcd(p-1, e) = 1.
fn gen_prime(bits: usize) -> Big {
    let smalls = small_primes(8192);
    loop {
        let mut p = bigint::random_bits(bits);
        p.set_bit(bits - 1);
        p.set_bit(bits - 2);
        p.set_bit(0);

        // Trial division knocks out ~92% of candidates for a fraction of the cost
        // of a single Miller-Rabin round.
        if smalls.iter().any(|&s| bigint::rem_u64(&p, s) == 0) {
            continue;
        }
        // e must be invertible mod p-1.
        if bigint::rem_u64(&bigint::sub(&p, &Big::one()), E) == 0 {
            continue;
        }
        if is_probable_prime(&p, 24) {
            return p;
        }
    }
}

/// Miller-Rabin with `rounds` random bases. Error probability below 4^-rounds.
pub fn is_probable_prime(n: &Big, rounds: usize) -> bool {
    if n.bit_len() < 2 {
        return false; // 0, 1
    }
    if n.is_even() {
        return n == &Big::from_u64(2);
    }
    for &s in &[3u64, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37] {
        if n == &Big::from_u64(s) {
            return true;
        }
        if bigint::rem_u64(n, s) == 0 {
            return false;
        }
    }

    let one = Big::one();
    let n1 = bigint::sub(n, &one); // n - 1
    let n3 = bigint::sub(n, &Big::from_u64(3)); // n - 3, for base selection

    // n-1 = d * 2^s with d odd
    let mut s = 0usize;
    while !n1.bit(s) {
        s += 1;
    }
    let d = bigint::shr(&n1, s);

    'outer: for _ in 0..rounds {
        // base in [2, n-2]
        let a = bigint::add(
            &bigint::rem(&bigint::random_bits(n.bit_len()), &n3),
            &Big::from_u64(2),
        );
        let mut x = bigint::modpow(&a, &d, n);
        if x.is_one() || x == n1 {
            continue;
        }
        for _ in 0..s - 1 {
            x = bigint::mulmod(&x, &x, n);
            if x == n1 {
                continue 'outer;
            }
        }
        return false;
    }
    true
}

// --- minimal DER ---

fn der_len(len: usize) -> Vec<u8> {
    if len < 0x80 {
        vec![len as u8]
    } else if len <= 0xff {
        vec![0x81, len as u8]
    } else if len <= 0xffff {
        vec![0x82, (len >> 8) as u8, len as u8]
    } else {
        vec![0x83, (len >> 16) as u8, (len >> 8) as u8, len as u8]
    }
}

fn der_tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    out.extend_from_slice(&der_len(content.len()));
    out.extend_from_slice(content);
    out
}

fn der_int(v: &Big) -> Vec<u8> {
    let mut c = v.to_bytes_be();
    if c.is_empty() {
        c.push(0);
    } else if c[0] & 0x80 != 0 {
        c.insert(0, 0); // keep it positive
    }
    der_tlv(0x02, &c)
}

// Returns (tag, content). `pos` points at the tag byte.
fn der_read(buf: &[u8], pos: usize) -> Result<(u8, &[u8]), KeyError> {
    let (start, len) = der_content_span(buf, pos)?;
    Ok((buf[pos], &buf[start..start + len]))
}

// Total bytes consumed by the TLV starting at `pos`.
fn der_tlv_len(buf: &[u8], pos: usize) -> Result<usize, KeyError> {
    let (start, len) = der_content_span(buf, pos)?;
    Ok(start - pos + len)
}

fn der_content_span(buf: &[u8], pos: usize) -> Result<(usize, usize), KeyError> {
    if pos + 1 >= buf.len() {
        return Err(KeyError::Der("truncated"));
    }
    let l0 = buf[pos + 1];
    let (hdr, len) = if l0 < 0x80 {
        (2usize, l0 as usize)
    } else {
        let nbytes = (l0 & 0x7f) as usize;
        if nbytes == 0 || nbytes > 4 || pos + 2 + nbytes > buf.len() {
            return Err(KeyError::Der("bad length"));
        }
        let mut len = 0usize;
        for i in 0..nbytes {
            len = (len << 8) | buf[pos + 2 + i] as usize;
        }
        (2 + nbytes, len)
    };
    let start = pos + hdr;
    if start + len > buf.len() {
        return Err(KeyError::Der("length overruns buffer"));
    }
    Ok((start, len))
}

// --- PEM ---

fn pem_wrap(label: &str, der: &[u8]) -> String {
    let b64 = crate::base64::encode(der);
    let mut out = format!("-----BEGIN {}-----\n", label);
    for chunk in b64.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(chunk).unwrap());
        out.push('\n');
    }
    out.push_str(&format!("-----END {}-----\n", label));
    out
}

fn pem_unwrap(pem: &str, label: &str) -> Result<Vec<u8>, KeyError> {
    let begin = format!("-----BEGIN {}-----", label);
    let end = format!("-----END {}-----", label);
    let start = pem
        .find(&begin)
        .ok_or(KeyError::Pem("missing BEGIN line"))?
        + begin.len();
    let stop = pem.find(&end).ok_or(KeyError::Pem("missing END line"))?;
    if stop < start {
        return Err(KeyError::Pem("END before BEGIN"));
    }
    crate::base64::decode(&pem[start..stop]).ok_or(KeyError::Pem("invalid base64 body"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // 512-bit keys keep the debug-build test suite fast; the maths is size-agnostic.
    fn key() -> RsaKey {
        RsaKey::generate(512)
    }

    #[test]
    fn generated_key_is_consistent() {
        let k = key();
        assert_eq!(k.n.bit_len(), 512);
        assert_eq!(k.size(), 64);
        assert_eq!(bigint::mul(&k.p, &k.q), k.n);
        // d is the inverse of e mod phi
        let p1 = bigint::sub(&k.p, &Big::one());
        let q1 = bigint::sub(&k.q, &Big::one());
        let phi = bigint::mul(&p1, &q1);
        assert!(bigint::mulmod(&k.e, &k.d, &phi).is_one());
        assert!(k.p > k.q, "qinv is computed mod p");
    }

    #[test]
    fn sign_then_verify() {
        let k = key();
        let sig = k.sign_sha256(b"hello minicloak");
        assert_eq!(sig.len(), k.size());
        assert!(k.verify_sha256(b"hello minicloak", &sig));
        assert!(!k.verify_sha256(b"hello minicloa", &sig));
    }

    #[test]
    fn signature_is_deterministic() {
        // PKCS#1 v1.5 has no salt: the same message must always give the same bytes.
        let k = key();
        assert_eq!(k.sign_sha256(b"abc"), k.sign_sha256(b"abc"));
    }

    #[test]
    fn tampered_signature_is_rejected() {
        let k = key();
        let mut sig = k.sign_sha256(b"abc");
        sig[10] ^= 0x01;
        assert!(!k.verify_sha256(b"abc", &sig));
        assert!(!k.verify_sha256(b"abc", &sig[..sig.len() - 1]));
    }

    #[test]
    fn crt_signature_matches_the_plain_modpow() {
        let k = key();
        let em = pkcs1v15_pad(&sha256(b"crt check"), k.size());
        let want = bigint::modpow(&Big::from_bytes_be(&em), &k.d, &k.n);
        assert_eq!(
            k.sign_sha256(b"crt check"),
            want.to_bytes_be_padded(k.size())
        );
    }

    #[test]
    fn pkcs1_padding_shape() {
        let em = pkcs1v15_pad(&sha256(b""), 64);
        assert_eq!(em.len(), 64);
        assert_eq!(&em[..2], &[0x00, 0x01]);
        // The separator is the first 0x00 *after* the leading 0x00 0x01.
        let zero = 2 + em[2..].iter().position(|&b| b == 0x00).unwrap();
        assert!(em[2..zero].iter().all(|&b| b == 0xff));
        assert!(
            zero - 2 >= 8,
            "RFC 8017 requires at least 8 bytes of 0xff padding"
        );
        assert_eq!(&em[zero + 1..zero + 20], &SHA256_DIGEST_INFO);
        assert_eq!(&em[zero + 20..], &sha256(b"")[..]);
    }

    #[test]
    fn pem_roundtrip() {
        let k = key();
        let pem = k.to_pkcs1_pem();
        assert!(pem.starts_with("-----BEGIN RSA PRIVATE KEY-----\n"));
        assert!(pem.trim_end().ends_with("-----END RSA PRIVATE KEY-----"));
        let k2 = RsaKey::from_pkcs1_pem(&pem).expect("reparse");
        assert_eq!(k2.n, k.n);
        assert_eq!(k2.d, k.d);
        assert_eq!(k2.qinv, k.qinv);
        // A key reloaded from disk must produce byte-identical signatures.
        assert_eq!(k2.sign_sha256(b"x"), k.sign_sha256(b"x"));
    }

    #[test]
    fn pem_rejects_garbage() {
        assert!(RsaKey::from_pkcs1_pem("not a pem").is_err());
        assert!(RsaKey::from_pkcs1_pem(&pem_wrap("RSA PRIVATE KEY", &[0x30, 0x01, 0x00])).is_err());
        // A well-formed SEQUENCE whose p*q != n must be caught.
        let mut body = Vec::new();
        for v in [0u64, 35, 65537, 5, 5, 7, 1, 1, 1] {
            body.extend_from_slice(&der_int(&Big::from_u64(v)));
        }
        let der = der_tlv(0x30, &body); // 5*7 == 35, so this one is *valid* shape-wise
        assert!(RsaKey::from_pkcs1_der(&der).is_ok());
        body.clear();
        for v in [0u64, 36, 65537, 5, 5, 7, 1, 1, 1] {
            body.extend_from_slice(&der_int(&Big::from_u64(v)));
        }
        assert!(RsaKey::from_pkcs1_der(&der_tlv(0x30, &body)).is_err());
    }

    #[test]
    fn der_int_keeps_integers_positive() {
        // 0x80 has the high bit set, so DER must prepend a zero byte.
        assert_eq!(der_int(&Big::from_u64(0x80)), vec![0x02, 0x02, 0x00, 0x80]);
        assert_eq!(der_int(&Big::from_u64(0x7f)), vec![0x02, 0x01, 0x7f]);
        assert_eq!(der_int(&Big::zero()), vec![0x02, 0x01, 0x00]);
    }

    #[test]
    fn miller_rabin_agrees_with_trial_division() {
        for n in 2u64..2000 {
            let want = (2..n).take_while(|i| i * i <= n).all(|i| n % i != 0);
            assert_eq!(is_probable_prime(&Big::from_u64(n), 8), want, "n = {}", n);
        }
        assert!(!is_probable_prime(&Big::from_u64(0), 8));
        assert!(!is_probable_prime(&Big::from_u64(1), 8));
        // Carmichael numbers fool Fermat but not Miller-Rabin.
        assert!(!is_probable_prime(&Big::from_u64(561), 8));
        assert!(!is_probable_prime(&Big::from_u64(41041), 8));
    }
}
