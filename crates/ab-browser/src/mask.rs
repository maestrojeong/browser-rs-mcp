//! Phrase masking: replaces forbidden phrases with `[BLOCKED]` in any page
//! content returned to the caller (snapshot, text, html, markdown, find, tool errors).
//! Edit `BLOCKED_PHRASES` below. Matching is per-char simple-lowercase
//! case-insensitive (no multi-char folds like ß/SS). Empty = no-op.

pub const MASK: &str = "[BLOCKED]";

/// Forbidden phrases — add yours here.
const BLOCKED_PHRASES: &[&str] = &[
    "Verify you are human",
    "Please slide to verify",
    "CAPTCHA",
    "Human verification",
];

pub fn mask(text: String) -> String {
    if BLOCKED_PHRASES.is_empty() {
        return text;
    }
    mask_with(text, BLOCKED_PHRASES)
}

/// One unit of a phrase pattern.
enum Tok {
    Ch(char),
    /// Any run (>=1) of whitespace, `&nbsp;`, or HTML tags.
    Gap,
}

/// Longest tag we will skip as a gap unit (bounds worst-case scan cost).
const MAX_TAG_BYTES: usize = 64 * 1024;

fn compile(phrase: &str) -> Vec<Tok> {
    let mut out = Vec::new();
    for c in phrase.chars() {
        if c.is_whitespace() {
            if !matches!(out.last(), Some(Tok::Gap)) {
                out.push(Tok::Gap);
            }
        } else {
            out.push(Tok::Ch(c));
        }
    }
    out
}

/// Byte length of one gap unit at `s[i..]`, or 0 if none.
fn gap_unit(s: &str, i: usize) -> usize {
    let rest = &s[i..];
    let Some(c) = rest.chars().next() else {
        return 0;
    };
    if c.is_whitespace() || c == '\u{200b}' {
        return c.len_utf8();
    }
    if c == '&' {
        for ent in ["&nbsp;", "&#160;", "&#xa0;"] {
            if rest.len() >= ent.len() && rest[..ent.len()].eq_ignore_ascii_case(ent) {
                return ent.len();
            }
        }
        return 0;
    }
    if c == '<' {
        // A tag: `<` ... `>`, ignoring `<`/`>` inside quoted attribute values.
        let mut quote: Option<u8> = None;
        for (k, &b) in rest
            .as_bytes()
            .iter()
            .enumerate()
            .skip(1)
            .take(MAX_TAG_BYTES)
        {
            match quote {
                Some(q) => {
                    if b == q {
                        quote = None;
                    }
                }
                None => match b {
                    b'"' | b'\'' => quote = Some(b),
                    b'<' => return 0,
                    b'>' => return k + 1,
                    _ => {}
                },
            }
        }
    }
    0
}

/// Try to match `pat` at byte offset `start`; returns the end byte offset.
fn match_at(s: &str, start: usize, pat: &[Tok]) -> Option<usize> {
    let mut i = start;
    for tok in pat {
        match tok {
            Tok::Ch(p) => {
                let c = s[i..].chars().next()?;
                if !c.to_lowercase().eq(p.to_lowercase()) {
                    return None;
                }
                i += c.len_utf8();
            }
            Tok::Gap => {
                let mut n = 0;
                loop {
                    let u = gap_unit(s, i);
                    if u == 0 {
                        break;
                    }
                    i += u;
                    n += 1;
                }
                if n == 0 {
                    return None;
                }
            }
        }
    }
    Some(i)
}

/// Replace every occurrence of each phrase with `[BLOCKED]`. Matching is
/// case-insensitive, and whitespace in a phrase matches any run of
/// whitespace / `&nbsp;` / HTML tags (so `Verify <b>you</b> are human` and
/// `Verify\n you  are human` are caught). Aria-labels and other attributes
/// are covered wherever the raw HTML/AX text is returned.
///
/// Single pass over the text; allocates the output only when a match exists.
pub fn mask_with(text: String, phrases: &[&str]) -> String {
    let pats: Vec<Vec<Tok>> = phrases
        .iter()
        .map(|p| compile(p.trim()))
        .filter(|p| !p.is_empty())
        .collect();
    if pats.is_empty() {
        return text;
    }
    let mut res: Option<String> = None;
    let mut copied = 0; // bytes of `text` already flushed into `res`
    let mut i = 0;
    while i < text.len() {
        let end = pats.iter().filter_map(|p| match_at(&text, i, p)).max();
        if let Some(end) = end {
            let out = res.get_or_insert_with(|| String::with_capacity(text.len()));
            out.push_str(&text[copied..i]);
            out.push_str(MASK);
            copied = end;
            i = end;
        } else {
            i += text[i..].chars().next().map_or(1, char::len_utf8);
        }
    }
    match res {
        Some(mut out) => {
            out.push_str(&text[copied..]);
            out
        }
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn masks_case_insensitive_and_korean() {
        assert_eq!(
            mask_with("a Secret b 금지어 c SECRET".into(), &["secret", "금지어"]),
            "a [BLOCKED] b [BLOCKED] c [BLOCKED]"
        );
    }
    #[test]
    fn empty_is_noop() {
        assert_eq!(mask_with("x".into(), &[]), "x");
    }
    #[test]
    fn whitespace_and_tags() {
        let p = ["verify you are human"];
        assert_eq!(
            mask_with("Verify\n  you\u{a0}are human!".into(), &p),
            "[BLOCKED]!"
        );
        assert_eq!(
            mask_with("Verify <b>you</b> are&nbsp;human".into(), &p),
            "[BLOCKED]"
        );
    }
    #[test]
    fn aria_label_attribute() {
        assert_eq!(
            mask_with(
                r#"<div aria-label="Please slide to verify">x</div>"#.into(),
                &["please slide to verify"]
            ),
            r#"<div aria-label="[BLOCKED]">x</div>"#
        );
    }
    #[test]
    fn no_partial_gap_match() {
        assert_eq!(mask_with("youare".into(), &["you are"]), "youare");
    }
    #[test]
    fn quoted_gt_in_tag() {
        assert_eq!(
            mask_with(
                r#"Verify <span title="a > b">you</span> are human"#.into(),
                &["verify you are human"]
            ),
            "[BLOCKED]"
        );
    }
    #[test]
    fn multiple_phrases_one_pass() {
        assert_eq!(
            mask_with(
                "CAPTCHA and captcha, Human  verification".into(),
                &["captcha", "human verification"]
            ),
            "[BLOCKED] and [BLOCKED], [BLOCKED]"
        );
    }
}
