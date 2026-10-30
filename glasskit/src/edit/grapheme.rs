//! Grapheme-cluster boundaries over `&[char]`, so the caret, Backspace and Delete never cut a user-perceived character in half.
//!
//! This is an **approximation of UAX #29** (extended grapheme clusters) without the Unicode tables, good for the text a field holds:
//!
//! * a base character followed by combining marks (`e` + U+0301, Cyrillic `и` + U+0306, Arabic / Hebrew / Thai / Devanagari marks in the
//!   ranges listed in [`is_extend`]) is one cluster;
//! * ZWJ sequences (family emoji), variation selectors, skin-tone modifiers and tag sequences stay together;
//! * regional indicators pair up (a flag is one cluster, three letters are one plus a lone one);
//! * CR LF is one cluster; any other control character stands alone;
//! * Hangul jamo L V T sequences and precomposed LV / LVT syllables join.
//!
//! Not handled: Indic conjunct rules (a virama + consonant joins by UAX #29 GB9c), prepend characters, and spacing marks outside the common
//! ranges. A cluster too exotic for this table is split into at most a few pieces; nothing is lost or reordered.

/// Characters that never start a cluster (combining marks, joiners, variation selectors, emoji modifiers, tags).
pub fn is_extend(c: char) -> bool {
    matches!(c as u32,
        0x0300..=0x036F | 0x0483..=0x0489 | 0x0591..=0x05BD | 0x05BF | 0x05C1..=0x05C2 | 0x05C4..=0x05C5 | 0x05C7
        | 0x0610..=0x061A | 0x064B..=0x065F | 0x0670 | 0x06D6..=0x06DC | 0x06DF..=0x06E4 | 0x06E7..=0x06E8 | 0x06EA..=0x06ED
        | 0x0711 | 0x0730..=0x074A | 0x0900..=0x0902 | 0x093A | 0x093C | 0x0941..=0x0948 | 0x094D | 0x0951..=0x0957 | 0x0962..=0x0963
        | 0x0E31 | 0x0E34..=0x0E3A | 0x0E47..=0x0E4E | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x200C | 0x200D | 0x20D0..=0x20FF
        | 0x302A..=0x302F | 0x3099..=0x309A | 0xFE00..=0xFE0F | 0xFE20..=0xFE2F | 0x1F3FB..=0x1F3FF | 0xE0020..=0xE007F | 0xE0100..=0xE01EF)
}

/// Emoji-like symbols (Extended_Pictographic, abridged): what a ZWJ may glue to.
pub fn is_pictographic(c: char) -> bool {
    matches!(c as u32,
        0x00A9 | 0x00AE | 0x203C | 0x2049 | 0x2122 | 0x2139 | 0x2194..=0x21AA | 0x231A..=0x23FF | 0x24C2 | 0x25AA..=0x25FE
        | 0x2600..=0x27BF | 0x2934 | 0x2935 | 0x2B05..=0x2B55 | 0x3030 | 0x303D | 0x3297 | 0x3299 | 0x1F000..=0x1FAFF)
}

fn is_regional(c: char) -> bool {
    ('\u{1F1E6}'..='\u{1F1FF}').contains(&c)
}

#[derive(PartialEq, Clone, Copy)]
enum Hangul {
    None,
    L,
    V,
    T,
    LV,
    LVT,
}

fn hangul(c: char) -> Hangul {
    match c as u32 {
        0x1100..=0x115F | 0xA960..=0xA97C => Hangul::L,
        0x1160..=0x11A7 | 0xD7B0..=0xD7C6 => Hangul::V,
        0x11A8..=0x11FF | 0xD7CB..=0xD7FB => Hangul::T,
        u @ 0xAC00..=0xD7A3 => {
            if (u - 0xAC00) % 28 == 0 {
                Hangul::LV
            } else {
                Hangul::LVT
            }
        }
        _ => Hangul::None,
    }
}

/// Is there a cluster boundary between `chars[i - 1]` and `chars[i]`? (`0 < i < len`; the ends are always boundaries.)
pub fn is_boundary(chars: &[char], i: usize) -> bool {
    if i == 0 || i >= chars.len() {
        return true;
    }
    let (a, b) = (chars[i - 1], chars[i]);
    if a == '\r' && b == '\n' {
        return false;
    }
    if a.is_control() || b.is_control() {
        return true;
    }
    if is_extend(b) {
        return false;
    }
    if a == '\u{200D}' && is_pictographic(b) {
        return false;
    }
    if is_regional(a) && is_regional(b) {
        // pairs: boundary after an even run of regional indicators
        let run = chars[..i].iter().rev().take_while(|&&c| is_regional(c)).count();
        return run % 2 == 0;
    }
    match (hangul(a), hangul(b)) {
        (Hangul::L, Hangul::L | Hangul::V | Hangul::LV | Hangul::LVT) => false,
        (Hangul::V | Hangul::LV, Hangul::V | Hangul::T) => false,
        (Hangul::T | Hangul::LVT, Hangul::T) => false,
        _ => true,
    }
}

/// The boundary after `i` (`i < len`); `len` at the end.
pub fn next_boundary(chars: &[char], i: usize) -> usize {
    let n = chars.len();
    let mut j = (i + 1).min(n);
    while j < n && !is_boundary(chars, j) {
        j += 1;
    }
    j
}

/// The boundary before `i` (`i > 0`); 0 at the start.
pub fn prev_boundary(chars: &[char], i: usize) -> usize {
    let mut j = i.min(chars.len()).saturating_sub(1);
    while j > 0 && !is_boundary(chars, j) {
        j -= 1;
    }
    j
}

/// Snap `i` down to the start of the cluster that contains it.
pub fn snap(chars: &[char], i: usize) -> usize {
    let mut j = i.min(chars.len());
    while j > 0 && j < chars.len() && !is_boundary(chars, j) {
        j -= 1;
    }
    j
}

/// Number of clusters in `chars`.
pub fn count(chars: &[char]) -> usize {
    let (mut i, mut n) = (0, 0);
    while i < chars.len() {
        i = next_boundary(chars, i);
        n += 1;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cl(s: &str) -> Vec<String> {
        let c: Vec<char> = s.chars().collect();
        let (mut i, mut v) = (0, Vec::new());
        while i < c.len() {
            let j = next_boundary(&c, i);
            v.push(c[i..j].iter().collect());
            i = j;
        }
        v
    }

    #[test]
    fn plain_and_cyrillic_text_is_one_cluster_per_char() {
        assert_eq!(cl("abc"), ["a", "b", "c"]);
        assert_eq!(cl("Привет, мир"), ["П", "р", "и", "в", "е", "т", ",", " ", "м", "и", "р"]);
        assert_eq!(cl(""), Vec::<String>::new());
    }

    #[test]
    fn combining_marks_join_their_base() {
        assert_eq!(cl("e\u{301}x"), ["e\u{301}", "x"]);
        assert_eq!(cl("и\u{306}"), ["и\u{306}"], "Cyrillic short i as a base + breve");
        assert_eq!(cl("a\u{300}\u{301}\u{302}b"), ["a\u{300}\u{301}\u{302}", "b"]);
        assert_eq!(cl("\u{301}"), ["\u{301}"], "a stray mark is its own cluster");
    }

    #[test]
    fn emoji_sequences_stay_together() {
        let family = "👨\u{200D}👩\u{200D}👧";
        assert_eq!(cl(&format!("a{family}b")), ["a", family, "b"]);
        assert_eq!(cl("❤\u{FE0F}!"), ["❤\u{FE0F}", "!"]);
        assert_eq!(cl("👍🏽x"), ["👍🏽", "x"]);
        assert_eq!(cl("👍👍"), ["👍", "👍"]);
        // ZWJ then a non-pictographic letter does break
        assert_eq!(cl("a\u{200D}b"), ["a\u{200D}", "b"]);
    }

    #[test]
    fn flags_pair_regional_indicators() {
        assert_eq!(cl("🇷🇺🇺🇸"), ["🇷🇺", "🇺🇸"]);
        assert_eq!(cl("🇷🇺🇺"), ["🇷🇺", "🇺"]);
        assert_eq!(cl("🇷"), ["🇷"]);
    }

    #[test]
    fn crlf_and_controls() {
        assert_eq!(cl("a\r\nb"), ["a", "\r\n", "b"]);
        assert_eq!(cl("\n\n"), ["\n", "\n"]);
        assert_eq!(cl("\u{7}\u{301}"), ["\u{7}", "\u{301}"], "a mark after a control is its own cluster");
    }

    #[test]
    fn hangul_syllables_join() {
        // L V T conjoining jamo
        assert_eq!(cl("\u{1112}\u{1161}\u{11AB}"), ["\u{1112}\u{1161}\u{11AB}"]);
        // precomposed LV + T, and two syllables
        assert_eq!(cl("\u{AC00}\u{11A8}"), ["\u{AC00}\u{11A8}"]);
        assert_eq!(cl("한글"), ["한", "글"]);
    }

    #[test]
    fn navigation_and_snap() {
        let c: Vec<char> = "a👨\u{200D}👩b".chars().collect(); // a, man ZWJ woman, b
        assert_eq!(next_boundary(&c, 0), 1);
        assert_eq!(next_boundary(&c, 1), 4);
        assert_eq!(next_boundary(&c, 4), 5);
        assert_eq!(prev_boundary(&c, 5), 4);
        assert_eq!(prev_boundary(&c, 4), 1);
        assert_eq!(prev_boundary(&c, 1), 0);
        assert_eq!(prev_boundary(&c, 0), 0);
        assert_eq!(snap(&c, 3), 1);
        assert_eq!(snap(&c, 2), 1);
        assert_eq!(snap(&c, 4), 4);
        assert_eq!(count(&c), 3);
        assert!(is_boundary(&c, 0) && is_boundary(&c, 5) && !is_boundary(&c, 2));
    }
}
