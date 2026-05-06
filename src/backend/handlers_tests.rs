use super::word_byte_offsets;

#[test]
fn finds_single_word() {
    let offsets: Vec<_> = word_byte_offsets("hello world", "world").collect();
    assert_eq!(offsets, vec![6]);
}

#[test]
fn skips_partial_match() {
    // "name" should not match inside "rename"
    let offsets: Vec<_> = word_byte_offsets("rename name", "name").collect();
    assert_eq!(offsets, vec![7]);
}

#[test]
fn multiple_occurrences() {
    let offsets: Vec<_> = word_byte_offsets("a b a c a", "a").collect();
    assert_eq!(offsets, vec![0, 4, 8]);
}

#[test]
fn unicode_line() {
    // "ñ" is 2 bytes in UTF-8; "name" after it still at correct byte offset
    let line = "ñ name ñ";
    let offsets: Vec<_> = word_byte_offsets(line, "name").collect();
    assert_eq!(offsets.len(), 1);
    assert_eq!(&line[offsets[0]..offsets[0] + 4], "name");
}

#[test]
fn no_match() {
    let offsets: Vec<_> = word_byte_offsets("foo bar", "baz").collect();
    assert!(offsets.is_empty());
}
