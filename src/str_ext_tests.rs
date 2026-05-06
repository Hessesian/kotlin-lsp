use super::StrExt;

// ── word_at_utf16_col ────────────────────────────────────────────────

#[test]
fn word_at_ascii() {
    assert_eq!("val foo: Int".word_at_utf16_col(4), "foo");
    assert_eq!("val foo: Int".word_at_utf16_col(5), "foo");
    assert_eq!("val foo: Int".word_at_utf16_col(6), "foo");
}

#[test]
fn word_at_col_zero_is_first_word() {
    assert_eq!("myVal = 1".word_at_utf16_col(0), "myVal");
}

#[test]
fn word_at_bmp_two_byte_utf8() {
    // 'é' is U+00E9 — 1 UTF-16 code unit, 2 UTF-8 bytes.
    // "aéb": 'a'=col 0, 'é'=col 1 (1 unit), 'b'=col 2.
    // A byte-offset algorithm would interpret col 2 as the second byte of 'é'
    // (a broken char boundary), not 'b'. The UTF-16-aware algorithm must return "aéb".
    let s = "a\u{00E9}b";
    assert_eq!(s.word_at_utf16_col(2), "a\u{00E9}b");
}

#[test]
fn word_at_surrogate_pair() {
    // '𝕳' is U+1D573 — 2 UTF-16 code units (surrogate pair), 4 UTF-8 bytes.
    // "𝕳ello": col 0 → first surrogate, col 2 → 'e'.
    // '𝕳' is a mathematical letter (alphanumeric), so the whole "𝕳ello" is one word.
    let s = "\u{1D573}ello";
    assert_eq!(s.word_at_utf16_col(0), "\u{1D573}ello");
    assert_eq!(s.word_at_utf16_col(2), "\u{1D573}ello");
}

#[test]
fn word_at_beyond_end_clamps_to_last_word() {
    // A col past the end is clamped to the last character position;
    // the scan then finds the enclosing word as usual.
    assert_eq!("val foo".word_at_utf16_col(100), "foo");
}

#[test]
fn word_at_non_word_char_returns_empty() {
    // A position that lands on a non-alphanumeric, non-underscore char
    // should return the empty string.
    assert_eq!("foo + bar".word_at_utf16_col(4), "");
}
