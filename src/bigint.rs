// Arbitrary-precision unsigned integers. Little-endian u64 limbs, u128 intermediates.
// Just enough arithmetic to generate an RSA key and sign with it. Pure std, no deps.
//
// Division is Knuth's Algorithm D (TAOCP 4.3.1), transcribed from Hacker's Delight
// `divmnu64`. The obvious shift-and-subtract alternative is O(bits) per divmod, which
// would make a 2048-bit modpow take ~500M limb ops instead of ~4M.

use std::cmp::Ordering;

const B: u128 = 1u128 << 64;
const LO: u128 = 0xFFFF_FFFF_FFFF_FFFF;

// Invariant: no trailing zero limbs. Zero is the empty vec.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Big {
    limbs: Vec<u64>,
}

impl Big {
    pub fn zero() -> Big {
        Big { limbs: Vec::new() }
    }
    pub fn one() -> Big {
        Big { limbs: vec![1] }
    }
    pub fn from_u64(v: u64) -> Big {
        if v == 0 {
            Big::zero()
        } else {
            Big { limbs: vec![v] }
        }
    }
    #[cfg(test)]
    pub fn from_limbs(limbs: Vec<u64>) -> Big {
        Big { limbs }.trimmed()
    }

    fn trimmed(mut self) -> Big {
        while self.limbs.last() == Some(&0) {
            self.limbs.pop();
        }
        self
    }

    pub fn is_zero(&self) -> bool {
        self.limbs.is_empty()
    }
    pub fn is_one(&self) -> bool {
        self.limbs.len() == 1 && self.limbs[0] == 1
    }
    pub fn is_even(&self) -> bool {
        self.limbs.first().is_none_or(|l| l & 1 == 0)
    }
    pub fn bit_len(&self) -> usize {
        match self.limbs.last() {
            None => 0,
            Some(&t) => self.limbs.len() * 64 - t.leading_zeros() as usize,
        }
    }

    pub fn bit(&self, i: usize) -> bool {
        let l = i / 64;
        l < self.limbs.len() && (self.limbs[l] >> (i % 64)) & 1 == 1
    }

    pub fn set_bit(&mut self, i: usize) {
        let l = i / 64;
        if l >= self.limbs.len() {
            self.limbs.resize(l + 1, 0);
        }
        self.limbs[l] |= 1 << (i % 64);
    }

    pub fn from_bytes_be(b: &[u8]) -> Big {
        let mut limbs = Vec::with_capacity(b.len() / 8 + 1);
        let mut chunk = [0u8; 8];
        let mut i = b.len();
        while i >= 8 {
            chunk.copy_from_slice(&b[i - 8..i]);
            limbs.push(u64::from_be_bytes(chunk));
            i -= 8;
        }
        if i > 0 {
            let mut last = [0u8; 8];
            last[8 - i..].copy_from_slice(&b[..i]);
            limbs.push(u64::from_be_bytes(last));
        }
        Big { limbs }.trimmed()
    }

    // Minimal big-endian encoding. Zero encodes as an empty slice.
    pub fn to_bytes_be(&self) -> Vec<u8> {
        if self.is_zero() {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(self.limbs.len() * 8);
        for (i, limb) in self.limbs.iter().enumerate().rev() {
            let bytes = limb.to_be_bytes();
            if i == self.limbs.len() - 1 {
                let skip = limb.leading_zeros() as usize / 8;
                out.extend_from_slice(&bytes[skip..]);
            } else {
                out.extend_from_slice(&bytes);
            }
        }
        out
    }

    // Left-zero-padded to exactly `len` bytes. Panics if the value does not fit.
    pub fn to_bytes_be_padded(&self, len: usize) -> Vec<u8> {
        let raw = self.to_bytes_be();
        assert!(raw.len() <= len, "integer too large for {} bytes", len);
        let mut out = vec![0u8; len - raw.len()];
        out.extend_from_slice(&raw);
        out
    }
}

impl Ord for Big {
    fn cmp(&self, other: &Big) -> Ordering {
        if self.limbs.len() != other.limbs.len() {
            return self.limbs.len().cmp(&other.limbs.len());
        }
        for i in (0..self.limbs.len()).rev() {
            match self.limbs[i].cmp(&other.limbs[i]) {
                Ordering::Equal => {}
                o => return o,
            }
        }
        Ordering::Equal
    }
}
impl PartialOrd for Big {
    fn partial_cmp(&self, other: &Big) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

pub fn add(a: &Big, b: &Big) -> Big {
    let n = a.limbs.len().max(b.limbs.len());
    let mut out = Vec::with_capacity(n + 1);
    let mut carry: u128 = 0;
    for i in 0..n {
        let x = *a.limbs.get(i).unwrap_or(&0) as u128;
        let y = *b.limbs.get(i).unwrap_or(&0) as u128;
        let s = x + y + carry;
        out.push(s as u64);
        carry = s >> 64;
    }
    if carry > 0 {
        out.push(carry as u64);
    }
    Big { limbs: out }.trimmed()
}

// Requires a >= b.
pub fn sub(a: &Big, b: &Big) -> Big {
    debug_assert!(a >= b, "sub underflow");
    let mut out = Vec::with_capacity(a.limbs.len());
    let mut borrow: i128 = 0;
    for i in 0..a.limbs.len() {
        let x = a.limbs[i] as i128;
        let y = *b.limbs.get(i).unwrap_or(&0) as i128;
        let d = x - y - borrow;
        if d < 0 {
            out.push((d + B as i128) as u64);
            borrow = 1;
        } else {
            out.push(d as u64);
            borrow = 0;
        }
    }
    debug_assert_eq!(borrow, 0);
    Big { limbs: out }.trimmed()
}

pub fn mul(a: &Big, b: &Big) -> Big {
    if a.is_zero() || b.is_zero() {
        return Big::zero();
    }
    let (la, lb) = (a.limbs.len(), b.limbs.len());
    let mut r = vec![0u64; la + lb];
    for i in 0..la {
        let mut carry: u128 = 0;
        let ai = a.limbs[i] as u128;
        for j in 0..lb {
            // Fits exactly: (2^64-1) + (2^64-1)^2 + (2^64-1) == 2^128 - 1.
            let cur = r[i + j] as u128 + ai * (b.limbs[j] as u128) + carry;
            r[i + j] = cur as u64;
            carry = cur >> 64;
        }
        r[i + lb] = carry as u64;
    }
    Big { limbs: r }.trimmed()
}

// Shift left by `bits` (any amount).
pub fn shl(a: &Big, bits: usize) -> Big {
    if a.is_zero() {
        return Big::zero();
    }
    let (whole, part) = (bits / 64, bits % 64);
    let mut out = vec![0u64; whole];
    if part == 0 {
        out.extend_from_slice(&a.limbs);
    } else {
        let mut carry = 0u64;
        for &l in &a.limbs {
            out.push((l << part) | carry);
            carry = l >> (64 - part);
        }
        if carry > 0 {
            out.push(carry);
        }
    }
    Big { limbs: out }.trimmed()
}

// Shift right by `bits` (any amount).
pub fn shr(a: &Big, bits: usize) -> Big {
    let (whole, part) = (bits / 64, bits % 64);
    if whole >= a.limbs.len() {
        return Big::zero();
    }
    let src = &a.limbs[whole..];
    let mut out = Vec::with_capacity(src.len());
    if part == 0 {
        out.extend_from_slice(src);
    } else {
        for i in 0..src.len() {
            let hi = if i + 1 < src.len() {
                src[i + 1] << (64 - part)
            } else {
                0
            };
            out.push((src[i] >> part) | hi);
        }
    }
    Big { limbs: out }.trimmed()
}

fn divmod_small(a: &Big, d: u64) -> (Big, u64) {
    let mut q = vec![0u64; a.limbs.len()];
    let mut rem: u128 = 0;
    for i in (0..a.limbs.len()).rev() {
        let cur = (rem << 64) | a.limbs[i] as u128;
        q[i] = (cur / d as u128) as u64;
        rem = cur % d as u128;
    }
    (Big { limbs: q }.trimmed(), rem as u64)
}

/// Returns (quotient, remainder). Panics if `b` is zero.
pub fn divmod(a: &Big, b: &Big) -> (Big, Big) {
    assert!(!b.is_zero(), "division by zero");
    if a < b {
        return (Big::zero(), a.clone());
    }
    if b.limbs.len() == 1 {
        let (q, r) = divmod_small(a, b.limbs[0]);
        return (q, Big::from_u64(r));
    }

    // Normalize so the divisor's top limb has its high bit set; this bounds the
    // quotient-digit estimate error to at most 2.
    let shift = b.limbs.last().unwrap().leading_zeros() as usize;
    let v = shl(b, shift).limbs;
    let n = v.len();
    let la = a.limbs.len();
    let mut u = shl(a, shift).limbs;
    u.resize(la + 1, 0); // shl grows by at most one limb, so this only ever pads
    let m = la - n;

    let mut q = vec![0u64; m + 1];
    for j in (0..=m).rev() {
        let num = ((u[j + n] as u128) << 64) | (u[j + n - 1] as u128);
        let d1 = v[n - 1] as u128;
        let mut qhat = num / d1;
        let mut rhat = num - qhat * d1;
        loop {
            // Short-circuit matters: when qhat >= B the product below would overflow.
            if qhat >= B || qhat * (v[n - 2] as u128) > (rhat << 64) + (u[j + n - 2] as u128) {
                qhat -= 1;
                rhat += d1;
                if rhat < B {
                    continue;
                }
            }
            break;
        }

        // u[j..j+n] -= qhat * v
        let mut k: i128 = 0;
        for i in 0..n {
            let p = qhat * (v[i] as u128);
            let t = (u[i + j] as i128) - k - ((p & LO) as i128);
            u[i + j] = t as u64;
            k = ((p >> 64) as i128) - (t >> 64); // arithmetic shift: -1 on borrow
        }
        let t = (u[j + n] as i128) - k;
        u[j + n] = t as u64;
        q[j] = qhat as u64;

        if t < 0 {
            // qhat was one too large (happens with probability ~2/B). Add v back.
            q[j] -= 1;
            let mut c: u128 = 0;
            for i in 0..n {
                let s = (u[i + j] as u128) + (v[i] as u128) + c;
                u[i + j] = s as u64;
                c = s >> 64;
            }
            u[j + n] = (u[j + n] as u128 + c) as u64;
        }
    }

    let rem = shr(
        &Big {
            limbs: u[..n].to_vec(),
        }
        .trimmed(),
        shift,
    );
    (Big { limbs: q }.trimmed(), rem)
}

pub fn rem(a: &Big, m: &Big) -> Big {
    divmod(a, m).1
}

/// Remainder by a single limb. Used for trial division during prime search,
/// where going through the full `divmod` for every small prime would dominate.
pub fn rem_u64(a: &Big, d: u64) -> u64 {
    assert!(d != 0, "division by zero");
    let mut r: u128 = 0;
    for i in (0..a.limbs.len()).rev() {
        r = ((r << 64) | a.limbs[i] as u128) % d as u128;
    }
    r as u64
}

pub fn mulmod(a: &Big, b: &Big, m: &Big) -> Big {
    rem(&mul(a, b), m)
}

// Requires a < m and b < m.
pub fn submod(a: &Big, b: &Big, m: &Big) -> Big {
    if a >= b {
        sub(a, b)
    } else {
        sub(m, &sub(b, a))
    }
}

pub fn modpow(base: &Big, exp: &Big, m: &Big) -> Big {
    if m.is_one() {
        return Big::zero();
    }
    let base = rem(base, m);
    let mut r = Big::one();
    for i in (0..exp.bit_len()).rev() {
        r = mulmod(&r, &r, m);
        if exp.bit(i) {
            r = mulmod(&r, &base, m);
        }
    }
    r
}

/// Modular inverse of `a` mod `m`, or None when gcd(a, m) != 1.
///
/// Extended Euclid, but the Bezout coefficient is kept reduced mod `m` throughout
/// so every value stays non-negative and we never need signed bignums.
pub fn modinv(a: &Big, m: &Big) -> Option<Big> {
    if m.is_zero() || m.is_one() {
        return None;
    }
    let mut old_r = rem(a, m);
    let mut r = m.clone();
    let mut old_s = Big::one();
    let mut s = Big::zero();

    while !r.is_zero() {
        let (q, rr) = divmod(&old_r, &r);
        old_r = std::mem::replace(&mut r, rr);
        let qs = mulmod(&q, &s, m);
        let sn = submod(&old_s, &qs, m);
        old_s = std::mem::replace(&mut s, sn);
    }
    if old_r.is_one() {
        Some(old_s)
    } else {
        None
    }
}

/// A uniformly random `bits`-bit integer with the top bit forced set.
pub fn random_bits(bits: usize) -> Big {
    assert!(bits > 0);
    let nbytes = bits.div_ceil(8);
    let mut buf = vec![0u8; nbytes];
    crate::rand::fill(&mut buf);
    // Mask off the bits above `bits` in the leading byte.
    let excess = nbytes * 8 - bits;
    if excess > 0 {
        buf[0] &= 0xFFu8 >> excess;
    }
    let mut n = Big::from_bytes_be(&buf);
    n.set_bit(bits - 1);
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(v: u64) -> Big {
        Big::from_u64(v)
    }
    fn hex(n: &Big) -> String {
        crate::sha256::hex(&n.to_bytes_be())
    }
    fn parse(h: &str) -> Big {
        let h = if h.len() % 2 == 1 {
            format!("0{}", h)
        } else {
            h.to_string()
        };
        let bytes: Vec<u8> = (0..h.len() / 2)
            .map(|i| u8::from_str_radix(&h[i * 2..i * 2 + 2], 16).unwrap())
            .collect();
        Big::from_bytes_be(&bytes)
    }

    #[test]
    fn add_sub_roundtrip() {
        let x = parse("ffffffffffffffffffffffffffffffff");
        let y = parse("1");
        let s = add(&x, &y);
        assert_eq!(hex(&s), "0100000000000000000000000000000000");
        assert_eq!(sub(&s, &y), x);
    }

    #[test]
    fn add_carries_across_limbs() {
        let x = parse("ffffffffffffffff"); // 2^64 - 1
        assert_eq!(hex(&add(&x, &b(1))), "010000000000000000");
    }

    #[test]
    fn mul_matches_u128() {
        let a = 0xdead_beef_cafe_babeu64;
        let c = 0x0123_4567_89ab_cdefu64;
        let want = (a as u128) * (c as u128);
        let got = mul(&b(a), &b(c));
        assert_eq!(got, Big::from_bytes_be(&want.to_be_bytes()));
    }

    #[test]
    fn divmod_small_and_large() {
        let a = parse("100000000000000000000000000000000"); // 2^128
        let d = parse("ffffffffffffffff"); // 2^64 - 1
        let (q, r) = divmod(&a, &d);
        // 2^128 = (2^64-1)*(2^64+1) + 1
        assert_eq!(hex(&q), "010000000000000001");
        assert_eq!(r, Big::one());
    }

    #[test]
    fn divmod_exercises_add_back_path() {
        // Chosen to force the qhat-too-large correction: divisor top limb barely
        // normalized, dividend leading limbs equal to it.
        let a = parse("7fffffffffffffff0000000000000000ffffffffffffffff");
        let d = parse("7fffffffffffffff0000000000000001");
        let (q, r) = divmod(&a, &d);
        assert_eq!(add(&mul(&q, &d), &r), a);
        assert!(r < d);
    }

    #[test]
    fn divmod_random_identity() {
        // a == q*d + r, with r < d, over a spread of sizes.
        let mut seed = 0x243f_6a88_85a3_08d3u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..200 {
            let al = 1 + (next() % 8) as usize;
            let dl = 1 + (next() % 8) as usize;
            let a = Big::from_limbs((0..al).map(|_| next()).collect());
            let d = Big::from_limbs((0..dl).map(|_| next()).collect());
            if d.is_zero() {
                continue;
            }
            let (q, r) = divmod(&a, &d);
            assert!(r < d, "remainder not reduced");
            assert_eq!(add(&mul(&q, &d), &r), a);
        }
    }

    #[test]
    fn shifts() {
        let x = parse("01");
        assert_eq!(shl(&x, 128), parse("0100000000000000000000000000000000"));
        assert_eq!(shr(&shl(&x, 130), 130), x);
        assert_eq!(shr(&x, 1), Big::zero());
        assert_eq!(shl(&Big::zero(), 5), Big::zero());
    }

    #[test]
    fn modpow_fermat() {
        // 2^(p-1) mod p == 1 for prime p = 2^61 - 1
        let p = sub(&shl(&Big::one(), 61), &Big::one());
        let e = sub(&p, &Big::one());
        assert_eq!(modpow(&b(2), &e, &p), Big::one());
    }

    #[test]
    fn modpow_known_vector() {
        // 5^117 mod 19 == 1
        assert_eq!(modpow(&b(5), &b(117), &b(19)), Big::one());
        assert_eq!(modpow(&b(4), &b(13), &b(497)), b(445));
    }

    #[test]
    fn modinv_roundtrip() {
        let m = parse("ffffffffffffffffffffffffffffff61"); // a 128-bit prime
        let a = parse("deadbeefcafebabe0123456789abcdef");
        let inv = modinv(&a, &m).unwrap();
        assert_eq!(mulmod(&a, &inv, &m), Big::one());
    }

    #[test]
    fn modinv_none_when_not_coprime() {
        assert!(modinv(&b(6), &b(9)).is_none());
        assert_eq!(modinv(&b(3), &b(11)).unwrap(), b(4)); // 3*4 = 12 = 1 mod 11
    }

    #[test]
    fn bytes_roundtrip() {
        let raw = [0x01u8, 0x00, 0xff, 0xab, 0xcd];
        let n = Big::from_bytes_be(&raw);
        assert_eq!(n.to_bytes_be(), raw);
        assert_eq!(n.to_bytes_be_padded(8), [0, 0, 0, 1, 0, 0xff, 0xab, 0xcd]);
        // Leading zeros are not significant.
        assert_eq!(Big::from_bytes_be(&[0, 0, 5]), b(5));
        assert_eq!(Big::zero().to_bytes_be(), Vec::<u8>::new());
    }

    #[test]
    fn bit_len_and_bits() {
        assert_eq!(Big::zero().bit_len(), 0);
        assert_eq!(b(1).bit_len(), 1);
        assert_eq!(b(255).bit_len(), 8);
        let x = shl(&Big::one(), 200);
        assert_eq!(x.bit_len(), 201);
        assert!(x.bit(200) && !x.bit(199));
    }

    #[test]
    fn random_bits_has_requested_width() {
        for bits in [8usize, 64, 65, 512, 1024] {
            let n = random_bits(bits);
            assert_eq!(n.bit_len(), bits, "width {}", bits);
        }
    }
}
