//! Tokenizer and block-reader for the A2L (ASAM MCD-2 MC) text grammar.
//!
//! A2L is a small keyword/block language:
//!
//! ```text
//! /begin TAG            # opens a block
//!   name "long ident"   # bare words and double-quoted strings
//!   VALUE 0x1234 ...     # positional fields, then optional keyword fields
//!   /begin SUBTAG .. /end SUBTAG   # blocks nest
//! /end TAG
//! ```
//!
//! Comments are C-style `/* ... */` and `// to end of line`. Whitespace
//! (including newlines) is insignificant: the same block may be written one
//! token per line or several tokens per line, so the reader is strictly
//! token-based, never line-based.
//!
//! [`lex`] turns a source string into a flat [`Token`] stream; [`build_tree`]
//! folds that stream into a [`Block`] tree (`/begin`…`/end` nesting), with the
//! leaf tokens of each block kept in order in [`Block::words`] and nested blocks
//! in [`Block::children`].

/// A single lexical token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    /// `/begin <tag>` — opens a block with the given tag.
    Begin(String),
    /// `/end <tag>` — closes a block.
    End(String),
    /// A double-quoted string literal (quotes removed, `""`/`\"` unescaped).
    Str(String),
    /// Any other whitespace-delimited word (identifier, number, keyword).
    Word(String),
}

impl Token {
    /// The string content of a `Str`/`Word` token, if it is one.
    pub fn text(&self) -> Option<&str> {
        match self {
            Token::Str(s) | Token::Word(s) => Some(s),
            _ => None,
        }
    }
}

/// Lex an A2L source string into a flat token stream, folding `/begin TAG` and
/// `/end TAG` into [`Token::Begin`] / [`Token::End`].
pub fn lex(src: &str) -> Vec<Token> {
    let pieces = lex_pieces_utf8(src);
    let mut out = Vec::with_capacity(pieces.len());
    let mut it = pieces.into_iter().peekable();
    while let Some(tok) = it.next() {
        match tok {
            Token::Word(w) if w == "/begin" => {
                if let Some(Token::Word(tag)) = it.next() {
                    out.push(Token::Begin(tag));
                }
            }
            Token::Word(w) if w == "/end" => {
                if let Some(Token::Word(tag)) = it.next() {
                    out.push(Token::End(tag));
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// Like [`raw_pieces`] but keeps string literals as UTF-8 slices of the source.
fn lex_pieces_utf8(src: &str) -> Vec<Token> {
    let b = src.as_bytes();
    let n = b.len();
    let mut i = 0;
    let mut out = Vec::new();
    while i < n {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if c == b'/' && i + 1 < n {
            if b[i + 1] == b'*' {
                i += 2;
                while i + 1 < n && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(n);
                continue;
            }
            if b[i + 1] == b'/' {
                i += 2;
                while i < n && b[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
        }
        if c == b'"' {
            i += 1;
            let mut s = String::new();
            let mut span_start = i;
            while i < n {
                if b[i] == b'"' {
                    s.push_str(&src[span_start..i]);
                    if i + 1 < n && b[i + 1] == b'"' {
                        s.push('"');
                        i += 2;
                        span_start = i;
                        continue;
                    }
                    i += 1;
                    break;
                }
                if b[i] == b'\\' && i + 1 < n && b[i + 1] == b'"' {
                    s.push_str(&src[span_start..i]);
                    s.push('"');
                    i += 2;
                    span_start = i;
                    continue;
                }
                i += 1;
            }
            if i == n {
                // Unterminated string: take the rest.
                s.push_str(&src[span_start..n]);
            }
            out.push(Token::Str(s));
            continue;
        }
        let start = i;
        while i < n {
            let d = b[i];
            if d.is_ascii_whitespace() || d == b'"' {
                break;
            }
            if d == b'/' && i + 1 < n && (b[i + 1] == b'*' || b[i + 1] == b'/') {
                break;
            }
            i += 1;
        }
        out.push(Token::Word(src[start..i].to_string()));
    }
    out
}

/// A parsed A2L block: its tag, its leaf tokens (in order), and its nested
/// child blocks (in order). `/begin`…`/end` nesting is resolved here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Block {
    pub tag: String,
    /// Leaf tokens of this block (bare words and strings), in source order,
    /// excluding nested blocks.
    pub words: Vec<Token>,
    /// Nested child blocks, in source order.
    pub children: Vec<Block>,
}

impl Block {
    /// The `i`-th leaf token's string content, if present.
    pub fn word(&self, i: usize) -> Option<&str> {
        self.words.get(i).and_then(Token::text)
    }

    /// Index of the first leaf word equal to `kw` (case-sensitive; A2L keywords
    /// are upper-case).
    pub fn keyword_pos(&self, kw: &str) -> Option<usize> {
        self.words.iter().position(|t| t.text() == Some(kw))
    }

    /// The token `offset` positions after keyword `kw`, if both exist.
    pub fn after_keyword(&self, kw: &str, offset: usize) -> Option<&str> {
        let p = self.keyword_pos(kw)?;
        self.words.get(p + offset).and_then(Token::text)
    }

    /// The first direct child block with the given tag.
    pub fn child(&self, tag: &str) -> Option<&Block> {
        self.children.iter().find(|c| c.tag == tag)
    }

    /// All direct child blocks with the given tag, in order.
    pub fn children_tagged<'a>(&'a self, tag: &'a str) -> impl Iterator<Item = &'a Block> {
        self.children.iter().filter(move |c| c.tag == tag)
    }

    /// Every descendant block (any depth) with the given tag.
    pub fn descendants_tagged<'a>(&'a self, tag: &str, out: &mut Vec<&'a Block>) {
        for c in &self.children {
            if c.tag == tag {
                out.push(c);
            }
            c.descendants_tagged(tag, out);
        }
    }

    /// Find the first descendant block (any depth) with the given tag.
    pub fn find_descendant(&self, tag: &str) -> Option<&Block> {
        for c in &self.children {
            if c.tag == tag {
                return Some(c);
            }
            if let Some(found) = c.find_descendant(tag) {
                return Some(found);
            }
        }
        None
    }
}

/// Fold a flat token stream into a single synthetic root [`Block`] whose
/// children are the top-level blocks of the file. Tokens outside any block
/// (e.g. `ASAP2_VERSION 1 21`) are kept as the root's leaf words.
pub fn build_tree(tokens: Vec<Token>) -> Block {
    let mut stack: Vec<Block> = vec![Block {
        tag: "<root>".to_string(),
        ..Default::default()
    }];
    for tok in tokens {
        match tok {
            Token::Begin(tag) => stack.push(Block {
                tag,
                ..Default::default()
            }),
            Token::End(_tag) => {
                if stack.len() > 1 {
                    let done = stack.pop().unwrap();
                    stack.last_mut().unwrap().children.push(done);
                }
                // An unmatched /end is ignored (tolerant parse).
            }
            w @ (Token::Str(_) | Token::Word(_)) => {
                stack.last_mut().unwrap().words.push(w);
            }
        }
    }
    // Fold any unclosed blocks (malformed input) up into their parents.
    while stack.len() > 1 {
        let done = stack.pop().unwrap();
        stack.last_mut().unwrap().children.push(done);
    }
    stack.pop().unwrap()
}

/// Lex and build a block tree in one step.
pub fn parse_tree(src: &str) -> Block {
    build_tree(lex(src))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lex_basic_blocks() {
        let src = r#"/begin A foo "bar baz" 0x10 /end A"#;
        let toks = lex(src);
        assert_eq!(toks[0], Token::Begin("A".into()));
        assert_eq!(toks[1], Token::Word("foo".into()));
        assert_eq!(toks[2], Token::Str("bar baz".into()));
        assert_eq!(toks[3], Token::Word("0x10".into()));
        assert_eq!(toks[4], Token::End("A".into()));
    }

    #[test]
    fn comments_and_whitespace_are_ignored() {
        let src = "
            /begin A /* block comment
               spanning lines */ foo // trailing
               bar
            /end A";
        let root = parse_tree(src);
        let a = root.child("A").expect("block A");
        assert_eq!(a.word(0), Some("foo"));
        assert_eq!(a.word(1), Some("bar"));
    }

    #[test]
    fn nested_blocks_preserve_order() {
        let src = r#"
            /begin OUTER name "d"
              /begin INNER x /end INNER
              /begin INNER y /end INNER
            /end OUTER"#;
        let root = parse_tree(src);
        let outer = root.child("OUTER").unwrap();
        assert_eq!(outer.word(0), Some("name"));
        assert_eq!(outer.word(1), Some("d"));
        let inners: Vec<_> = outer.children_tagged("INNER").collect();
        assert_eq!(inners.len(), 2);
        assert_eq!(inners[0].word(0), Some("x"));
        assert_eq!(inners[1].word(0), Some("y"));
    }

    #[test]
    fn doubled_quote_escape() {
        let src = r#"/begin A "say ""hi""" /end A"#;
        let root = parse_tree(src);
        assert_eq!(root.child("A").unwrap().word(0), Some(r#"say "hi""#));
    }

    #[test]
    fn utf8_string_preserved() {
        let src = "/begin A \"Drehzahl µ ° text\" /end A";
        let root = parse_tree(src);
        assert_eq!(root.child("A").unwrap().word(0), Some("Drehzahl µ ° text"));
    }
}
