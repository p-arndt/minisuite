// Quoted-printable (RFC 2045 §6.7) + RFC 2047 'Q' decode. Pure std.

/// Standard QP: "=XX" hex escapes; soft line breaks "=\r\n" and "=\n" dropped.
/// Lenient: a lone '=' not followed by valid hex is passed through literally.
pub fn decode(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        let b = input[i];
        if b == b'=' {
            if input.get(i + 1) == Some(&b'\n') {
                i += 2; // soft break "=\n"
            } else if input.get(i + 1) == Some(&b'\r') && input.get(i + 2) == Some(&b'\n') {
                i += 3; // soft break "=\r\n"
            } else if let Some(v) = hex_pair(input, i + 1) {
                out.push(v);
                i += 3;
            } else {
                out.push(b'='); // lone '=': not a valid escape, pass through
                i += 1;
            }
        } else {
            out.push(b);
            i += 1;
        }
    }
    out
}

/// RFC 2047 Q-encoding: '_' -> 0x20, then "=XX"; NO soft line breaks.
pub fn decode_q(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        match input[i] {
            b'_' => {
                out.push(b' ');
                i += 1;
            }
            b'=' => {
                if let Some(v) = hex_pair(input, i + 1) {
                    out.push(v);
                    i += 3;
                } else {
                    out.push(b'='); // lone '=': pass through (no soft breaks in Q)
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    out
}

/// Decode the two hex digits at `input[i]`/`input[i+1]` into a byte, or None if
/// either is out of range or not a hex digit.
fn hex_pair(input: &[u8], i: usize) -> Option<u8> {
    let h = hex_val(*input.get(i)?)?;
    let l = hex_val(*input.get(i + 1)?)?;
    Some((h << 4) | l)
}

pub(crate) fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn soft_line_break_crlf() {
        // "=\r\n" at end of line joins the two physical lines.
        assert_eq!(decode(b"hello=\r\nworld"), b"helloworld");
    }

    #[test]
    fn soft_line_break_bare_lf() {
        assert_eq!(decode(b"hello=\nworld"), b"helloworld");
    }

    #[test]
    fn hex_escapes_upper_and_lower() {
        // 0x3D '=', 0xE9 'é' — exercise both upper- and lowercase hex digits.
        assert_eq!(decode(b"a=3Db"), b"a=b");
        assert_eq!(decode(b"caf=E9"), b"caf\xe9");
        assert_eq!(decode(b"caf=e9"), b"caf\xe9");
        assert_eq!(decode(b"=ff=FF"), b"\xff\xff");
    }

    #[test]
    fn trailing_lone_equals_passes_through() {
        assert_eq!(decode(b"abc="), b"abc=");
        // '=' followed by a single hex digit then EOF is also incomplete.
        assert_eq!(decode(b"abc=A"), b"abc=A");
    }

    #[test]
    fn lone_equals_midstream_passes_through() {
        // '=' not followed by valid hex nor a line ending is literal.
        assert_eq!(decode(b"1 = 2"), b"1 = 2");
        assert_eq!(decode(b"a=Zb"), b"a=Zb"); // 'Z' is not a hex digit
    }

    #[test]
    fn literal_chars_passed_through() {
        assert_eq!(decode(b"Plain ASCII text!"), b"Plain ASCII text!");
        // Underscore is literal in standard QP (only special in the Q variant).
        assert_eq!(decode(b"a_b"), b"a_b");
    }

    #[test]
    fn q_underscore_is_space() {
        assert_eq!(decode_q(b"a_b"), b"a b");
        assert_eq!(decode_q(b"Keith_Moore"), b"Keith Moore");
    }

    #[test]
    fn q_vs_qp_underscore_difference() {
        // Same input, different meaning of '_' between the two decoders.
        let input = b"x_y";
        assert_eq!(decode(input), b"x_y");
        assert_eq!(decode_q(input), b"x y");
    }

    #[test]
    fn q_does_not_honor_soft_breaks() {
        // "=\r\n" is a soft break in QP but a lone '=' + CRLF in Q.
        assert_eq!(decode(b"a=\r\nb"), b"ab");
        assert_eq!(decode_q(b"a=\r\nb"), b"a=\r\nb");
    }

    #[test]
    fn q_hex_escapes() {
        // RFC 2047 example: "=?ISO-8859-1?Q?=A1Hola?=" payload decodes 0xA1.
        assert_eq!(decode_q(b"=A1Hola"), b"\xa1Hola");
        assert_eq!(decode_q(b"caf=e9"), b"caf\xe9");
    }

    #[test]
    fn hex_val_bounds() {
        assert_eq!(hex_val(b'0'), Some(0));
        assert_eq!(hex_val(b'9'), Some(9));
        assert_eq!(hex_val(b'a'), Some(10));
        assert_eq!(hex_val(b'F'), Some(15));
        assert_eq!(hex_val(b'g'), None);
        assert_eq!(hex_val(b' '), None);
    }
}
