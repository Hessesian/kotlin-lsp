//! Shared text-processing utilities for feature modules.

/// Iterates over the byte offsets in `line` where `word` appears as a whole
/// word (not as a substring of a longer identifier).
pub(crate) fn word_byte_offsets<'a>(
    line: &'a str,
    word: &'a str,
) -> impl Iterator<Item = usize> + 'a {
    let word_len = word.len();
    let is_id = |c: char| c.is_alphanumeric() || c == '_';
    let mut search_from = 0;
    std::iter::from_fn(move || {
        while let Some(rel) = line[search_from..].find(word) {
            let pos = search_from + rel;
            search_from = pos + word_len;
            let before_ok = pos == 0 || !is_id(line[..pos].chars().next_back()?);
            let after_ok =
                pos + word_len >= line.len() || !is_id(line[pos + word_len..].chars().next()?);
            if before_ok && after_ok {
                return Some(pos);
            }
        }
        None
    })
}

/// Counts UTF-16 code units in `text` (for LSP column offsets).
pub(crate) fn utf16_column(text: &str) -> u32 {
    text.chars().map(|c| c.len_utf16() as u32).sum()
}
