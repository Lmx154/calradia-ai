//! `sanitize_text` (protocol-v1.md, "sanitize_text") and its input-side variant.
//!
//! The engine splits response bodies on `|`, treats all-digit fields as integers, turns
//! `^` into newlines, expands `{...}` in strings and mangles non-ASCII, so every T the
//! server sends goes through `sanitize_text` first.

use crate::protocol::{MAX_TEXT, REASON_EMPTY_REPLY};

const ELLIPSIS: &str = "...";
/// Step 7 cuts at the last space at or before byte `MAX_TEXT - 3`, unless that space is
/// before this byte, in which case it hard-cuts.
const MIN_WORD_CUT: usize = 400;

#[derive(Clone, Copy, PartialEq)]
enum Side {
    Output,
    Input,
}

/// Steps 1-8 of protocol-v1.md: returns the text to send as T, or `Err("empty_reply")`
/// if nothing with a letter is left.
pub fn sanitize_text(raw: &str) -> Result<String, &'static str> {
    let text = cap(clean(raw, Side::Output));
    if text.bytes().any(|b| b.is_ascii_alphabetic()) {
        Ok(text)
    } else {
        Err(REASON_EMPTY_REPLY)
    }
}

/// Input sanitizing: the same steps, except that leftover non-ASCII becomes `?` and braces
/// are kept. Step 7 is not applied because it cannot change the outcome (a msg over
/// MAX_MSG is rejected either way and pname is cut to 32 characters, far below the cap),
/// and step 8 is not applied because it defines a job failure, not an input rule: a msg
/// like "42" or "?" is valid. The result is pure ASCII.
pub fn sanitize_input(raw: &str) -> String {
    clean(raw, Side::Input)
}

/// Steps 1-6. Steps 2-5 run as one pass over the characters. That equals running them in
/// sequence because transliterations only produce letters, quotes, `-`, `.` and spaces,
/// never a control character or one of `|{}^`.
fn clean(raw: &str, side: Side) -> String {
    let text = strip_think(raw);
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\0'..='\x1f' | '\x7f' => out.push(' '),
            '|' => out.push('/'),
            '^' => out.push(' '),
            '{' if side == Side::Output => out.push('('),
            '}' if side == Side::Output => out.push(')'),
            c if c.is_ascii() => out.push(c),
            c => match transliterate(c) {
                Some(t) => out.push_str(t),
                None if side == Side::Input => out.push('?'),
                None => {}
            },
        }
    }
    // Step 6. Only ASCII spaces are left as whitespace at this point.
    out.split_ascii_whitespace().collect::<Vec<_>>().join(" ")
}

/// Step 1: removes every `<think>...</think>` block, and an unclosed `<think>` through the
/// end of the text.
fn strip_think(s: &str) -> String {
    const OPEN: &str = "<think>";
    const CLOSE: &str = "</think>";
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find(OPEN) {
        out.push_str(&rest[..i]);
        let inner = &rest[i + OPEN.len()..];
        match inner.find(CLOSE) {
            Some(j) => rest = &inner[j + CLOSE.len()..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// Step 7: caps an ASCII text at `MAX_TEXT` bytes, ending in "...".
fn cap(text: String) -> String {
    debug_assert!(text.is_ascii());
    if text.len() <= MAX_TEXT {
        return text;
    }
    let limit = MAX_TEXT - ELLIPSIS.len();
    let cut = match text.as_bytes()[..=limit].iter().rposition(|&b| b == b' ') {
        Some(i) if i >= MIN_WORD_CUT => i,
        _ => limit,
    };
    format!("{}{ELLIPSIS}", &text[..cut])
}

/// Step 2: the fixed transliteration table for non-ASCII characters. `None` means the
/// character is dropped (output) or becomes `?` (input).
fn transliterate(c: char) -> Option<&'static str> {
    Some(match c {
        // Curly single and double quotes, including the low-9 and reversed forms.
        '\u{2018}'..='\u{201B}' => "'",
        '\u{201C}'..='\u{201F}' => "\"",
        // Hyphen, non-breaking hyphen, figure/en/em dash, horizontal bar, minus sign,
        // two- and three-em dash, small em dash, small and fullwidth hyphen-minus.
        '\u{2010}'..='\u{2015}'
        | '\u{2212}'
        | '\u{2E3A}'
        | '\u{2E3B}'
        | '\u{FE58}'
        | '\u{FE63}'
        | '\u{FF0D}' => "-",
        '\u{2026}' => "...",
        // Unicode White_Space outside ASCII: NEL, no-break and typographic spaces, line
        // and paragraph separators, ideographic space.
        c if c.is_whitespace() => " ",
        // The Latin-1 ordinal indicators are letters too. The micro sign (U+00B5), the only
        // other Latin-1 letter below U+00C0, is Greek and has no Latin base letter.
        '\u{AA}' => "a",
        '\u{BA}' => "o",
        '\u{C0}'..='\u{17F}' => match LATIN[c as usize - 0xC0] {
            "" => return None,
            base => base,
        },
        _ => return None,
    })
}

/// Base letters for U+00C0..=U+017F (the Latin-1 Supplement letters and Latin
/// Extended-A), one row per 16 code points. The two empty entries are U+00D7 (x) and
/// U+00F7 (division sign), which are not letters.
#[rustfmt::skip]
const LATIN: [&str; 0x180 - 0xC0] = [
    // U+00C0
    "A", "A", "A", "A", "A", "A", "AE", "C", "E", "E", "E", "E", "I", "I", "I", "I",
    // U+00D0
    "D", "N", "O", "O", "O", "O", "O", "", "O", "U", "U", "U", "U", "Y", "Th", "ss",
    // U+00E0
    "a", "a", "a", "a", "a", "a", "ae", "c", "e", "e", "e", "e", "i", "i", "i", "i",
    // U+00F0
    "d", "n", "o", "o", "o", "o", "o", "", "o", "u", "u", "u", "u", "y", "th", "y",
    // U+0100
    "A", "a", "A", "a", "A", "a", "C", "c", "C", "c", "C", "c", "C", "c", "D", "d",
    // U+0110
    "D", "d", "E", "e", "E", "e", "E", "e", "E", "e", "E", "e", "G", "g", "G", "g",
    // U+0120
    "G", "g", "G", "g", "H", "h", "H", "h", "I", "i", "I", "i", "I", "i", "I", "i",
    // U+0130
    "I", "i", "IJ", "ij", "J", "j", "K", "k", "k", "L", "l", "L", "l", "L", "l", "L",
    // U+0140
    "l", "L", "l", "N", "n", "N", "n", "N", "n", "n", "N", "n", "O", "o", "O", "o",
    // U+0150
    "O", "o", "OE", "oe", "R", "r", "R", "r", "R", "r", "S", "s", "S", "s", "S", "s",
    // U+0160
    "S", "s", "T", "t", "T", "t", "T", "t", "U", "u", "U", "u", "U", "u", "U", "u",
    // U+0170
    "U", "u", "U", "u", "W", "w", "Y", "y", "Y", "Z", "z", "Z", "z", "Z", "z", "s",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn out(s: &str) -> String {
        sanitize_text(s).unwrap()
    }

    #[test]
    fn strips_think_blocks() {
        assert_eq!(out("<think>plan</think>Aye."), "Aye.");
        assert_eq!(out("A <think>x</think>b<think>y</think> c"), "A b c");
        assert_eq!(out("Aye. <think>unclosed to the end"), "Aye.");
        assert_eq!(
            out("</think>stray close is kept"),
            "</think>stray close is kept"
        );
        assert_eq!(
            sanitize_text("<think>only thoughts</think>"),
            Err(REASON_EMPTY_REPLY)
        );
    }

    #[test]
    fn transliterates_table_entries() {
        assert_eq!(
            out("\u{201C}Caf\u{E9}\u{201D} \u{2014} it\u{2019}s na\u{EF}ve\u{2026} Stra\u{DF}e"),
            "\"Cafe\" - it's naive... Strasse"
        );
        assert_eq!(
            out("\u{C6}sir \u{E6}\u{153}\u{141}\u{F8}d\u{17E}"),
            "AEsir aeoeLodz"
        );
        assert_eq!(out("a\u{A0}b\u{2003}c\u{3000}d\u{2028}e"), "a b c d e");
        assert_eq!(out("x\u{2013}y\u{2212}z \u{2018}q\u{201A}"), "x-y-z 'q'");
        // Anything outside the table is dropped: emoji, CJK, symbols, zero-width space.
        assert_eq!(
            out("Ale \u{1F37A} \u{4F60}\u{597D} \u{A9}\u{D7}\u{B5} w\u{200B}ord"),
            "Ale word"
        );
    }

    #[test]
    fn latin_table_is_aligned() {
        for cp in 0xC0u32..0x180 {
            let c = char::from_u32(cp).unwrap();
            let base = LATIN[cp as usize - 0xC0];
            if !c.is_alphabetic() {
                assert_eq!(base, "", "U+{cp:04X} is not a letter");
                continue;
            }
            assert!(
                !base.is_empty() && base.bytes().all(|b| b.is_ascii_alphabetic()),
                "U+{cp:04X} -> {base:?}"
            );
            let first = base.chars().next().unwrap();
            assert_eq!(
                c.is_uppercase(),
                first.is_ascii_uppercase(),
                "case of U+{cp:04X} {c} -> {base:?}"
            );
            assert_eq!(transliterate(c), Some(base));
        }
    }

    #[test]
    fn replaces_controls_and_specials() {
        assert_eq!(
            out("a|b{c}d^e\tf\r\ng\u{0}h\u{7F}i\u{85}j"),
            "a/b(c)d e f g h i j"
        );
        assert_eq!(out("  lots \n\n of    space  "), "lots of space");
    }

    #[test]
    fn caps_at_word_boundary_or_hard_cut() {
        let exact = "a".repeat(MAX_TEXT);
        assert_eq!(out(&exact), exact);
        // Last space at 497: cut there.
        let s = format!("{} {}", "a".repeat(497), "b".repeat(50));
        assert_eq!(out(&s), format!("{}...", "a".repeat(497)));
        // Last space at or before 497 is at 450: cut there.
        let s = format!("{} {}", "a".repeat(450), "b".repeat(100));
        assert_eq!(out(&s), format!("{}...", "a".repeat(450)));
        // The only space is at 399, before 400: hard cut at 497.
        let s = format!("{} {}", "a".repeat(399), "b".repeat(200));
        let t = out(&s);
        assert_eq!(t.len(), MAX_TEXT);
        assert!(t.starts_with(&"a".repeat(399)) && t.ends_with("bbb..."));
        // The space at exactly 400 is accepted.
        let s = format!("{} {}", "a".repeat(400), "b".repeat(200));
        assert_eq!(out(&s), format!("{}...", "a".repeat(400)));
        // 501 bytes of words.
        let words = "word ".repeat(101);
        let t = out(&words);
        assert!(t.len() <= MAX_TEXT && t.ends_with("word..."), "{t}");
    }

    #[test]
    fn fails_without_a_letter() {
        for s in ["", "   ", "12345", "- 3 ... 4", "\u{1F37A}\u{4F60}", "|{}^"] {
            assert_eq!(sanitize_text(s), Err(REASON_EMPTY_REPLY), "{s:?}");
        }
        assert_eq!(out("7 x"), "7 x");
    }

    #[test]
    fn input_side_keeps_braces_and_marks_non_ascii() {
        assert_eq!(sanitize_input("{s0}|&%^+"), "{s0}/&% +");
        assert_eq!(sanitize_input("\u{E9}t\u{E9} \u{1F37A}!"), "ete ?!");
        assert_eq!(sanitize_input(" 42 "), "42");
        assert_eq!(sanitize_input("hi <think>x</think>there"), "hi there");
    }
}
