// Cryptographically secure OS randomness. Pure std, no external deps.
// The single `unsafe` block is the Windows BCryptGenRandom FFI.

/// Fill `buf` with cryptographically secure random bytes.
/// Panics if the OS RNG fails (correct for a keygen path).
#[cfg(unix)]
pub fn fill(buf: &mut [u8]) {
    use std::io::Read;
    if buf.is_empty() {
        return;
    }
    let mut f = std::fs::File::open("/dev/urandom").expect("open /dev/urandom");
    f.read_exact(buf).expect("read /dev/urandom");
}

#[cfg(windows)]
#[link(name = "bcrypt")]
extern "system" {
    fn BCryptGenRandom(h: *mut core::ffi::c_void, buf: *mut u8, len: u32, flags: u32) -> i32;
}

#[cfg(windows)]
const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 0x0000_0002;

/// Fill `buf` with cryptographically secure random bytes.
/// Panics if the OS RNG fails (correct for a keygen path).
#[cfg(windows)]
pub fn fill(buf: &mut [u8]) {
    if buf.is_empty() {
        return;
    }
    // BCryptGenRandom takes a u32 length; chunk to stay within range.
    for chunk in buf.chunks_mut(u32::MAX as usize) {
        let status = unsafe {
            BCryptGenRandom(
                core::ptr::null_mut(),
                chunk.as_mut_ptr(),
                chunk.len() as u32,
                BCRYPT_USE_SYSTEM_PREFERRED_RNG,
            )
        };
        assert!(status == 0, "BCryptGenRandom failed: 0x{:08x}", status);
    }
}

/// Return `n` cryptographically secure random bytes.
pub fn bytes(n: usize) -> Vec<u8> {
    let mut v = vec![0u8; n];
    fill(&mut v);
    v
}

/// Return an unpadded base64url token derived from `n_bytes` of randomness.
pub fn token(n_bytes: usize) -> String {
    crate::base64::encode_url(&bytes(n_bytes))
}

/// Return a lowercase hex string of `n_bytes` random bytes.
pub fn hex(n_bytes: usize) -> String {
    let mut s = String::with_capacity(n_bytes * 2);
    for b in bytes(n_bytes) {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_not_all_zero() {
        let b = bytes(32);
        assert_eq!(b.len(), 32);
        assert!(b.iter().any(|&x| x != 0));
    }

    #[test]
    fn two_calls_differ() {
        assert_ne!(bytes(32), bytes(32));
    }

    #[test]
    fn hex_format() {
        let h = hex(16);
        assert_eq!(h.len(), 32);
        assert!(h
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn token_is_urlsafe() {
        let t = token(32);
        assert!(!t.contains('='));
        assert!(!t.contains('+'));
        assert!(!t.contains('/'));
    }

    #[test]
    fn empty_fill_no_panic() {
        fill(&mut []);
    }
}
