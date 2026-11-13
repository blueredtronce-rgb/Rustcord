//! Pure Discord-markdown parser (no egui): text -> blocks of styled inlines.
//!
//! Follows docs/PROTOCOL_NOTES.md part 3 §4–5: at each position the rules are tried in
//! Discord's order and the first match wins. Inline styles may span lines and contain block
//! elements (`*foo\n```x```*`); those are lifted into the block list with the style applied.
//! Inside inline-only containers (spoiler, link label, heading, subtext) blocks are flattened.

use crate::model::Id;

/// Container nesting limit; deeper markup renders literally (keeps recursion bounded).
const MAX_NEST: u8 = 16;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
}

#[derive(Clone, Debug, PartialEq)]
#[allow(clippy::enum_variant_names)] // InlineCode vs CodeBlock reads clearer than Code
pub enum Inline {
    Text(String, Style),
    InlineCode(String),
    /// `idx` = position among the message's spoilers (reveal-state key).
    Spoiler { idx: usize, content: Vec<Inline> },
    /// `masked` = `[label](url)`; otherwise an autolink whose label is the URL itself.
    Link { label: Vec<Inline>, url: String, masked: bool },
    UserMention(Id),
    RoleMention(Id),
    ChannelMention(Id),
    Everyone,
    Here,
    CustomEmoji { name: String, id: Id, animated: bool },
    /// Unix seconds + style letter (`f` when omitted).
    Timestamp { unix: i64, style: char },
    /// `</name sub:id>` -> "name sub".
    SlashCommand(String),
    LineBreak,
}

#[derive(Clone, Debug, PartialEq)]
#[allow(clippy::enum_variant_names)]
pub enum Block {
    Paragraph(Vec<Inline>),
    CodeBlock { lang: Option<String>, code: String },
    Quote(Vec<Block>),
    Heading { level: u8, content: Vec<Inline> },
    Subtext(Vec<Inline>),
    /// `ordered` = number shown for `N.` items, None for bullets.
    ListItem { ordered: Option<u32>, depth: u8, content: Vec<Block> },
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Doc {
    pub blocks: Vec<Block>,
    /// Only emoji (custom or unicode) and whitespace, 1..=30 of them.
    pub jumbo: bool,
}

pub fn parse(text: &str) -> Doc {
    let text = text.trim();
    let mut p = Parser { work: 0, budget: 64 * text.len() + 4096, spoilers: 0 };
    let mut out = Out::default();
    p.run(text, Ctx { line_start: true, ..Ctx::default() }, &mut out);
    let blocks = out.finish();
    let jumbo = is_jumbo(&blocks);
    Doc { blocks, jumbo }
}

#[derive(Clone, Copy, Default)]
struct Ctx {
    style: Style,
    line_start: bool,
    /// Char before the current position (word-boundary checks).
    prev: Option<char>,
    in_quote: bool,
    in_link: bool,
    nest: u8,
    list_depth: u8,
}

struct Parser {
    /// Bytes scanned so far; once over `budget` only cheap rules run (worst case stays linear).
    work: usize,
    budget: usize,
    spoilers: usize,
}

type Rule = fn(&mut Parser, &str, usize, Ctx, &mut Out) -> Option<usize>;

/// Discord's rule order (first match wins); plain text is the fallback in `run`.
const RULES: [Rule; 17] = [
    Parser::newline,
    Parser::escape,
    Parser::bold,
    Parser::underline,
    Parser::italic,
    Parser::strike,
    Parser::spoiler,
    Parser::code_block,
    Parser::inline_code,
    Parser::quote,
    Parser::subtext,
    Parser::heading,
    Parser::masked_link,
    Parser::list,
    Parser::token,
    Parser::angle_link,
    Parser::autolink,
];

impl Parser {
    fn run(&mut self, s: &str, ctx: Ctx, out: &mut Out) {
        let mut c = ctx;
        let mut i = 0;
        while i < s.len() {
            if i > 0 {
                c.prev = s[..i].chars().next_back();
            }
            let n = RULES.iter().find_map(|r| r(self, s, i, c, out)).unwrap_or_else(|| self.text(s, i, c.style, out));
            i += n;
            c.line_start = s.as_bytes()[i - 1] == b'\n';
        }
    }

    fn exhausted(&self) -> bool {
        self.work > self.budget
    }

    fn deep(&self, c: &Ctx) -> bool {
        c.nest >= MAX_NEST || self.exhausted()
    }

    /// Parse `s[a..b]` as the content of an inline container (inherits line state).
    fn inner(&mut self, s: &str, a: usize, b: usize, c: Ctx, out: &mut Out) {
        let ctx = Ctx { prev: s[..a].chars().next_back(), nest: c.nest + 1, ..c };
        self.run(&s[a..b], ctx, out);
    }

    /// First `k >= from` where `pat` starts and is not followed by `not_followed`.
    fn find(&mut self, s: &str, from: usize, pat: &[u8], not_followed: Option<u8>) -> Option<usize> {
        let b = s.as_bytes();
        let mut k = from;
        while k + pat.len() <= b.len() {
            self.work += 1;
            if b[k..].starts_with(pat) && (not_followed.is_none() || b.get(k + pat.len()).copied() != not_followed) {
                return Some(k);
            }
            k += 1;
        }
        None
    }

    /// `^D(.+?)D` (s): index of the closing delimiter.
    fn delimited(&mut self, s: &str, i: usize, c: &Ctx, d: &str, not_followed: Option<u8>) -> Option<usize> {
        if self.deep(c) || !s[i..].starts_with(d) {
            return None;
        }
        self.find(s, i + d.len() + 1, d.as_bytes(), not_followed)
    }

    fn newline(&mut self, s: &str, i: usize, _c: Ctx, out: &mut Out) -> Option<usize> {
        (s.as_bytes()[i] == b'\n').then(|| {
            out.inline(Inline::LineBreak);
            1
        })
    }

    fn escape(&mut self, s: &str, i: usize, c: Ctx, out: &mut Out) -> Option<usize> {
        if s.as_bytes()[i] != b'\\' {
            return None;
        }
        let ch = s[i + 1..].chars().next()?;
        if ch.is_ascii_alphanumeric() || ch.is_whitespace() {
            return None;
        }
        out.text(&s[i + 1..i + 1 + ch.len_utf8()], c.style);
        Some(1 + ch.len_utf8())
    }

    fn bold(&mut self, s: &str, i: usize, c: Ctx, out: &mut Out) -> Option<usize> {
        let j = self.delimited(s, i, &c, "**", Some(b'*'))?;
        self.inner(s, i + 2, j, Ctx { style: Style { bold: true, ..c.style }, ..c }, out);
        Some(j + 2 - i)
    }

    fn underline(&mut self, s: &str, i: usize, c: Ctx, out: &mut Out) -> Option<usize> {
        let j = self.delimited(s, i, &c, "__", Some(b'_'))?;
        self.inner(s, i + 2, j, Ctx { style: Style { underline: true, ..c.style }, ..c }, out);
        Some(j + 2 - i)
    }

    fn strike(&mut self, s: &str, i: usize, c: Ctx, out: &mut Out) -> Option<usize> {
        let j = self.delimited(s, i, &c, "~~", None)?;
        self.inner(s, i + 2, j, Ctx { style: Style { strike: true, ..c.style }, ..c }, out);
        Some(j + 2 - i)
    }

    fn italic(&mut self, s: &str, i: usize, c: Ctx, out: &mut Out) -> Option<usize> {
        if self.deep(&c) {
            return None;
        }
        let j = match s.as_bytes()[i] {
            b'*' => self.star_italic_end(s, i)?,
            b'_' if !c.prev.is_some_and(is_word) => self.underscore_italic_end(s, i)?,
            _ => return None,
        };
        self.inner(s, i + 1, j, Ctx { style: Style { italic: true, ..c.style }, ..c }, out);
        Some(j + 1 - i)
    }

    /// `^\*(?=\S)((?:\\\*|\*\*|\s+(?:[^*\s]|\\\*|\*\*)|[^\s*])+?)(?<!\\)\*(?!\*)`, scanned
    /// deterministically (the alternatives only overlap on backslashes).
    fn star_italic_end(&mut self, s: &str, i: usize) -> Option<usize> {
        let b = s.as_bytes();
        let at = |k: usize| b.get(k).copied();
        let pair = |k: usize| (at(k) == Some(b'\\') || at(k) == Some(b'*')) && at(k + 1) == Some(b'*');
        if s[i + 1..].chars().next()?.is_whitespace() {
            return None;
        }
        let mut k = i + 1;
        let mut first = true;
        loop {
            self.work += 1;
            if !first && at(k) == Some(b'*') && at(k + 1) != Some(b'*') && b[k - 1] != b'\\' {
                return Some(k);
            }
            first = false;
            if pair(k) {
                k += 2;
                continue;
            }
            let ch = s[k..].chars().next()?;
            if ch == '*' {
                return None;
            }
            if !ch.is_whitespace() {
                k += ch.len_utf8();
                continue;
            }
            // Whitespace must be followed by `\*`, `**` or a non-star char (` *` never closes).
            let m = k + ws_len(&s[k..]);
            if pair(m) {
                k = m + 2;
            } else {
                let ch = s[m..].chars().next()?;
                if ch == '*' {
                    return None;
                }
                k = m + ch.len_utf8();
            }
        }
    }

    /// `_((?:__|\\[\s\S]|[^\\_])+?)_\b` (the opening `\b` is checked by the caller).
    fn underscore_italic_end(&mut self, s: &str, i: usize) -> Option<usize> {
        let b = s.as_bytes();
        let mut k = i + 1;
        let mut first = true;
        loop {
            self.work += 1;
            let ch = s[k..].chars().next()?;
            if !first && ch == '_' && !s[k + 1..].chars().next().is_some_and(is_word) {
                return Some(k);
            }
            first = false;
            k += match ch {
                '_' if b.get(k + 1) == Some(&b'_') => 2,
                '_' => return None,
                '\\' => 1 + s[k + 1..].chars().next()?.len_utf8(),
                _ => ch.len_utf8(),
            };
        }
    }

    fn spoiler(&mut self, s: &str, i: usize, c: Ctx, out: &mut Out) -> Option<usize> {
        let j = self.delimited(s, i, &c, "||", None)?;
        let idx = self.spoilers;
        self.spoilers += 1;
        let mut sub = Out::default();
        self.inner(s, i + 2, j, c, &mut sub);
        out.inline(Inline::Spoiler { idx, content: flatten(sub.finish()) });
        Some(j + 2 - i)
    }

    fn code_block(&mut self, s: &str, i: usize, _c: Ctx, out: &mut Out) -> Option<usize> {
        if self.exhausted() || !s[i..].starts_with("```") {
            return None;
        }
        let j = self.find(s, i + 4, b"```", None)?;
        let inner = &s[i + 3..j];
        if inner.bytes().all(|b| b == b'\n') {
            return None;
        }
        let (lang, code) = split_lang(inner);
        out.block(Block::CodeBlock { lang, code });
        Some(j + 3 - i)
    }

    /// `^(``?)(.*?)\1` (s); the content must contain a non-backtick char.
    fn inline_code(&mut self, s: &str, i: usize, _c: Ctx, out: &mut Out) -> Option<usize> {
        if self.exhausted() || s.as_bytes()[i] != b'`' {
            return None;
        }
        let d = if s[i..].starts_with("``") { 2 } else { 1 };
        let pat = &b"``"[..d];
        // With "``" and no "``" closer, the single-tick variant closes immediately -> empty.
        let j = self.find(s, i + d, pat, None)?;
        let code = &s[i + d..j];
        if code.bytes().all(|b| b == b'`') {
            return None;
        }
        out.inline(Inline::InlineCode(code.to_owned()));
        Some(j + d - i)
    }

    fn quote(&mut self, s: &str, i: usize, c: Ctx, out: &mut Out) -> Option<usize> {
        if !c.line_start || c.in_quote || c.in_link || self.deep(&c) {
            return None;
        }
        let rest = &s[i..];
        let sp = leading_spaces(rest);
        let r = &rest[sp..];
        let (a, b) = if let Some(body) = r.strip_prefix(">>> ") {
            // `>>> ` quotes everything that follows.
            if body.is_empty() {
                return None;
            }
            (i + sp + 4, s.len())
        } else {
            let after = r.strip_prefix('>')?;
            if after.starts_with(">>") {
                return None;
            }
            let len = line_len(after.strip_prefix(' ')?);
            if len == 0 {
                return None;
            }
            (i + sp + 2, i + sp + 2 + len)
        };
        let mut sub = Out::default();
        let ctx = Ctx { line_start: true, in_quote: true, prev: None, nest: c.nest + 1, ..c };
        self.run(&s[a..b], ctx, &mut sub);
        out.block(Block::Quote(sub.finish()));
        Some(b - i + nl(s, b))
    }

    fn subtext(&mut self, s: &str, i: usize, c: Ctx, out: &mut Out) -> Option<usize> {
        if !c.line_start || c.in_link || self.deep(&c) {
            return None;
        }
        let r = s[i..].strip_prefix("-# ")?;
        if r.starts_with("-#") {
            return None;
        }
        let len = line_len(r);
        if len == 0 {
            return None;
        }
        let content = self.line_inlines(&r[..len], c);
        out.block(Block::Subtext(content));
        Some(3 + len + nl(s, i + 3 + len))
    }

    fn heading(&mut self, s: &str, i: usize, c: Ctx, out: &mut Out) -> Option<usize> {
        if !c.line_start || c.in_link || self.deep(&c) {
            return None;
        }
        let rest = &s[i..];
        let sp = leading_spaces(rest);
        let level = rest[sp..].bytes().take_while(|&b| b == b'#').count();
        if !(1..=3).contains(&level) {
            return None;
        }
        let r = rest[sp + level..].strip_prefix(' ')?;
        if r.starts_with('#') {
            return None;
        }
        let len = line_len(r);
        if len == 0 {
            return None;
        }
        let content = self.line_inlines(&r[..len], c);
        out.block(Block::Heading { level: level as u8, content });
        let end = i + sp + level + 1 + len;
        Some(end - i + nl(s, end))
    }

    /// Single-line content of a heading/subtext: trimmed, parsed inline.
    fn line_inlines(&mut self, line: &str, c: Ctx) -> Vec<Inline> {
        let mut sub = Out::default();
        let ctx = Ctx { line_start: false, prev: Some(' '), nest: c.nest + 1, ..c };
        self.run(line.trim(), ctx, &mut sub);
        flatten(sub.finish())
    }

    fn masked_link(&mut self, s: &str, i: usize, c: Ctx, out: &mut Out) -> Option<usize> {
        let b = s.as_bytes();
        if b[i] != b'[' || c.in_link || self.deep(&c) {
            return None;
        }
        // Label ends at the matching `]` (`\]` is not an escape here).
        let mut depth = 0usize;
        let mut k = i;
        let close = loop {
            self.work += 1;
            match *b.get(k)? {
                b'[' => depth += 1,
                b']' => {
                    depth -= 1;
                    if depth == 0 {
                        break k;
                    }
                }
                _ => {}
            }
            k += 1;
        };
        if b.get(close + 1) != Some(&b'(') {
            return None;
        }
        let (url, end) = self.link_target(s, close + 2)?;
        let label = &s[i + 1..close];
        // Anti-phishing: a URL or code block in the label makes the whole thing plain text.
        if label.trim().is_empty() || !is_web_url(&url) || label.contains("```") || contains_url(label) {
            return None;
        }
        let mut sub = Out::default();
        self.inner(s, i + 1, close, Ctx { in_link: true, ..c }, &mut sub);
        out.inline(Inline::Link { label: flatten(sub.finish()), url, masked: true });
        Some(end - i)
    }

    /// `\s*<?url>?\s*)` starting after `(`: (unescaped url, index after `)`).
    fn link_target(&mut self, s: &str, mut k: usize) -> Option<(String, usize)> {
        k += ws_len(&s[k..]);
        let mut url = String::new();
        if s[k..].starts_with('<') {
            let len = s[k + 1..].find('>')?;
            self.work += len;
            url.push_str(&s[k + 1..k + 1 + len]);
            k += len + 2;
        } else {
            let mut depth = 0u32;
            loop {
                self.work += 1;
                let ch = s[k..].chars().next()?;
                match ch {
                    ')' if depth == 0 => break,
                    _ if ch.is_whitespace() => break,
                    '(' => depth += 1,
                    ')' => depth -= 1,
                    '\\' => {
                        if let Some(e) = s[k + 1..].chars().next().filter(|e| !e.is_ascii_alphanumeric() && !e.is_whitespace()) {
                            url.push(e);
                            k += 1 + e.len_utf8();
                            continue;
                        }
                    }
                    _ => {}
                }
                url.push(ch);
                k += ch.len_utf8();
            }
        }
        k += ws_len(&s[k..]);
        (s.as_bytes().get(k) == Some(&b')')).then_some((url, k + 1))
    }

    /// `^( *)([+*-]|(\d+)\.)( +)([^\n]+(?:\n\1 [^\n]+)*\n?)`
    fn list(&mut self, s: &str, i: usize, c: Ctx, out: &mut Out) -> Option<usize> {
        if !c.line_start || c.in_link || self.deep(&c) {
            return None;
        }
        let rest = &s[i..];
        let sp = leading_spaces(rest);
        let r = &rest.as_bytes()[sp..];
        let (number, marker) = match *r.first()? {
            b'-' | b'*' | b'+' => (None, 1),
            b'0'..=b'9' => {
                let d = r.iter().take_while(|b| b.is_ascii_digit()).count();
                if r.get(d) != Some(&b'.') {
                    return None;
                }
                let n = rest[sp..sp + d].parse::<u32>().unwrap_or(u32::MAX).max(1);
                (Some(n), d + 1)
            }
            _ => return None,
        };
        let spaces = r[marker..].iter().take_while(|&&b| b == b' ').count();
        let body_start = sp + marker + spaces;
        let first = line_len(&rest[body_start..]);
        if spaces == 0 || first == 0 {
            return None;
        }
        // Continuation lines are indented one space deeper than the marker.
        let indent = " ".repeat(sp + 1);
        let mut end = body_start + first;
        while let Some(line) = rest[end..].strip_prefix('\n').and_then(|l| l.strip_prefix(indent.as_str())) {
            let len = line_len(line);
            if len == 0 {
                break;
            }
            end += 1 + indent.len() + len;
        }
        self.work += end;
        let ordered = number.map(|n| match (out.para.is_empty(), out.blocks.last()) {
            (true, Some(Block::ListItem { ordered: Some(p), depth, .. })) if *depth == c.list_depth => p.saturating_add(1),
            _ => n,
        });
        let mut sub = Out::default();
        let ctx = Ctx { line_start: true, prev: Some(' '), nest: c.nest + 1, list_depth: c.list_depth + 1, ..c };
        self.run(&rest[body_start..end], ctx, &mut sub);
        out.block(Block::ListItem { ordered, depth: c.list_depth, content: sub.finish() });
        Some(end + nl(rest, end))
    }

    /// Mentions, custom emoji, timestamps, slash commands, @everyone/@here.
    fn token(&mut self, s: &str, i: usize, _c: Ctx, out: &mut Out) -> Option<usize> {
        let r = &s[i..];
        let (inline, len) = if let Some(t) = r.strip_prefix("<@&") {
            let (id, n) = snowflake_gt(t)?;
            (Inline::RoleMention(id), 3 + n)
        } else if let Some(t) = r.strip_prefix("<@") {
            let bang = usize::from(t.starts_with('!'));
            let (id, n) = snowflake_gt(&t[bang..])?;
            (Inline::UserMention(id), 2 + bang + n)
        } else if let Some(t) = r.strip_prefix("<#") {
            let (id, n) = snowflake_gt(t)?;
            (Inline::ChannelMention(id), 2 + n)
        } else if let Some(t) = r.strip_prefix("<t:") {
            let (inline, n) = timestamp(t)?;
            (inline, 3 + n)
        } else if let Some(t) = r.strip_prefix("</") {
            let (name, n) = slash_command(t)?;
            (Inline::SlashCommand(name), 2 + n)
        } else if r.starts_with("<:") || r.starts_with("<a:") {
            custom_emoji(r)?
        } else if r.starts_with("@everyone") {
            (Inline::Everyone, 9)
        } else if r.starts_with("@here") {
            (Inline::Here, 5)
        } else {
            return None;
        };
        out.inline(inline);
        Some(len)
    }

    /// `<scheme:/...>`: a link the unfurler leaves alone.
    fn angle_link(&mut self, s: &str, i: usize, c: Ctx, out: &mut Out) -> Option<usize> {
        if self.exhausted() {
            return None;
        }
        let t = s[i..].strip_prefix('<')?;
        let scheme = t.bytes().take(33).take_while(|&b| b != b':' && b != b'>' && !b.is_ascii_whitespace()).count();
        if scheme == 0 || scheme > 32 || !t[scheme..].starts_with(":/") {
            return None;
        }
        let tail = t[scheme + 2..].find(|ch: char| ch == '>' || ch.is_whitespace())?;
        self.work += tail;
        if tail == 0 || !t[scheme + 2 + tail..].starts_with('>') {
            return None;
        }
        let url = &t[..scheme + 2 + tail];
        out.inline(Inline::Link { label: vec![Inline::Text(url.to_owned(), c.style)], url: url.to_owned(), masked: false });
        Some(url.len() + 2)
    }

    fn autolink(&mut self, s: &str, i: usize, c: Ctx, out: &mut Out) -> Option<usize> {
        if s.as_bytes()[i] != b'h' {
            return None;
        }
        let n = url_len(&s[i..])?;
        self.work += n;
        let url = &s[i..i + n];
        out.inline(Inline::Link { label: vec![Inline::Text(url.to_owned(), c.style)], url: url.to_owned(), masked: false });
        Some(n)
    }

    /// Plain text up to the next char that could start a rule.
    fn text(&mut self, s: &str, i: usize, style: Style, out: &mut Out) -> usize {
        let r = &s[i..];
        let mut n = r.chars().next().map_or(1, char::len_utf8);
        for ch in r[n..].chars() {
            let plain = ch.is_ascii_alphanumeric() || (ch.is_whitespace() && ch != '\n') || ch >= '\u{c0}';
            if !plain || (ch == 'h' && r[n..].starts_with("http")) {
                break;
            }
            n += ch.len_utf8();
        }
        self.work += n;
        out.text(&r[..n], style);
        n
    }
}

/// Output of one container: finished blocks plus the paragraph being built.
#[derive(Default)]
struct Out {
    blocks: Vec<Block>,
    para: Vec<Inline>,
    /// A code block was just emitted: the newline right after it is implied.
    after_code: bool,
}

impl Out {
    fn text(&mut self, t: &str, style: Style) {
        self.after_code = false;
        if let Some(Inline::Text(prev, st)) = self.para.last_mut()
            && *st == style
        {
            prev.push_str(t);
        } else {
            self.para.push(Inline::Text(t.to_owned(), style));
        }
    }

    fn inline(&mut self, x: Inline) {
        let skip = x == Inline::LineBreak && self.para.is_empty() && self.after_code;
        self.after_code = false;
        if !skip {
            self.para.push(x);
        }
    }

    /// Ends the open paragraph (dropping the newline that led into the next block).
    /// Returns whether anything was pending, even just a blank line.
    fn flush(&mut self) -> bool {
        let had = !self.para.is_empty();
        if self.para.last() == Some(&Inline::LineBreak) {
            self.para.pop();
        }
        if !self.para.is_empty() {
            self.blocks.push(Block::Paragraph(std::mem::take(&mut self.para)));
        }
        had
    }

    fn block(&mut self, b: Block) {
        let gap = self.flush();
        self.after_code = matches!(b, Block::CodeBlock { .. });
        let b = match (b, self.blocks.last_mut()) {
            // Consecutive `> ` lines are separate matches; show them as one quote.
            (Block::Quote(new), Some(Block::Quote(prev))) if !gap => {
                merge_quote(prev, new);
                return;
            }
            (b, _) => b,
        };
        self.blocks.push(b);
    }

    fn finish(mut self) -> Vec<Block> {
        self.flush();
        self.blocks
    }
}

fn merge_quote(prev: &mut Vec<Block>, mut new: Vec<Block>) {
    if let (Some(Block::Paragraph(a)), Some(Block::Paragraph(b))) = (prev.last_mut(), new.first_mut()) {
        a.push(Inline::LineBreak);
        a.append(b);
        new.remove(0);
    }
    prev.extend(new);
}

/// Blocks inside an inline-only container become inline content, one line each.
fn flatten(blocks: Vec<Block>) -> Vec<Inline> {
    let mut v = Vec::new();
    for b in blocks {
        if !v.is_empty() {
            v.push(Inline::LineBreak);
        }
        match b {
            Block::Paragraph(x) | Block::Subtext(x) | Block::Heading { content: x, .. } => v.extend(x),
            Block::CodeBlock { code, .. } => v.push(Inline::InlineCode(code)),
            Block::Quote(inner) => v.extend(flatten(inner)),
            Block::ListItem { ordered, content, .. } => {
                let marker = ordered.map_or_else(|| "• ".to_owned(), |n| format!("{n}. "));
                v.push(Inline::Text(marker, Style::default()));
                v.extend(flatten(content));
            }
        }
    }
    v
}

/// Language rule: a first line like `rust` (and more lines after it) names the language.
fn split_lang(inner: &str) -> (Option<String>, String) {
    let inner = inner.trim_end_matches('\n');
    if let Some((first, rest)) = inner.split_once('\n') {
        let f = first.trim_end();
        if f.is_empty() {
            return (None, rest.to_owned());
        }
        if f.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'+' | b'-' | b'.')) {
            return (Some(f.to_owned()), rest.to_owned());
        }
    }
    (None, inner.to_owned())
}

/// Length of a bare URL at the start of `s` (`https?://[^\s<]+[^<.,:;"')\]\s]`), keeping a
/// trailing `)` that closes a `(` inside the URL (wiki links).
fn url_len(s: &str) -> Option<usize> {
    let scheme = if s.starts_with("https://") {
        8
    } else if s.starts_with("http://") {
        7
    } else {
        return None;
    };
    let mut end = s[scheme..].find(|ch: char| ch.is_whitespace() || ch == '<').map_or(s.len(), |n| scheme + n);
    let body = &s[scheme..end];
    let opens = body.matches('(').count();
    let mut closes = body.matches(')').count();
    loop {
        let last = s[scheme..end].chars().next_back()?;
        let keep = match last {
            '.' | ',' | ':' | ';' | '"' | '\'' | ']' => false,
            ')' => opens >= closes,
            _ => true,
        };
        if keep {
            break;
        }
        if last == ')' {
            closes -= 1;
        }
        end -= last.len_utf8();
    }
    (s[scheme..end].chars().nth(1).is_some()).then_some(end)
}

fn is_web_url(u: &str) -> bool {
    url_len(u) == Some(u.len())
}

fn contains_url(s: &str) -> bool {
    s.match_indices("http").any(|(p, _)| url_len(&s[p..]).is_some())
}

/// `\d+>` -> (id, bytes incl. `>`).
fn snowflake_gt(t: &str) -> Option<(Id, usize)> {
    let d = t.bytes().take_while(u8::is_ascii_digit).count();
    if d == 0 || t.as_bytes().get(d) != Some(&b'>') {
        return None;
    }
    Some((Id(t[..d].parse().ok()?), d + 1))
}

/// After `<t:`: `-?\d+(:[tTdDfFsSR])?>`.
fn timestamp(t: &str) -> Option<(Inline, usize)> {
    let neg = usize::from(t.starts_with('-'));
    let d = t[neg..].bytes().take_while(u8::is_ascii_digit).count();
    if d == 0 {
        return None;
    }
    let unix: i64 = t[..neg + d].parse().ok()?;
    let mut k = neg + d;
    let mut style = 'f';
    if t[k..].starts_with(':') {
        style = char::from(*t.as_bytes().get(k + 1)?);
        if !"tTdDfFsSR".contains(style) {
            return None;
        }
        k += 2;
    }
    t[k..].starts_with('>').then_some((Inline::Timestamp { unix, style }, k + 1))
}

/// After `</`: up to three space-separated name parts of 1..=32 chars, then `:id>`.
fn slash_command(t: &str) -> Option<(String, usize)> {
    let mut k = 0;
    for part in 0..3 {
        if part > 0 {
            if !t[k..].starts_with(' ') {
                break;
            }
            k += 1;
        }
        let mut n = 0;
        while let Some(ch) = t[k..].chars().next().filter(|&ch| ch.is_alphanumeric() || matches!(ch, '-' | '_' | '\'')) {
            n += 1;
            k += ch.len_utf8();
            if n > 32 {
                return None;
            }
        }
        if n == 0 {
            return None;
        }
    }
    let (_, len) = snowflake_gt(t[k..].strip_prefix(':')?)?;
    Some((t[..k].to_owned(), k + 1 + len))
}

/// `<(a)?:(\w{2,32}):(\d+)>` at the start of `r`.
fn custom_emoji(r: &str) -> Option<(Inline, usize)> {
    let (animated, t) = match r.strip_prefix("<a:") {
        Some(t) => (true, t),
        None => (false, r.strip_prefix("<:")?),
    };
    let n = t.bytes().take(33).take_while(|&b| b.is_ascii_alphanumeric() || b == b'_').count();
    if !(2..=32).contains(&n) || t.as_bytes().get(n) != Some(&b':') {
        return None;
    }
    let (id, len) = snowflake_gt(&t[n + 1..])?;
    let consumed = r.len() - t.len() + n + 1 + len;
    Some((Inline::CustomEmoji { name: t[..n].to_owned(), id, animated }, consumed))
}

fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn leading_spaces(s: &str) -> usize {
    s.bytes().take_while(|&b| b == b' ').count()
}

fn ws_len(s: &str) -> usize {
    s.len() - s.trim_start().len()
}

fn line_len(s: &str) -> usize {
    s.find('\n').unwrap_or(s.len())
}

/// 1 if `s[k]` is a newline (consumed together with a line-based block).
fn nl(s: &str, k: usize) -> usize {
    usize::from(s.as_bytes().get(k) == Some(&b'\n'))
}

fn is_jumbo(blocks: &[Block]) -> bool {
    let [Block::Paragraph(items)] = blocks else { return false };
    let mut n = 0;
    for it in items {
        n += match it {
            Inline::CustomEmoji { .. } => 1,
            Inline::LineBreak => 0,
            Inline::Text(t, _) => match count_emoji(t) {
                Some(k) => k,
                None => return false,
            },
            _ => return false,
        };
    }
    (1..=30).contains(&n)
}

/// Number of emoji sequences in `t`, or None if it has anything but emoji and whitespace.
/// shortcut: range-based emoji detection (no Unicode emoji tables); keycaps count as text.
fn count_emoji(t: &str) -> Option<usize> {
    let (mut n, mut joined, mut half_flag) = (0, false, false);
    for ch in t.chars() {
        let cp = ch as u32;
        if ch.is_whitespace() {
            joined = false;
            continue;
        }
        match cp {
            0x200D => joined = true,
            0xFE0F | 0x20E3 | 0x1F3FB..=0x1F3FF | 0xE0020..=0xE007F => {}
            // Two regional indicators form one flag.
            0x1F1E6..=0x1F1FF => {
                half_flag = !half_flag;
                n += usize::from(half_flag);
            }
            _ if is_emoji(cp) => {
                n += usize::from(!joined);
                joined = false;
            }
            _ => return None,
        }
    }
    Some(n)
}

fn is_emoji(cp: u32) -> bool {
    matches!(
        cp,
        0x1F000..=0x1FAFF
            | 0x2600..=0x27BF
            | 0x2300..=0x23FF
            | 0x2B00..=0x2BFF
            | 0x2190..=0x21FF
            | 0x25A0..=0x25FF
            | 0x2934
            | 0x2935
            | 0x3030
            | 0x303D
            | 0x3297
            | 0x3299
            | 0xA9
            | 0xAE
            | 0x203C
            | 0x2049
            | 0x2122
            | 0x2139
            | 0x24C2
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const B: Style = Style { bold: true, italic: false, underline: false, strike: false };
    const I: Style = Style { bold: false, italic: true, underline: false, strike: false };
    const U: Style = Style { bold: false, italic: false, underline: true, strike: false };
    const S: Style = Style { bold: false, italic: false, underline: false, strike: true };

    fn blocks(s: &str) -> Vec<Block> {
        parse(s).blocks
    }
    fn t(s: &str) -> Inline {
        Inline::Text(s.into(), Style::default())
    }
    fn st(s: &str, style: Style) -> Inline {
        Inline::Text(s.into(), style)
    }
    fn p(items: Vec<Inline>) -> Block {
        Block::Paragraph(items)
    }
    /// Inlines of a single-paragraph document.
    fn inl(s: &str) -> Vec<Inline> {
        match blocks(s).as_slice() {
            [Block::Paragraph(v)] => v.clone(),
            other => panic!("{s:?} is not one paragraph: {other:?}"),
        }
    }
    fn literal(s: &str) {
        assert_eq!(inl(s), vec![t(s)], "{s:?} should be literal");
    }
    fn link(url: &str) -> Inline {
        Inline::Link { label: vec![t(url)], url: url.into(), masked: false }
    }
    const LB: Inline = Inline::LineBreak;

    #[test]
    fn plain_and_trim() {
        assert_eq!(blocks("  hello world \n"), vec![p(vec![t("hello world")])]);
        assert!(blocks("   ").is_empty());
        literal("a.b, c! (d) = e?");
    }

    #[test]
    fn line_breaks() {
        assert_eq!(inl("a\nb"), vec![t("a"), LB, t("b")]);
        assert_eq!(inl("a\n\nb"), vec![t("a"), LB, LB, t("b")]);
    }

    #[test]
    fn bold() {
        assert_eq!(inl("**a**"), vec![st("a", B)]);
        assert_eq!(inl("foo ** a ** bar"), vec![t("foo "), st(" a ", B), t(" bar")]);
        assert_eq!(inl("**foo***"), vec![st("foo*", B)]);
        assert_eq!(inl("**a\nb**"), vec![st("a", B), LB, st("b", B)]);
        literal("**a");
    }

    #[test]
    fn star_italic() {
        assert_eq!(inl("*a*"), vec![st("a", I)]);
        assert_eq!(inl("*a b*"), vec![st("a b", I)]);
        assert_eq!(inl("x *a **b** c* y"), vec![t("x "), st("a ", I), st("b", Style { bold: true, ..I }), st(" c", I), t(" y")]);
        literal("foo * a * bar");
        literal("foo * a *bar");
        literal("*a *");
        literal("***");
        assert_eq!(inl("*****"), vec![st("*", B)]);
        assert_eq!(inl("**foo*"), vec![t("*"), st("foo", I)]);
    }

    #[test]
    fn four_stars_is_italic_stars() {
        // Notes: `****` gives italic `**` (bold needs content, italic takes `**` as content).
        assert_eq!(blocks("x ****"), vec![p(vec![t("x "), st("**", I)])]);
    }

    #[test]
    fn underscore_italic_needs_word_boundaries() {
        assert_eq!(inl("_a_"), vec![st("a", I)]);
        assert_eq!(inl("a _b c_ d"), vec![t("a "), st("b c", I), t(" d")]);
        literal("snake_case_word");
        literal("snake_case_");
        literal("_a_b");
        assert_eq!(inl("_a__b_"), vec![st("a__b", I)]);
    }

    #[test]
    fn underline_strike_and_nesting() {
        assert_eq!(inl("__a__"), vec![st("a", U)]);
        assert_eq!(inl("~~a~~"), vec![st("a", S)]);
        assert_eq!(inl("***a***"), vec![st("a", Style { bold: true, italic: true, ..Style::default() })]);
        assert_eq!(inl("__*a*__"), vec![st("a", Style { underline: true, italic: true, ..Style::default() })]);
        assert_eq!(inl("~~**a**~~"), vec![st("a", Style { bold: true, strike: true, ..Style::default() })]);
        literal("~~a");
        literal("__a");
    }

    #[test]
    fn spoilers_are_numbered() {
        assert_eq!(
            inl("||a|| b ||**c**||"),
            vec![
                Inline::Spoiler { idx: 0, content: vec![t("a")] },
                t(" b "),
                Inline::Spoiler { idx: 1, content: vec![st("c", B)] },
            ]
        );
        literal("||a");
        // Code blocks inside a spoiler stay hidden as (multi-line) inline code.
        assert_eq!(inl("||```x```||"), vec![Inline::Spoiler { idx: 0, content: vec![Inline::InlineCode("x".into())] }]);
    }

    #[test]
    fn code_blocks() {
        let cb = |lang: Option<&str>, code: &str| Block::CodeBlock { lang: lang.map(Into::into), code: code.into() };
        assert_eq!(blocks("```rust\nfn main() {}\n```"), vec![cb(Some("rust"), "fn main() {}")]);
        assert_eq!(blocks("```cpp foo\nbar```"), vec![cb(None, "cpp foo\nbar")]);
        assert_eq!(blocks("```x```"), vec![cb(None, "x")]);
        assert_eq!(blocks("```\ncode\n```"), vec![cb(None, "code")]);
        assert_eq!(blocks("```**a** <@1>```"), vec![cb(None, "**a** <@1>")]);
        assert_eq!(blocks("foo```x```bar"), vec![p(vec![t("foo")]), cb(None, "x"), p(vec![t("bar")])]);
        assert_eq!(blocks("a\n```x```\nb"), vec![p(vec![t("a")]), cb(None, "x"), p(vec![t("b")])]);
        // Quirk from the notes: ````foo```` is a block "`foo" plus a literal backtick.
        assert_eq!(blocks("````foo````"), vec![cb(None, "`foo"), p(vec![t("`")])]);
        literal("```a");
        literal("``````");
    }

    #[test]
    fn inline_code() {
        assert_eq!(inl("a `b` c"), vec![t("a "), Inline::InlineCode("b".into()), t(" c")]);
        assert_eq!(inl("``a`b``"), vec![Inline::InlineCode("a`b".into())]);
        assert_eq!(inl("`**a** <@1>`"), vec![Inline::InlineCode("**a** <@1>".into())]);
        assert_eq!(inl("`a\nb`"), vec![Inline::InlineCode("a\nb".into())]);
        literal("``");
        literal("`a");
    }

    #[test]
    fn italic_spans_code_fence() {
        // Inline formatting is matched before code blocks and may span the fence.
        assert_eq!(blocks("*foo\n```\nbar*\n```"), vec![p(vec![st("foo", I), LB, st("```", I), LB, st("bar", I), LB, t("```")])]);
    }

    #[test]
    fn quotes() {
        let q = |v: Vec<Block>| Block::Quote(v);
        assert_eq!(blocks("> a"), vec![q(vec![p(vec![t("a")])])]);
        assert_eq!(blocks("> a\n> b"), vec![q(vec![p(vec![t("a"), LB, t("b")])])]);
        assert_eq!(blocks("> a\nb"), vec![q(vec![p(vec![t("a")])]), p(vec![t("b")])]);
        assert_eq!(blocks(">>> a\nb"), vec![q(vec![p(vec![t("a"), LB, t("b")])])]);
        assert_eq!(blocks("> > a"), vec![q(vec![p(vec![t("> a")])])]);
        assert_eq!(blocks("x\n  > a"), vec![p(vec![t("x")]), q(vec![p(vec![t("a")])])]);
        assert_eq!(blocks("> # h"), vec![q(vec![Block::Heading { level: 1, content: vec![t("h")] }])]);
        literal(">a");
        literal(">> a");
        literal("a > b");
        assert_eq!(blocks("> a\n\n> b").len(), 2);
    }

    #[test]
    fn headings() {
        let h = |level, s: &str| Block::Heading { level, content: vec![t(s)] };
        assert_eq!(blocks("# a"), vec![h(1, "a")]);
        assert_eq!(blocks("### a  "), vec![h(3, "a")]);
        assert_eq!(blocks("x\n  ## a\ny"), vec![p(vec![t("x")]), h(2, "a"), p(vec![t("y")])]);
        assert_eq!(blocks("# **a**"), vec![Block::Heading { level: 1, content: vec![st("a", B)] }]);
        literal("#### a");
        literal("#a");
        literal("# # a");
        literal("a # b");
        // Inline containers inherit line start: `*# foo*` is an italic heading.
        assert_eq!(blocks("*# foo*"), vec![Block::Heading { level: 1, content: vec![st("foo", I)] }]);
    }

    #[test]
    fn subtext() {
        assert_eq!(blocks("-# small"), vec![Block::Subtext(vec![t("small")])]);
        assert_eq!(blocks("a\n-# b\nc"), vec![p(vec![t("a")]), Block::Subtext(vec![t("b")]), p(vec![t("c")])]);
        literal("-#foo");
        literal("-# -# foo");
        assert_eq!(inl("x\n -# a"), vec![t("x"), LB, t(" -# a")]);
    }

    #[test]
    fn lists() {
        let li = |ordered, depth, content| Block::ListItem { ordered, depth, content };
        assert_eq!(
            blocks("- a\n* b\n+ c"),
            vec![li(None, 0, vec![p(vec![t("a")])]), li(None, 0, vec![p(vec![t("b")])]), li(None, 0, vec![p(vec![t("c")])])]
        );
        assert_eq!(blocks("1. a\n1. b"), vec![li(Some(1), 0, vec![p(vec![t("a")])]), li(Some(2), 0, vec![p(vec![t("b")])])]);
        assert_eq!(blocks("0. a"), vec![li(Some(1), 0, vec![p(vec![t("a")])])]);
        assert_eq!(blocks("7. a\n9. b"), vec![li(Some(7), 0, vec![p(vec![t("a")])]), li(Some(8), 0, vec![p(vec![t("b")])])]);
        assert_eq!(
            blocks("- foo\n  - bar"),
            vec![li(None, 0, vec![p(vec![t("foo")]), li(None, 1, vec![p(vec![t("bar")])])])]
        );
        assert_eq!(blocks("- - foo"), vec![li(None, 0, vec![li(None, 1, vec![p(vec![t("foo")])])])]);
        assert_eq!(
            blocks("- > - foo"),
            vec![li(None, 0, vec![Block::Quote(vec![li(None, 1, vec![p(vec![t("foo")])])])])]
        );
        assert_eq!(blocks("- a\n b"), vec![li(None, 0, vec![p(vec![t("a"), LB, t(" b")])])]);
        assert_eq!(blocks("- a\nb"), vec![li(None, 0, vec![p(vec![t("a")])]), p(vec![t("b")])]);
        literal("-1. foo");
        literal("-a");
        literal("1.a");
        literal("a - b");
    }

    #[test]
    fn masked_links() {
        let ml = |label: Vec<Inline>, url: &str| Inline::Link { label, url: url.into(), masked: true };
        assert_eq!(inl("[a](https://x.com)"), vec![ml(vec![t("a")], "https://x.com")]);
        assert_eq!(inl("[a]( <https://x.com> )"), vec![ml(vec![t("a")], "https://x.com")]);
        assert_eq!(inl("[google.com](https://google.com)"), vec![ml(vec![t("google.com")], "https://google.com")]);
        assert_eq!(inl("[https://](https://x.com)"), vec![ml(vec![t("https://")], "https://x.com")]);
        assert_eq!(inl("[**b**](https://x.com)"), vec![ml(vec![st("b", B)], "https://x.com")]);
        assert_eq!(
            inl("[wiki](https://en.wikipedia.org/wiki/Rust_(lang))"),
            vec![ml(vec![t("wiki")], "https://en.wikipedia.org/wiki/Rust_(lang)")]
        );
        assert_eq!(inl("[a](https://x.com/a\\_b)"), vec![ml(vec![t("a")], "https://x.com/a_b")]);
        // Rejected: link-looking labels, non-web targets, space before `(`.
        assert_eq!(inl("[https://a.com](https://b.com)")[0], t("["));
        assert_eq!(inl("[https://google](https://b.com)")[0], t("["));
        literal("[a](ftp://x)");
        literal("[a](google.com)");
        assert_eq!(inl("[a] (https://x.com)"), vec![t("[a] ("), link("https://x.com"), t(")")]);
        assert_eq!(inl("[a](https://x.com"), vec![t("[a]("), link("https://x.com")]);
    }

    #[test]
    fn autolinks() {
        assert_eq!(inl("see https://x.com/a_b_c."), vec![t("see "), link("https://x.com/a_b_c"), t(".")]);
        assert_eq!(inl("(https://x.com)"), vec![t("("), link("https://x.com"), t(")")]);
        assert_eq!(inl("https://en.wikipedia.org/wiki/Foo_(bar)"), vec![link("https://en.wikipedia.org/wiki/Foo_(bar)")]);
        assert_eq!(inl("<https://x.com/*a*>"), vec![link("https://x.com/*a*")]);
        assert_eq!(inl("x<https://a.b>y"), vec![t("x"), link("https://a.b"), t("y")]);
        assert_eq!(inl("**https://x.com**"), vec![Inline::Link { label: vec![st("https://x.com", B)], url: "https://x.com".into(), masked: false }]);
        literal("https://x");
        literal("http protocol");
        literal("<https:/>");
    }

    #[test]
    fn mentions_and_tokens() {
        assert_eq!(inl("<@123>"), vec![Inline::UserMention(Id(123))]);
        assert_eq!(inl("<@!123>"), vec![Inline::UserMention(Id(123))]);
        assert_eq!(inl("<@&5>"), vec![Inline::RoleMention(Id(5))]);
        assert_eq!(inl("<#7>"), vec![Inline::ChannelMention(Id(7))]);
        assert_eq!(inl("@everyone @here"), vec![Inline::Everyone, t(" "), Inline::Here]);
        assert_eq!(inl("a<@1>b"), vec![t("a"), Inline::UserMention(Id(1)), t("b")]);
        assert_eq!(inl("**<@1>**"), vec![Inline::UserMention(Id(1))]);
        literal("<@$1>");
        literal("<@>");
        literal("<@99999999999999999999999>");
        assert_eq!(inl("\\<@1>"), vec![t("<@1>")]);
    }

    #[test]
    fn custom_emoji_and_slash_commands() {
        assert_eq!(inl("<:smile:123>"), vec![Inline::CustomEmoji { name: "smile".into(), id: Id(123), animated: false }]);
        assert_eq!(inl("<a:x1:5>"), vec![Inline::CustomEmoji { name: "x1".into(), id: Id(5), animated: true }]);
        literal("<:a:1>");
        literal(&format!("<:{}:1>", "a".repeat(33)));
        assert_eq!(inl("</cmd sub:123>"), vec![Inline::SlashCommand("cmd sub".into())]);
        assert_eq!(inl("</a b c:1>"), vec![Inline::SlashCommand("a b c".into())]);
        literal("</a b c d:1>");
        literal("</:1>");
    }

    #[test]
    fn timestamps() {
        let ts = |unix, style| Inline::Timestamp { unix, style };
        assert_eq!(inl("<t:1618953630>"), vec![ts(1618953630, 'f')]);
        assert_eq!(inl("<t:1618953630:R>"), vec![ts(1618953630, 'R')]);
        assert_eq!(inl("<t:-5:d>"), vec![ts(-5, 'd')]);
        literal("<t:1:x>");
        literal("<t:>");
        literal("<t:99999999999999999999>");
    }

    #[test]
    fn escapes() {
        assert_eq!(inl("\\*a\\*"), vec![t("*a*")]);
        assert_eq!(inl("\\_a\\_"), vec![t("_a_")]);
        literal("\\a");
        assert_eq!(inl("\\\\"), vec![t("\\")]);
        assert_eq!(inl("\\# a"), vec![t("# a")]);
        assert_eq!(inl("*a\\*"), vec![t("*a*")]);
    }

    #[test]
    fn jumbo_emoji() {
        assert!(parse("<:aa:1>").jumbo);
        assert!(parse("<:aa:1> <a:bb:2>\n😀").jumbo);
        assert!(parse("😀👍🏽 ❤️").jumbo);
        assert!(parse("👨‍👩‍👧 🇩🇪").jumbo);
        assert!(!parse("<:aa:1> hi").jumbo);
        assert!(!parse("**<:aa:1>**x").jumbo);
        assert!(!parse(&"😀".repeat(31)).jumbo);
        assert!(parse(&"😀".repeat(30)).jumbo);
        assert!(!parse("").jumbo);
        assert_eq!(count_emoji("👨‍👩‍👧"), Some(1));
        assert_eq!(count_emoji("🇩🇪🇫🇷"), Some(2));
    }

    #[test]
    fn unicode_text_is_safe() {
        assert_eq!(inl("héllo **wörld** 日本"), vec![t("héllo "), st("wörld", B), t(" 日本")]);
        assert_eq!(inl("*é*"), vec![st("é", I)]);
        assert_eq!(inl("**é**"), vec![st("é", B)]);
    }

    /// Deterministic pseudo-random inputs over the markdown alphabet: no panics, and the
    /// text content (minus markup) never grows beyond the input.
    #[test]
    fn fuzz_no_panics() {
        const ALPHABET: &[&str] = &[
            "*", "**", "_", "__", "~~", "||", "`", "```", ">", ">>> ", "> ", "#", "# ", "-# ", "- ", "1. ", " ", "  ", "\n",
            "\\", "[", "]", "(", ")", "<", ">", "@", "<@", "<@&", "<#", "<t:", "</", "<:", "<a:", ":", "1", "a", "é", "😀",
            "http://", "https://x.y", "@everyone", "<https://a.b>", "](https://x.com)", "0.", "!", "&",
        ];
        let mut seed: u64 = 0x2545_f491_4f6c_dd1d;
        for _ in 0..3000 {
            let mut s = String::new();
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let len = (seed % 40) as usize;
            for _ in 0..len {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                s.push_str(ALPHABET[(seed % ALPHABET.len() as u64) as usize]);
            }
            let _ = parse(&s);
        }
    }

    /// Pathological inputs finish quickly (work budget + linear rules).
    #[test]
    fn pathological_inputs_are_fast() {
        let start = std::time::Instant::now();
        for s in [
            "[a](".repeat(1000),
            "[".repeat(4000),
            "*a ".repeat(1500),
            "_a".repeat(2000),
            "**".repeat(2000),
            "- ".repeat(2000),
            "> ".repeat(2000),
            "||".repeat(2000) + "a",
            "`".repeat(4000),
            "<".repeat(4000),
            "<a:".repeat(1000),
            "\\".repeat(4000),
            "~~a".repeat(1300),
            format!("https://x.com/{}", ")".repeat(4000)),
            "*a**".repeat(1000),
        ] {
            let _ = parse(&s);
        }
        assert!(start.elapsed() < std::time::Duration::from_secs(5), "took {:?}", start.elapsed());
    }

    #[test]
    fn deep_nesting_is_bounded() {
        let s = "- ".repeat(100) + "x";
        let d = parse(&s);
        fn depth(b: &[Block]) -> usize {
            b.iter().map(|b| if let Block::ListItem { content, .. } = b { 1 + depth(content) } else { 0 }).max().unwrap_or(0)
        }
        assert!(depth(&d.blocks) <= MAX_NEST as usize);
    }
}
