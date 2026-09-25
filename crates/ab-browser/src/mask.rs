//! Phrase rewriting: replaces configured phrases with a substitute (or
//! `[BLOCKED]`) in any page content returned to the caller (snapshot, text,
//! html, markdown, find, tool errors). Edit `RULES` below. Matching is
//! per-char simple-lowercase case-insensitive (no multi-char folds like
//! ß/SS). Empty = no-op.

pub const MASK: &str = "[BLOCKED]";

/// Boundary marker between separate text nodes when segments are joined
/// (see [`mask_segments`]). It is transparent inside words and counts as a
/// gap unit between words.
const SEP: char = '\u{1f}';

/// `(phrase, replacement, whole_word)`.
/// - `replacement`: text to put in place of the phrase; `""` = `[BLOCKED]`.
///   A replacement must not itself contain any phrase (masking is applied on
///   several layers, so it has to be a fixed point).
/// - `whole_word`: only match when not embedded in a longer word
///   (letters/digits/`_`), so `captcha` skips `recaptcha` / `g-recaptcha`.
type Rule = (&'static str, &'static str, bool);

const RULES: &[Rule] = &[
    // English
    ("Verify you are human", "Complete the check", false),
    ("Confirm you are human", "Complete the check", false),
    (
        "Press & Hold to confirm you are a human",
        "Please continue",
        false,
    ),
    ("Human verification", "Check", false),
    ("Please slide to verify", "Please continue", false),
    ("Slide to verify", "Please continue", false),
    ("I'm not a robot", "Continue", false),
    ("I\u{2019}m not a robot", "Continue", false),
    ("Are you a robot?", "Continue", false),
    (
        "Checking if the site connection is secure",
        "Please wait",
        false,
    ),
    ("CAPTCHA", "check", true),
    // Status messages
    ("Validation successful", "Check complete", false),
    ("Validation failed", "Check failed", false),
    ("Validation expired", "Check expired", false),
    // Widget class/id names (HTML attributes)
    ("capture-wrapper", "widget-wrapper", false),
    ("capture-box", "widget-box", false),
    ("verify-img-out", "widget-img-out", false),
    ("verify-img-panel", "widget-img-panel", false),
    ("verify-refresh", "widget-refresh", false),
    ("verify-bar-area", "widget-bar-area", false),
    ("verify-move-block", "widget-move-block", false),
    // Korean (no whole_word: particles attach directly, e.g. 캡차를)
    ("로봇이 아닙니다", "확인을 완료해 주세요", false),
    ("사람인지 확인", "확인 진행", false),
    ("보안 문자", "확인 문자", false),
    ("보안문자", "확인문자", false),
    ("자동 입력 방지", "확인", false),
    ("자동입력 방지", "확인", false),
    ("자동입력방지", "확인", false),
    ("슬라이드하여 인증", "계속 진행", false),
    ("캡차", "확인", false),
    ("캡챠", "확인", false),
];

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

/// Convenience: mask plain phrases (no whole-word) with `[BLOCKED]`.
pub fn mask_with(text: String, phrases: &[&'static str]) -> String {
    let rules: Vec<Rule> = phrases.iter().map(|&p| (p, "", false)).collect();
    mask_rules(text, &rules)
}

/// Rewrite phrases that may span several adjacent text nodes (e.g. the
/// accessibility tree splits `Verify <b>you</b> are human` into three
/// `StaticText` nodes). The segments are matched as one joined text; for each
/// match the first touched segment receives the replacement in place of its
/// part and the other touched segments lose theirs. Returns one string per
/// input segment.
pub fn mask_segments(segs: &[String]) -> Vec<String> {
    mask_segments_rules(segs, RULES)
}

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
    #[test]
    fn segments_split_across_nodes() {
        let segs: Vec<String> = ["Hello. Please verify: Verify", "you", "are human", "x"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let out = mask_segments_rules(&segs, &[("verify you are human", "", false)]);
        assert_eq!(out, ["Hello. Please verify: [BLOCKED]", "", "", "x"]);
    }
    #[test]
    fn segments_split_mid_word() {
        let segs: Vec<String> = ["CAP", "TCHA now"].iter().map(|s| s.to_string()).collect();
        let out = mask_segments_rules(&segs, &[("captcha", "", false)]);
        assert_eq!(out, ["[BLOCKED]", " now"]);
    }
    #[test]
    fn segments_without_match_unchanged() {
        let segs = vec!["a".to_string(), "b".to_string()];
        assert_eq!(mask_segments_rules(&segs, &[("zzz", "", false)]), segs);
    }
    #[test]
    fn replacement_and_whole_word() {
        let rules: &[Rule] = &[
            ("CAPTCHA", "check", true),
            ("Human verification", "Check", false),
        ];
        assert_eq!(
            mask_rules(
                "Solve the CAPTCHA. <div class=\"g-recaptcha\"> reCAPTCHA human  verification"
                    .into(),
                rules
            ),
            "Solve the check. <div class=\"g-recaptcha\"> reCAPTCHA Check"
        );
    }
    #[test]
    fn whole_word_across_nodes() {
        let segs: Vec<String> = ["CAP", "TCHA", " x"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            mask_segments_rules(&segs, &[("captcha", "check", true)]),
            ["check", "", " x"]
        );
    }
    #[test]
    fn rules_are_fixed_points() {
        // Re-masking already-masked output must not change it, for every rule.
        for &(phrase, _, _) in RULES {
            let once = mask(format!("x {phrase} y"));
            assert_eq!(
                mask(once.clone()),
                once,
                "rule {phrase:?} is not idempotent"
            );
        }
        let ids = mask(
            r#"<div class="capture-wrapper"><i id="verify-img-out"></i>Validation failed</div>"#
                .into(),
        );
        assert_eq!(
            ids,
            r#"<div class="widget-wrapper"><i id="widget-img-out"></i>Check failed</div>"#
        );
    }
    #[test]
    fn korean_rules_with_particles() {
        assert_eq!(
            mask("캡차를 풀고 보안문자를 입력하세요. 로봇이 아닙니다".into()),
            "확인를 풀고 확인문자를 입력하세요. 확인을 완료해 주세요"
        );
    }
}
