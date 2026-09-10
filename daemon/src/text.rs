//! Text bounds shared by every persisted document.
//!
//! One home for the truncation rule, because there were two byte-identical
//! private copies (`control_paths.rs`, `pwm_baselines.rs`) before `P8-br` needed
//! a third. Re-deriving a rule locally for each new consumer is the DEC-276
//! defect: each copy can be improved without the others hearing about it.

/// Truncate on a **character** boundary, never a byte one.
///
/// `String::truncate` panics mid-codepoint, and the strings this bounds — hwmon
/// labels, sensor ids, daemon-formatted `detail` prose — can legitimately
/// contain non-ASCII. Truncating down to the nearest boundary yields at most
/// `max_bytes`, never more, which is what every byte-budget derivation assumes.
pub fn truncate(s: &mut String, max_bytes: usize) {
    if s.len() <= max_bytes {
        return;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_string_at_or_under_the_bound_is_untouched() {
        let mut s = "abcd".to_string();
        truncate(&mut s, 4);
        assert_eq!(
            s, "abcd",
            "a string exactly at the bound must survive whole"
        );
        truncate(&mut s, 99);
        assert_eq!(s, "abcd");
    }

    #[test]
    fn an_ascii_string_truncates_to_exactly_the_bound() {
        let mut s = "x".repeat(100);
        truncate(&mut s, 40);
        assert_eq!(s.len(), 40);
    }

    /// The reason this is not `String::truncate`. Each `é` is two bytes, so a
    /// bound of 5 lands mid-codepoint; the naive call panics, and a byte count
    /// derived from a panicking truncation is not a bound at all.
    #[test]
    fn a_bound_landing_mid_codepoint_steps_back_instead_of_panicking() {
        let mut s = "ééé".to_string();
        assert_eq!(s.len(), 6, "the fixture must really straddle the bound");
        truncate(&mut s, 5);
        assert_eq!(s, "éé", "must step back to the boundary below");
        assert!(s.len() <= 5);
    }

    /// A bound below the first character's width empties the string rather than
    /// underflowing the loop counter.
    #[test]
    fn a_bound_under_the_first_character_yields_an_empty_string() {
        let mut s = "€uro".to_string();
        assert_eq!(s.chars().next().unwrap().len_utf8(), 3);
        truncate(&mut s, 2);
        assert!(s.is_empty());
    }
}
