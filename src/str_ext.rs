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
}
