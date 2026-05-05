//! Extension trait adding string-analysis helper methods to `str`.
use crate::indexer::is_id_char;

pub(crate) trait StrExt {
    /// Returns `true` if `self` starts with an uppercase letter (Unicode-aware).
    /// Returns `false` for empty strings.
    fn starts_with_uppercase(&self) -> bool;

    /// Returns `true` if `self` starts with a lowercase letter (Unicode-aware).
    /// Returns `false` for empty strings.
    fn starts_with_lowercase(&self) -> bool;

    /// Returns the leading identifier portion of `self` — all leading chars satisfying `is_id_char`.
    /// `"foo.bar()"` → `"foo"`;  `"Bar<T>"` → `"Bar"`.
    fn ident_prefix(&self) -> String;

    /// Returns the leading dotted-identifier portion of `self` — all leading chars satisfying
    /// `is_id_char` or `.`. `"foo.Bar.baz()"` → `"foo.Bar.baz"`.
    fn dotted_ident_prefix(&self) -> String;

    /// Returns the trailing dot-separated segment of a dotted path.
    /// `"com.example.Foo"` → `"Foo"`, `"Foo"` → `"Foo"`.
    fn last_segment(&self) -> &str;

    /// Returns the trailing identifier at the end of `self` — all trailing chars satisfying `is_id_char`.
    /// `"foo.barBaz"` → `"barBaz"`;  `"foo.bar("` → `""`.
    fn last_ident_in(&self) -> &str;

    /// Returns the declaration-keyword prefix of `self` — strips leading whitespace and annotations.
    fn decl_prefix(&self) -> &str;

    /// Returns the identifier word at `utf16_col` (a UTF-16 code-unit offset, as in LSP positions).
    fn word_at_utf16_col(&self, utf16_col: usize) -> String;
}

impl StrExt for str {
    #[inline]
    fn starts_with_uppercase(&self) -> bool {
        self.chars()
            .next()
            .map(|c| c.is_uppercase())
            .unwrap_or(false)
    }

    #[inline]
    fn starts_with_lowercase(&self) -> bool {
        self.chars()
            .next()
            .map(|c| c.is_lowercase())
            .unwrap_or(false)
    }

    #[inline]
    fn ident_prefix(&self) -> String {
        self.chars().take_while(|&c| is_id_char(c)).collect()
    }

    #[inline]
    fn dotted_ident_prefix(&self) -> String {
        self.chars()
            .take_while(|&c| is_id_char(c) || c == '.')
            .collect()
    }

    #[inline]
    fn last_segment(&self) -> &str {
        self.rsplit('.').next().unwrap_or(self)
    }

    #[inline]
    fn last_ident_in(&self) -> &str {
        let ident_bytes: usize = self
            .chars()
            .rev()
            .take_while(|&c| is_id_char(c))
            .map(|c| c.len_utf8())
            .sum();
        &self[self.len() - ident_bytes..]
    }

    #[inline]
    fn decl_prefix(&self) -> &str {
        self.split_once('{')
            .map(|(l, _)| l)
            .unwrap_or(self)
            .split_once('=')
            .map(|(l, _)| l)
            .unwrap_or(self)
    }

    fn word_at_utf16_col(&self, utf16_col: usize) -> String {
        let chars: Vec<char> = self.chars().collect();
        // Convert UTF-16 code-unit offset to char index.
        let col = {
            let mut cu = 0usize;
            let mut idx = chars.len();
            for (i, c) in chars.iter().enumerate() {
                if cu >= utf16_col { idx = i; break; }
                cu += c.len_utf16();
            }
            idx
        };
        let mut ws = col;
        while ws > 0 && (chars[ws - 1].is_alphanumeric() || chars[ws - 1] == '_') { ws -= 1; }
        let mut we = col;
        while we < chars.len() && (chars[we].is_alphanumeric() || chars[we] == '_') { we += 1; }
        chars[ws..we].iter().collect()
    }
}

#[cfg(test)]
mod tests {
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
}
