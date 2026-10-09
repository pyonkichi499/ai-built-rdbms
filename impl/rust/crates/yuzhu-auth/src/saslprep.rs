//! `SASLprep` (RFC 4013).

use std::borrow::Cow;

/// `SASLprep`. When a prohibited character or bidi check fails the input is
/// returned unchanged (as PostgreSQL's callers of `pg_saslprep` do).
/// Returns `Borrowed` when the result equals the input.
pub fn prepare(password: &str) -> Cow<'_, str> {
    try_prepare(password).unwrap_or(Cow::Borrowed(password))
}

/// Like [`prepare`] but reports failure.
#[allow(clippy::result_unit_err)]
pub fn try_prepare(password: &str) -> Result<Cow<'_, str>, ()> {
    match stringprep::saslprep(password) {
        Ok(Cow::Owned(s)) if s == password => Ok(Cow::Borrowed(password)),
        Ok(c) => Ok(c),
        Err(_) => Err(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc4013_examples() {
        assert_eq!(prepare("I\u{00AD}X"), "IX");
        assert_eq!(prepare("user"), "user");
        assert_eq!(prepare("USER"), "USER");
        assert_eq!(prepare("\u{00AA}"), "a");
        assert_eq!(prepare("\u{2168}"), "IX");
    }

    #[test]
    fn ascii_is_borrowed() {
        assert!(matches!(prepare("pencil"), Cow::Borrowed(_)));
        assert!(matches!(prepare(""), Cow::Borrowed(_)));
        assert!(matches!(try_prepare("héllo"), Ok(Cow::Borrowed(_))));
    }

    #[test]
    fn prohibited_fails_and_falls_back() {
        assert!(try_prepare("\u{0007}").is_err());
        assert_eq!(prepare("\u{0007}"), "\u{0007}");
        assert!(try_prepare("\u{0627}\u{0031}").is_err());
        assert_eq!(prepare("\u{0627}\u{0031}"), "\u{0627}\u{0031}");
    }
}
