//! Phrase masking: replaces forbidden phrases with `[BLOCKED]` in any page
//! content returned to the caller (snapshot, text, html, markdown, find).
//! Edit `BLOCKED_PHRASES` below (ASCII case-insensitive). Empty = no-op.

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

/// Length of one gap unit at `t[i..]`, or 0 if none.
fn gap_unit(t: &[char], i: usize) -> usize {
    let Some(&c) = t.get(i) else { return 0 };
    if c.is_whitespace() || c == '\u{200b}' {
        return 1;
    }
    if c == '&' {
        for ent in ["&nbsp;", "&#160;", "&#xa0;"] {
            let e: Vec<char> = ent.chars().collect();
            if t.len() >= i + e.len() && t[i..i + e.len()] == e[..] {
                return e.len();
            }
        }
        return 0;
    }
    if c == '<' {
        // A tag: `<` ... `>` with no nested `<`, bounded length.
        for (k, &d) in t.iter().enumerate().skip(i + 1).take(500) {
            if d == '<' {
                return 0;
            }
            if d == '>' {
                return k + 1 - i;
            }
        }
    }
    0
}

/// Try to match `pat` at `t[start..]`; returns the end index.
fn match_at(t: &[char], start: usize, pat: &[Tok]) -> Option<usize> {
    let mut i = start;
    for tok in pat {
        match tok {
            Tok::Ch(p) => {
                let c = *t.get(i)?;
                if !c.to_lowercase().eq(p.to_lowercase()) {
                    return None;
                }
                i += 1;
            }
            Tok::Gap => {
                let mut n = 0;
                loop {
                    let u = gap_unit(t, i);
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
pub fn mask_with(text: String, phrases: &[&str]) -> String {
    let mut out = text;
    for p in phrases {
        let pat = compile(p.trim());
        if pat.is_empty() {
            continue;
        }
        let t: Vec<char> = out.chars().collect();
        let mut res = String::with_capacity(out.len());
        let mut i = 0;
        let mut hit = false;
        while i < t.len() {
            if let Some(end) = match_at(&t, i, &pat) {
                res.push_str(MASK);
                i = end;
                hit = true;
            } else {
                res.push(t[i]);
                i += 1;
            }
        }
        if hit {
            out = res;
        }
    }
    out
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
}
