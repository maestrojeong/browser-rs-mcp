//! Phrase rewriting: replaces configured phrases with a substitute (or
//! `[BLOCKED]`) in text returned to the caller. Edit `RULES` below; it is
//! empty by default, which makes everything here a no-op.
//!
//! Matching is per-char simple-lowercase case-insensitive, and whitespace in a
//! phrase matches any run of whitespace / `&nbsp;` / HTML tags.

pub const MASK: &str = "[BLOCKED]";

/// Boundary marker between separate text nodes when segments are joined
/// (see [`mask_segments_rules`]). It is transparent inside words and counts as a
/// gap unit between words.
const SEP: char = '\u{1f}';

/// `(phrase, replacement, whole_word)`.
/// - `replacement`: text to put in place of the phrase; `""` = `[BLOCKED]`.
///   It must not itself contain any phrase (masking is applied on several
///   layers, so the output has to be a fixed point).
/// - `whole_word`: only match when not embedded in a longer word
///   (letters/digits/`_`).
pub type Rule = (&'static str, &'static str, bool);

/// Rules applied to everything returned to the caller. Example entry:
/// `("some phrase", "replacement", false),`
pub const RULES: &[Rule] = &[];

pub fn mask(text: String) -> String {
    mask_rules(text, RULES)
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
    if c.is_whitespace() || c == '\u{200b}' || c == SEP {
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
                // Node boundaries are invisible inside a word.
                while s[i..].starts_with(SEP) {
                    i += SEP.len_utf8();
                }
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

struct Compiled {
    pat: Vec<Tok>,
    replace: &'static str,
    whole_word: bool,
}

fn compile_all(rules: &[Rule]) -> Vec<Compiled> {
    rules
        .iter()
        .map(|&(phrase, replace, whole_word)| Compiled {
            pat: compile(phrase.trim()),
            replace: if replace.is_empty() { MASK } else { replace },
            whole_word,
        })
        .filter(|c| !c.pat.is_empty())
        .collect()
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// True when `text[start..end]` is not glued to a neighbouring word char
/// (node boundaries are looked through).
fn at_word_boundary(text: &str, start: usize, end: usize) -> bool {
    let before = text[..start].chars().rev().find(|&c| c != SEP);
    let after = text[end..].chars().find(|&c| c != SEP);
    !before.is_some_and(is_word_char) && !after.is_some_and(is_word_char)
}

/// Leftmost, longest, non-overlapping matches: `(start, end, rule index)`.
fn find_matches(text: &str, rules: &[Compiled]) -> Vec<(usize, usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < text.len() {
        if text[i..].starts_with(SEP) {
            i += SEP.len_utf8();
            continue;
        }
        let best = rules
            .iter()
            .enumerate()
            .filter_map(|(k, r)| {
                let end = match_at(text, i, &r.pat)?;
                (!r.whole_word || at_word_boundary(text, i, end)).then_some((end, k))
            })
            .max_by_key(|&(end, _)| end);
        if let Some((end, k)) = best {
            out.push((i, end, k));
            i = end;
        } else {
            i += text[i..].chars().next().map_or(1, char::len_utf8);
        }
    }
    out
}

/// Apply `rules` to `text`. Matching is case-insensitive, and whitespace in a
/// phrase matches any run of whitespace / `&nbsp;` / HTML tags (so
/// `Verify <b>you</b> are human` and `Verify\n you  are human` are caught).
/// Aria-labels and other attributes are covered wherever the raw HTML/AX text
/// is returned.
///
/// Single pass over the text; allocates the output only when a match exists.
pub fn mask_rules(text: String, rules: &[Rule]) -> String {
    let compiled = compile_all(rules);
    if compiled.is_empty() {
        return text;
    }
    let matches = find_matches(&text, &compiled);
    if matches.is_empty() {
        return text;
    }
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    for (s, e, k) in matches {
        out.push_str(&text[copied..s]);
        out.push_str(compiled[k].replace);
        copied = e;
    }
    out.push_str(&text[copied..]);
    out
}

/// Rewrite phrases that may span several adjacent text nodes (e.g. the
/// accessibility tree splits `Verify <b>you</b> are human` into three
/// `StaticText` nodes). The segments are matched as one joined text; for each
/// match the first touched segment receives the replacement in place of its
/// part and the other touched segments lose theirs. Returns one string per
/// input segment.
pub fn mask_segments_rules(segs: &[String], rules: &[Rule]) -> Vec<String> {
    let compiled = compile_all(rules);
    if compiled.is_empty() || segs.is_empty() {
        return segs.to_vec();
    }
    let joined = segs.join(&SEP.to_string());
    let matches = find_matches(&joined, &compiled);
    if matches.is_empty() {
        return segs.to_vec();
    }
    let mut inserted = vec![false; matches.len()];
    let mut out = Vec::with_capacity(segs.len());
    let mut start = 0;
    for seg in segs {
        let end = start + seg.len();
        let mut res = String::with_capacity(seg.len());
        let mut copied = start;
        for (m, &(ms, me, k)) in matches.iter().enumerate() {
            let (os, oe) = (ms.max(start), me.min(end));
            if os >= oe {
                continue;
            }
            res.push_str(&joined[copied..os]);
            if !inserted[m] {
                res.push_str(compiled[k].replace);
                inserted[m] = true;
            }
            copied = oe;
        }
        res.push_str(&joined[copied..end]);
        out.push(res);
        start = end + SEP.len_utf8();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_RULES: &[Rule] = &[
        ("verify you are human", "Complete the check", false),
        ("captcha", "check", true),
        ("secret-id", "", false),
        ("금지어", "대체어", false),
    ];

    #[test]
    fn no_rules_is_noop() {
        assert_eq!(
            mask_rules("Verify you are human".into(), &[]),
            "Verify you are human"
        );
    }
    #[test]
    fn replaces_case_insensitive_with_default_marker() {
        assert_eq!(
            mask_rules("a SECRET-ID b 금지어".into(), TEST_RULES),
            "a [BLOCKED] b 대체어"
        );
    }
    #[test]
    fn whitespace_and_tags() {
        assert_eq!(
            mask_rules("Verify\n  you\u{a0}are human!".into(), TEST_RULES),
            "Complete the check!"
        );
        assert_eq!(
            mask_rules(
                r#"Verify <span title="a > b">you</span> are&nbsp;human"#.into(),
                TEST_RULES
            ),
            "Complete the check"
        );
        assert_eq!(
            mask_rules("verifyyou are human".into(), TEST_RULES),
            "verifyyou are human"
        );
    }
    #[test]
    fn whole_word() {
        assert_eq!(
            mask_rules("CAPTCHA reCAPTCHA g-recaptcha".into(), TEST_RULES),
            "check reCAPTCHA g-recaptcha"
        );
    }
    #[test]
    fn segments_across_nodes() {
        let segs: Vec<String> = ["Hi Verify", "you", "are human", "x"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            mask_segments_rules(&segs, TEST_RULES),
            ["Hi Complete the check", "", "", "x"]
        );
        let mid: Vec<String> = ["CAP", "TCHA", " x"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(mask_segments_rules(&mid, TEST_RULES), ["check", "", " x"]);
    }
    #[test]
    fn rules_are_fixed_points() {
        for &(phrase, _, _) in TEST_RULES {
            let once = mask_rules(format!("x {phrase} y"), TEST_RULES);
            assert_eq!(mask_rules(once.clone(), TEST_RULES), once);
        }
    }
}
