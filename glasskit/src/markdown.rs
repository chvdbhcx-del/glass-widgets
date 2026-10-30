//! A small, SAFE markdown subset for chat answers: a port of Pill's `md-lite.js` plus a few extensions. Pure data in, pure data out: no
//! GPU, no Win32, no HTML. [`parse`] returns [`Block`]s of [`Inline`]s; [`plain`] strips the markers ("copy the answer"). The layout and
//! drawing of the blocks live in [`crate::richtext`].
//!
//! # Subset (identical to `md-lite.js`)
//! * paragraphs (a soft line break stays a `'\n'` inside the text), `-` / `*` / `+` bullet lists, `1.` / `1)` numbered lists
//! * inline `**bold**`, `*italic*`, `` `code` ``, `[text](url)` links
//! * fenced code blocks with an optional language (an unclosed fence runs to the end of the text)
//! * `#`..`######` headings (md-lite showed them as bold paragraphs; here they are [`Block::Heading`] with a level)
//! * indented lines under a list item continue that item
//! * the same quirks: a closing mark is looked for at most [`WINDOW`] chars ahead (so unmatched `*` stay linear on huge answers), `**x**`
//!   needs a non-space on both ends, `2 * 3`, `5*4 *x`, `snake_case_name` stay text
//!
//! # Extensions (not in md-lite)
//! * **nested lists**: a marker indented 2+ columns deeper than the item above nests under it ([`ListItem::children`]); md-lite flattened them
//! * **block quotes**: lines starting with `>` become [`Block::Quote`] (their content is parsed again, so quotes can hold lists or code)
//! * **horizontal rules**: a line of three or more `-`, `*` or `_` (spaces allowed) is [`Block::Rule`]
//! * **strikethrough**: `~~x~~` is [`Inline::Strike`] (same rules as `**`)
//! * numbered lists remember their first number ([`Block::List::start`])
//!
//! # Safety
//! Links are DATA: [`Link::url`] is kept verbatim and [`Link::is_safe`] says whether it may be opened (`http`, `https`, `mailto` only; never
//! `javascript:`, `file:`...). Nothing is interpreted as markup other than the subset above; HTML stays literal text.
//!
//! ```
//! use glasskit::markdown::{parse, plain, Block, Inline};
//! let b = parse("Hello **big** world\n\n- one\n- two");
//! assert!(matches!(b[0], Block::Paragraph(_)) && matches!(b[1], Block::List { .. }));
//! assert_eq!(plain("a **b** `c`"), "a b c");
//! ```

/// A closing mark is looked for this far ahead only (md-lite's `WINDOW`).
pub const WINDOW: usize = 500;

#[derive(Clone, Debug, PartialEq)]
pub enum Inline {
    Text(String),
    Bold(Vec<Inline>),
    Italic(Vec<Inline>),
    /// extension: `~~x~~`
    Strike(Vec<Inline>),
    Code(String),
    Link(Link),
}

/// `[text](url)`: the url is kept as written; ask [`Link::is_safe`] before opening it.
#[derive(Clone, Debug, PartialEq)]
pub struct Link {
    pub text: String,
    pub url: String,
}

impl Link {
    /// May this url be opened by the host? Only `http://`, `https://` and `mailto:` (case-insensitive).
    pub fn is_safe(&self) -> bool {
        is_safe_url(&self.url)
    }
}

pub fn is_safe_url(url: &str) -> bool {
    let u = url.trim().to_ascii_lowercase();
    u.starts_with("http://") || u.starts_with("https://") || u.starts_with("mailto:")
}

#[derive(Clone, Debug, PartialEq)]
pub struct ListItem {
    pub content: Vec<Inline>,
    /// extension: nested lists (and only lists) under this item
    pub children: Vec<Block>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Block {
    Paragraph(Vec<Inline>),
    Heading { level: u8, content: Vec<Inline> },
    List { ordered: bool, start: u32, items: Vec<ListItem> },
    Code { lang: String, text: String },
    Quote(Vec<Block>),
    Rule,
}

// ---------------------------------------------------------------- inline

fn is_ws(c: char) -> bool {
    c.is_whitespace()
}

/// Inline markup of one paragraph / list item / heading.
pub fn inline(s: &str) -> Vec<Inline> {
    let cs: Vec<char> = s.chars().collect();
    inline_chars(&cs)
}

fn find2(cs: &[char], from: usize, a: char) -> Option<usize> {
    // first index >= from where cs[i] == cs[i + 1] == a
    (from..cs.len().saturating_sub(1)).find(|&i| cs[i] == a && cs[i + 1] == a)
}

fn inline_chars(cs: &[char]) -> Vec<Inline> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut i = 0;
    let n = cs.len();
    macro_rules! flush {
        () => {
            if !buf.is_empty() {
                out.push(Inline::Text(std::mem::take(&mut buf)));
            }
        };
    }
    while i < n {
        let ch = cs[i];
        if ch == '`' {
            // the closing backtick must be within WINDOW and leave a non-empty code span
            let lim = (i + 1 + WINDOW).min(n);
            if let Some(j) = (i + 1..lim).find(|&j| cs[j] == '`') {
                if j > i + 1 {
                    flush!();
                    out.push(Inline::Code(cs[i + 1..j].iter().collect()));
                    i = j + 1;
                    continue;
                }
            }
        } else if (ch == '*' || ch == '~') && cs.get(i + 1) == Some(&ch) {
            let lim = (i + 2 + WINDOW).min(n);
            if let Some(j) = find2(&cs[..lim], i + 2, ch) {
                let inner = &cs[i + 2..j];
                if j > i + 2 && !is_ws(inner[0]) && !is_ws(inner[inner.len() - 1]) {
                    flush!();
                    let c = inline_chars(inner);
                    out.push(if ch == '*' { Inline::Bold(c) } else { Inline::Strike(c) });
                    i = j + 2;
                    continue;
                }
            }
        } else if ch == '*' && cs.get(i + 1).is_some_and(|&c| !is_ws(c) && c != '*') {
            let end = (i + 1 + WINDOW).min(n);
            let mut j = None;
            for k in i + 2..end {
                if cs[k] == '*' && cs[k - 1] != '*' && cs.get(k + 1) != Some(&'*') && !is_ws(cs[k - 1]) {
                    j = Some(k);
                    break;
                }
            }
            if let Some(j) = j {
                flush!();
                out.push(Inline::Italic(inline_chars(&cs[i + 1..j])));
                i = j + 1;
                continue;
            }
        } else if ch == '[' {
            if let Some((link, used)) = link_at(&cs[i..(i + 720).min(n)]) {
                flush!();
                out.push(Inline::Link(link));
                i += used;
                continue;
            }
        }
        buf.push(ch);
        i += 1;
    }
    flush!();
    out
}

/// `^\[([^\]\n]{1,200})\]\(([^)\s]{0,500})\)` on `cs`: the link and the chars it used.
fn link_at(cs: &[char]) -> Option<(Link, usize)> {
    let close = (1..cs.len().min(202)).find(|&k| cs[k] == ']' || cs[k] == '\n')?;
    if cs[close] != ']' || close < 2 || cs.get(close + 1) != Some(&'(') {
        return None;
    }
    let ustart = close + 2;
    let mut k = ustart;
    while k < cs.len() && k - ustart <= 500 && cs[k] != ')' && !is_ws(cs[k]) {
        k += 1;
    }
    if k >= cs.len() || cs[k] != ')' || k - ustart > 500 {
        return None;
    }
    Some((Link { text: cs[1..close].iter().collect(), url: cs[ustart..k].iter().collect() }, k + 1))
}

// ---------------------------------------------------------------- blocks

/// `^\s*(?:([-*+])|(\d{1,3})[.)])\s+(.*)$` -> (ordered, number, text)
fn list_marker(line: &str) -> Option<(usize, bool, u32, &str)> {
    let indent = indent_of(line);
    let t = line.trim_start();
    let mut it = t.char_indices();
    let (_, c0) = it.next()?;
    let (ordered, num, after) = if matches!(c0, '-' | '*' | '+') {
        (false, 0, c0.len_utf8())
    } else if c0.is_ascii_digit() {
        let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
        if digits > 3 {
            return None;
        }
        let d = t.as_bytes().get(digits).copied()?;
        if d != b'.' && d != b')' {
            return None;
        }
        (true, t[..digits].parse().ok()?, digits + 1)
    } else {
        return None;
    };
    let rest = &t[after..];
    let first = rest.chars().next()?;
    if !is_ws(first) {
        return None;
    }
    Some((indent, ordered, num, rest.trim_start_matches(is_ws)))
}

/// Leading columns of a line (a tab counts 4).
fn indent_of(line: &str) -> usize {
    line.chars().take_while(|c| is_ws(*c)).map(|c| if c == '\t' { 4 } else { 1 }).sum()
}

/// A fence line: the language after the backticks.
fn fence(line: &str) -> Option<String> {
    let t = line.trim_start_matches(is_ws).strip_prefix("```")?;
    let t = t.trim_start_matches(is_ws);
    Some(t.chars().take_while(|c| c.is_alphanumeric() || matches!(c, '_' | '+' | '#' | '.' | '-')).collect())
}

fn is_closing_fence(line: &str) -> bool {
    line.trim() == "```"
}

fn is_rule(line: &str) -> bool {
    if indent_of(line) > 3 {
        return false;
    }
    let t: Vec<char> = line.chars().filter(|c| !is_ws(*c)).collect();
    t.len() >= 3 && matches!(t[0], '-' | '*' | '_') && t.iter().all(|&c| c == t[0])
}

fn quote_body(line: &str) -> Option<&str> {
    if indent_of(line) > 3 {
        return None;
    }
    let t = line.trim_start().strip_prefix('>')?;
    Some(t.strip_prefix(' ').unwrap_or(t))
}

/// Open lists while parsing: (indent of the marker, ordered, start, items).
struct OpenList {
    indent: usize,
    ordered: bool,
    start: u32,
    items: Vec<ListItem>,
}

fn close_top(stack: &mut Vec<OpenList>, out: &mut Vec<Block>) {
    let Some(l) = stack.pop() else { return };
    let b = Block::List { ordered: l.ordered, start: l.start, items: l.items };
    match stack.last_mut().and_then(|p| p.items.last_mut()) {
        Some(item) => item.children.push(b),
        None => out.push(b),
    }
}

/// Markdown -> blocks.
pub fn parse(text: &str) -> Vec<Block> {
    let norm = text.replace("\r\n", "\n").replace('\r', "\n");
    let lines: Vec<&str> = norm.split('\n').collect();
    parse_lines(&lines)
}

fn parse_lines(lines: &[&str]) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut para: Vec<&str> = Vec::new();
    let mut lists: Vec<OpenList> = Vec::new();
    fn end_para(para: &mut Vec<&str>, blocks: &mut Vec<Block>) {
        if !para.is_empty() {
            blocks.push(Block::Paragraph(inline(&para.join("\n"))));
            para.clear();
        }
    }
    fn close_all(lists: &mut Vec<OpenList>, blocks: &mut Vec<Block>) {
        while !lists.is_empty() {
            close_top(lists, blocks);
        }
    }
    let mut n = 0;
    while n < lines.len() {
        let line = lines[n];
        if let Some(lang) = fence(line) {
            end_para(&mut para, &mut blocks);
            close_all(&mut lists, &mut blocks);
            let mut code = Vec::new();
            n += 1;
            while n < lines.len() && !is_closing_fence(lines[n]) {
                code.push(lines[n]);
                n += 1;
            }
            blocks.push(Block::Code { lang, text: code.join("\n") });
            n += 1;
            continue;
        }
        if line.trim().is_empty() {
            end_para(&mut para, &mut blocks);
            close_all(&mut lists, &mut blocks);
            n += 1;
            continue;
        }
        if quote_body(line).is_some() {
            end_para(&mut para, &mut blocks);
            close_all(&mut lists, &mut blocks);
            let mut body = Vec::new();
            while n < lines.len() {
                let Some(b) = quote_body(lines[n]) else { break };
                body.push(b);
                n += 1;
            }
            blocks.push(Block::Quote(parse_lines(&body)));
            continue;
        }
        if is_rule(line) {
            end_para(&mut para, &mut blocks);
            close_all(&mut lists, &mut blocks);
            blocks.push(Block::Rule);
            n += 1;
            continue;
        }
        if let Some((indent, ordered, num, rest)) = list_marker(line) {
            end_para(&mut para, &mut blocks);
            // dedent: close the lists that are deeper than this marker
            while lists.len() > 1 && indent < lists.last().map_or(0, |l| l.indent) {
                close_top(&mut lists, &mut blocks);
            }
            let deeper = lists.last().is_some_and(|l| indent >= l.indent + 2 && !l.items.is_empty());
            let same_kind = lists.last().is_some_and(|l| l.ordered == ordered);
            let mut fresh = deeper || lists.is_empty();
            if !fresh && !same_kind {
                close_top(&mut lists, &mut blocks); // another kind of list at the same level starts a new list (md-lite)
                fresh = true;
            }
            if fresh {
                lists.push(OpenList { indent, ordered, start: if ordered { num } else { 1 }, items: Vec::new() });
            }
            if let Some(l) = lists.last_mut() {
                l.items.push(ListItem { content: inline(rest), children: Vec::new() });
            }
            n += 1;
            continue;
        }
        if !lists.is_empty() && line.starts_with(is_ws) {
            // indented continuation of the last item
            if let Some(item) = lists.last_mut().and_then(|l| l.items.last_mut()) {
                item.content.extend(inline(&format!("\n{}", line.trim())));
            }
            n += 1;
            continue;
        }
        close_all(&mut lists, &mut blocks);
        let t = line.trim_start_matches('#');
        let hashes = line.len() - t.len();
        if (1..=6).contains(&hashes) && t.starts_with(is_ws) {
            end_para(&mut para, &mut blocks);
            blocks.push(Block::Heading { level: hashes as u8, content: inline(t.trim_start_matches(is_ws)) });
            n += 1;
            continue;
        }
        para.push(line);
        n += 1;
    }
    end_para(&mut para, &mut blocks);
    close_all(&mut lists, &mut blocks);
    blocks
}

// ---------------------------------------------------------------- plain text

/// Inline content without markers; links read `text (url)`.
pub fn inline_plain(c: &[Inline]) -> String {
    let mut s = String::new();
    for x in c {
        match x {
            Inline::Text(t) | Inline::Code(t) => s.push_str(t),
            Inline::Bold(c) | Inline::Italic(c) | Inline::Strike(c) => s.push_str(&inline_plain(c)),
            Inline::Link(l) => {
                if l.url.is_empty() {
                    s.push_str(&l.text)
                } else {
                    s.push_str(&format!("{} ({})", l.text, l.url))
                }
            }
        }
    }
    s
}

/// One block as plain text (markers stripped).
pub fn block_plain(b: &Block) -> String {
    match b {
        Block::Paragraph(c) | Block::Heading { content: c, .. } => inline_plain(c),
        Block::Code { text, .. } => text.clone(),
        Block::Rule => "---".to_string(),
        Block::Quote(bs) => blocks_plain(bs).lines().map(|l| format!("> {l}")).collect::<Vec<_>>().join("\n"),
        Block::List { ordered, start, items } => items
            .iter()
            .enumerate()
            .map(|(i, it)| {
                let mark = if *ordered { format!("{}. ", *start as usize + i) } else { "- ".to_string() };
                let mut s = mark + &inline_plain(&it.content);
                for ch in &it.children {
                    for l in block_plain(ch).lines() {
                        s.push_str("\n  ");
                        s.push_str(l);
                    }
                }
                s
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

pub fn blocks_plain(bs: &[Block]) -> String {
    bs.iter().map(block_plain).collect::<Vec<_>>().join("\n\n").trim().to_string()
}

/// The answer as plain text, markers stripped (for "copy the answer").
pub fn plain(text: &str) -> String {
    blocks_plain(&parse(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(c: &[Inline]) -> Vec<&'static str> {
        c.iter()
            .map(|x| match x {
                Inline::Text(_) => "text",
                Inline::Bold(_) => "b",
                Inline::Italic(_) => "i",
                Inline::Strike(_) => "s",
                Inline::Code(_) => "code",
                Inline::Link(_) => "link",
            })
            .collect()
    }
    fn bk(b: &[Block]) -> Vec<&'static str> {
        b.iter()
            .map(|x| match x {
                Block::Paragraph(_) => "p",
                Block::Heading { .. } => "h",
                Block::List { ordered: false, .. } => "ul",
                Block::List { ordered: true, .. } => "ol",
                Block::Code { .. } => "code",
                Block::Quote(_) => "quote",
                Block::Rule => "hr",
            })
            .collect()
    }
    fn items(b: &Block) -> usize {
        match b {
            Block::List { items, .. } => items.len(),
            _ => panic!("not a list"),
        }
    }
    fn para(b: &Block) -> &[Inline] {
        match b {
            Block::Paragraph(c) => c,
            _ => panic!("not a paragraph"),
        }
    }

    // ---- the assertions of tools/test-md-lite.js, one by one

    #[test]
    fn paragraphs_lists_bold_italic_code() {
        let b = parse("Hello **big** *world* and `x*y`\nsecond line\n\n- one\n* two **b**\n\n1. a\n2) b\n3. c");
        assert_eq!(bk(&b), ["p", "ul", "ol"]);
        let c = para(&b[0]);
        assert_eq!(kinds(c), ["text", "b", "text", "i", "text", "code", "text"]);
        assert_eq!(c[5], Inline::Code("x*y".into()), "markers inside code stay literal");
        match &c[4] {
            Inline::Text(s) => assert!(s.starts_with(" and ") && !s.contains("\nsecond line")),
            _ => panic!(),
        }
        assert_eq!(c[6], Inline::Text("\nsecond line".into()), "the soft break stays a newline in the text");
        assert_eq!(items(&b[1]), 2);
        assert_eq!(items(&b[2]), 3);
    }

    #[test]
    fn math_like_stars_and_unmatched_marks_stay_text() {
        assert_eq!(plain("2 * 3 = 6 and 5*4 *x"), "2 * 3 = 6 and 5*4 *x");
        assert_eq!(plain("**unclosed bold"), "**unclosed bold");
        assert_eq!(plain("snake_case_name"), "snake_case_name");
    }

    #[test]
    fn fenced_code_is_verbatim_and_unclosed_fences_run_to_the_end() {
        let b = parse("before\n```js\nlet a = 1; // **not bold**\n\n- not a list\n```\nafter");
        assert_eq!(bk(&b), ["p", "code", "p"]);
        assert_eq!(b[1], Block::Code { lang: "js".into(), text: "let a = 1; // **not bold**\n\n- not a list".into() });
        match parse("```\nopen").last().unwrap() {
            Block::Code { text, .. } => assert_eq!(text, "open"),
            _ => panic!(),
        }
    }

    #[test]
    fn headings_and_bold_lead_in() {
        // md-lite: `## Title` is a bold paragraph; here a heading whose plain text is the same
        match &parse("## Title")[0] {
            Block::Heading { level, content } => {
                assert_eq!(*level, 2);
                assert_eq!(content, &vec![Inline::Text("Title".into())]);
            }
            b => panic!("{b:?}"),
        }
        assert_eq!(plain("## Title"), "Title");
        assert_eq!(bk(&parse("**Note:** hi")), ["p"], "`**x** y` at line start is a paragraph, not a bullet");
    }

    #[test]
    fn links_are_data_and_plain_shows_the_url() {
        assert_eq!(plain("see [docs](https://e.com/x) now"), "see docs (https://e.com/x) now");
        let b = parse("[a](https://e.com) [b](javascript:alert(1)) [c](mailto:x@y.z) [d](file:///c:/x) [e]()");
        let links: Vec<&Link> = para(&b[0]).iter().filter_map(|i| if let Inline::Link(l) = i { Some(l) } else { None }).collect();
        assert_eq!(links.len(), 5);
        assert_eq!(links.iter().map(|l| l.is_safe()).collect::<Vec<_>>(), [true, false, true, false, false]);
    }

    #[test]
    fn plain_strips_markers() {
        assert_eq!(plain("# T\n\n**a** *b* `c`\n\n- x\n- y\n\n1. p\n2. q\n\n```\ncode\n```"), "T\n\na b c\n\n- x\n- y\n\n1. p\n2. q\n\ncode");
        assert_eq!(plain(""), "");
    }

    #[test]
    fn pathological_input_stays_fast() {
        let t0 = std::time::Instant::now();
        let s = format!("{}{}{}{}", "*a ".repeat(30000), "**".repeat(30000), "[".repeat(30000), "`".repeat(30000));
        let _ = parse(&s);
        assert!(t0.elapsed().as_secs_f32() < 2.0, "slow on pathological input: {:?}", t0.elapsed());
    }

    #[test]
    fn hostile_text_stays_text() {
        // the DOM test of md-lite (no a / img / script elements, no innerHTML) becomes: only the known node kinds, markup stays literal
        let hostile = "<script>alert(1)</script> <img src=x onerror=1> [click](javascript:alert(1)) **<b>x</b>**\n```\n<i>raw</i>\n```";
        let b = parse(hostile);
        assert_eq!(bk(&b), ["p", "code"]);
        let c = para(&b[0]);
        let mut texts = String::new();
        fn walk(c: &[Inline], texts: &mut String) {
            for x in c {
                match x {
                    Inline::Text(t) | Inline::Code(t) => texts.push_str(t),
                    Inline::Bold(c) | Inline::Italic(c) | Inline::Strike(c) => walk(c, texts),
                    Inline::Link(l) => {
                        assert!(!l.is_safe(), "javascript: is never safe");
                        texts.push_str(&l.text);
                    }
                }
            }
        }
        walk(c, &mut texts);
        assert!(texts.contains("<script>alert(1)</script>") && texts.contains("<img src=x onerror=1>") && texts.contains("<b>x</b>"));
        assert!(c.iter().any(|x| matches!(x, Inline::Bold(b) if inline_plain(b) == "<b>x</b>")));
        assert_eq!(b[1], Block::Code { lang: String::new(), text: "<i>raw</i>".into() });
    }

    // ---- extensions and edge cases

    #[test]
    fn nested_lists_and_numbering() {
        let b = parse("- a\n  - a1\n  - a2\n    1. deep\n- b\n\n3. x\n4. y");
        assert_eq!(bk(&b), ["ul", "ol"]);
        match &b[0] {
            Block::List { items, .. } => {
                assert_eq!(items.len(), 2);
                assert_eq!(items[0].children.len(), 1);
                match &items[0].children[0] {
                    Block::List { items: sub, ordered: false, .. } => {
                        assert_eq!(sub.len(), 2);
                        assert!(matches!(&sub[1].children[0], Block::List { ordered: true, items, .. } if items.len() == 1));
                    }
                    b => panic!("{b:?}"),
                }
                assert!(items[1].children.is_empty());
            }
            _ => panic!(),
        }
        assert!(matches!(&b[1], Block::List { ordered: true, start: 3, .. }));
        assert_eq!(plain("3. x\n4. y"), "3. x\n4. y");
        assert_eq!(plain("- a\n  - b"), "- a\n  - b");
    }

    #[test]
    fn indented_continuation_joins_the_last_item() {
        let b = parse("- one\n  more text\n- two");
        assert_eq!(items(&b[0]), 2);
        match &b[0] {
            Block::List { items, .. } => assert_eq!(inline_plain(&items[0].content), "one\nmore text"),
            _ => panic!(),
        }
    }

    #[test]
    fn quotes_rules_and_strike() {
        let b = parse("> quoted **b**\n> - item\n\n---\n\n* * *\n\n~~gone~~ and ~~ not~~");
        assert_eq!(bk(&b), ["quote", "hr", "hr", "p"]);
        match &b[0] {
            Block::Quote(inner) => assert_eq!(bk(inner), ["p", "ul"]),
            _ => panic!(),
        }
        assert_eq!(kinds(para(&b[3])), ["s", "text"]);
        assert_eq!(plain("> a\n> b"), "> a\n> b");
        assert_eq!(bk(&parse("a\n---")), ["p", "hr"]);
        assert_eq!(bk(&parse("2 * 3 = 6")), ["p"]);
    }

    #[test]
    fn headings_of_every_level() {
        for l in 1..=6u8 {
            let s = format!("{} H{l}", "#".repeat(l as usize));
            assert!(matches!(&parse(&s)[0], Block::Heading { level, .. } if *level == l));
        }
        assert_eq!(bk(&parse("####### seven")), ["p"]);
        assert_eq!(bk(&parse("#nospace")), ["p"]);
    }

    #[test]
    fn unicode_and_crlf() {
        let b = parse("Привет, **мир** 😀\r\n\r\n- пункт\r\n- ещё");
        assert_eq!(bk(&b), ["p", "ul"]);
        assert_eq!(plain("Привет, **мир** 😀"), "Привет, мир 😀");
        assert_eq!(kinds(para(&b[0])), ["text", "b", "text"]);
    }

    #[test]
    fn code_fence_language_and_empty_code() {
        assert!(matches!(&parse("```c++\nint x;\n```")[0], Block::Code { lang, .. } if lang == "c++"));
        assert!(matches!(&parse("```\n```")[0], Block::Code { text, .. } if text.is_empty()));
        // empty backticks stay text
        assert_eq!(plain("a `` b"), "a `` b");
    }
}
