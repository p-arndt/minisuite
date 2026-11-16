// Edit-distance suggestions for mistyped flags and keys. The same helper lives in
// each service crate's util.rs: the crates do not depend on each other.

/// The candidate closest to `input` by edit distance, if it is close enough to
/// be a typo. The allowance grows with the input, so a short flag must be
/// nearly exact while a long one may be off by a couple of characters.
pub fn suggest<'a>(input: &str, candidates: &[&'a str]) -> Option<&'a str> {
    let limit = (input.chars().count() / 3).max(2);
    candidates
        .iter()
        .map(|c| (edit_distance(input, c), *c))
        .filter(|(d, _)| *d <= limit)
        .min_by_key(|(d, _)| *d)
        .map(|(_, c)| c)
}

/// The message for an argument no parser arm recognises, with a hint when one
/// of `flags` is a near miss.
pub fn unknown_argument(arg: &str, flags: &[&str]) -> String {
    match suggest(arg, flags) {
        Some(s) => format!("unknown argument: {} (did you mean {}?)", arg, s),
        None => format!("unknown argument: {}", arg),
    }
}

fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1; b.len() + 1];
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != *cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        prev = cur;
    }
    prev[b.len()]
}

#[cfg(test)]
mod suggest_tests {
    use super::*;

    const FLAGS: &[&str] = &["--realm", "--bind", "--config"];

    #[test]
    fn a_close_typo_gets_the_nearest_flag() {
        assert_eq!(suggest("--realn", FLAGS), Some("--realm"));
        assert_eq!(suggest("--bnd", FLAGS), Some("--bind"));
    }

    #[test]
    fn garbage_gets_no_suggestion() {
        assert_eq!(suggest("--zzzzzzzz", FLAGS), None);
        assert_eq!(suggest("--x", FLAGS), None);
    }

    #[test]
    fn the_message_names_the_hint_only_when_there_is_one() {
        assert_eq!(
            unknown_argument("--realn", FLAGS),
            "unknown argument: --realn (did you mean --realm?)"
        );
        assert_eq!(
            unknown_argument("--zzzzzzzz", FLAGS),
            "unknown argument: --zzzzzzzz"
        );
    }
}
