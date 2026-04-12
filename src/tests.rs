#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_between_chars() {
        // Test basic ASCII characters
        assert_eq!(
            between_chars("hello \"world\" test", '"', '"'),
            Some("world")
        );

        // Test with Unicode characters
        assert_eq!(between_chars("hello \"世界\" test", '"', '"'), Some("世界"));

        // Test with mixed ASCII and Unicode
        assert_eq!(
            between_chars("hello \"wörld\" test", '"', '"'),
            Some("wörld")
        );

        // Test when left char not found
        assert_eq!(between_chars("hello world test", '[', ']'), None);

        // Test when right char not found
        assert_eq!(between_chars("hello [world test", '[', ']'), None);

        // Test with empty content between chars
        assert_eq!(between_chars("hello \"\" test", '"', '"'), Some(""));

        // Test with multiple pairs (should return first match)
        assert_eq!(
            between_chars("hello [first] and [second] test", '[', ']'),
            Some("first")
        );
    }

    #[test]
    fn test_parse_quoted() {
        // Test basic quoted string
        assert_eq!(parse_quoted("\"hello world\"").unwrap(), "hello world");

        // Test with Unicode
        assert_eq!(parse_quoted("\"héllo wörld\"").unwrap(), "héllo wörld");

        // Test with escaped quotes
        assert_eq!(
            parse_quoted("\"hello \\\"world\\\"\"").unwrap(),
            "hello \"world\""
        );

        // Test missing quotes
        assert!(parse_quoted("hello world").is_err());

        // Test empty quoted string
        assert_eq!(parse_quoted("\"\"").unwrap(), "");

        // Test single quote
        assert_eq!(parse_quoted("\"a\"").unwrap(), "a");
    }
}
