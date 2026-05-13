//! Slugification used by both `DocumentKind::folder_slug` and the
//! filename policy. One implementation, one source of truth.

/// Map `input` to a `[a-z0-9-]+` slug: lowercase ASCII alphanumerics
/// pass through, everything else collapses to a single `-`. Leading and
/// trailing dashes are stripped.
pub(crate) fn slugify(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut prev_dash = false;
    for ch in input.chars() {
        let mapped = ch.to_ascii_lowercase();
        if mapped.is_ascii_alphanumeric() {
            out.push(mapped);
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapses_whitespace_and_punctuation_to_single_dash() {
        assert_eq!(slugify("Hello, World!"), "hello-world");
        assert_eq!(slugify("Acme & Co."), "acme-co");
        assert_eq!(slugify("   spaced   out   "), "spaced-out");
    }

    #[test]
    fn empty_input_yields_empty_string() {
        assert_eq!(slugify(""), "");
        assert_eq!(slugify("!!!"), "");
    }
}
