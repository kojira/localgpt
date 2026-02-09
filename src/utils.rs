/// Truncate a string to at most `max_bytes` bytes without splitting a multi-byte
/// character. Returns the original string if it already fits.
pub fn safe_truncate(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_within_limit() {
        assert_eq!(safe_truncate("hello", 10), "hello");
    }

    #[test]
    fn ascii_at_limit() {
        assert_eq!(safe_truncate("hello", 5), "hello");
    }

    #[test]
    fn ascii_truncated() {
        assert_eq!(safe_truncate("hello world", 5), "hello");
    }

    #[test]
    fn multibyte_no_split() {
        // "あいう" = 3 chars × 3 bytes = 9 bytes
        let s = "あいう";
        // Truncating at 7 bytes should back up to 6 (2 full chars)
        assert_eq!(safe_truncate(s, 7), "あい");
    }

    #[test]
    fn multibyte_exact_boundary() {
        let s = "あいう";
        assert_eq!(safe_truncate(s, 6), "あい");
    }

    #[test]
    fn empty_string() {
        assert_eq!(safe_truncate("", 10), "");
    }

    #[test]
    fn zero_max() {
        assert_eq!(safe_truncate("hello", 0), "");
    }
}
