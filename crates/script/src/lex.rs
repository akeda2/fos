//! Words and operators. A word is a list of parts so that quoting survives to expansion: a quoted `$VAR` is never split into
//! fields, an unquoted `*` may be a glob.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

#[derive(Clone, Debug, PartialEq)]
pub enum Part {
    Lit { text: String, quoted: bool },
    /// `$NAME`, `${NAME}`, `$1`, `$?`, `$#`, `$*`, `$@`.
    Var { name: String, quoted: bool },
    /// `$(source)`.
    Cmd { source: String, quoted: bool },
    /// `$((source))`.
    Arith { source: String, quoted: bool },
}

pub type Word = Vec<Part>;

#[derive(Clone, Debug, PartialEq)]
pub enum Tok {
    Word(Word),
    Semi,
    AndIf,
    OrIf,
    Pipe,
    Gt,
    GtGt,
    Newline,
}

pub struct Token {
    pub tok: Tok,
    pub line: usize,
}

/// The plain text of a word that is entirely unquoted literal (a keyword candidate or a name).
pub fn plain(word: &Word) -> Option<&str> {
    match word.as_slice() {
        [Part::Lit { text, quoted: false }] => Some(text),
        _ => None,
    }
}

/// Index of the `)` that closes a `(` already consumed (`start` is the first character after it).
fn matching(chars: &[char], start: usize) -> Option<usize> {
    let mut depth = 1;
    let mut i = start;
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 1,
            '\'' => {
                i += 1;
                while i < chars.len() && chars[i] != '\'' {
                    i += 1;
                }
            }
            '"' => {
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    if chars[i] == '\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Reads the expansion at `chars[*i] == '$'` into `parts`, advancing `i` past it.
fn dollar(chars: &[char], i: &mut usize, quoted: bool, parts: &mut Word) -> Result<(), String> {
    let next = chars.get(*i + 1).copied();
    match next {
        Some('(') => {
            let end = matching(chars, *i + 2).ok_or("unterminated $(")?;
            let inner: String = chars[*i + 2..end].iter().collect();
            let arithmetic = inner.starts_with('(') && matching(&inner.chars().collect::<Vec<_>>(), 1) == Some(inner.chars().count() - 1);
            if arithmetic {
                let source = inner.chars().skip(1).take(inner.chars().count() - 2).collect();
                parts.push(Part::Arith { source, quoted });
            } else {
                parts.push(Part::Cmd { source: inner, quoted });
            }
            *i = end + 1;
        }
        Some('{') => {
            let end = chars[*i + 2..].iter().position(|&c| c == '}').ok_or("unterminated ${")? + *i + 2;
            parts.push(Part::Var { name: chars[*i + 2..end].iter().collect(), quoted });
            *i = end + 1;
        }
        Some(c) if c.is_ascii_digit() => {
            parts.push(Part::Var { name: c.to_string(), quoted });
            *i += 2;
        }
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {
            let mut end = *i + 1;
            while end < chars.len() && (chars[end].is_ascii_alphanumeric() || chars[end] == '_') {
                end += 1;
            }
            parts.push(Part::Var { name: chars[*i + 1..end].iter().collect(), quoted });
            *i = end;
        }
        Some(c @ ('?' | '#' | '*' | '@')) => {
            parts.push(Part::Var { name: c.to_string(), quoted });
            *i += 2;
        }
        _ => {
            parts.push(Part::Lit { text: "$".to_string(), quoted });
            *i += 1;
        }
    }
    Ok(())
}

fn flush(parts: &mut Word, text: &mut String) {
    if !text.is_empty() {
        parts.push(Part::Lit { text: core::mem::take(text), quoted: false });
    }
}

fn ends_word(chars: &[char], i: usize) -> bool {
    match chars[i] {
        ' ' | '\t' | '\r' | '\n' | ';' | '|' | '>' | '<' => true,
        '&' => chars.get(i + 1) == Some(&'&'),
        _ => false,
    }
}

pub fn lex(text: &str) -> Result<Vec<Token>, String> {
    let chars: Vec<char> = text.chars().collect();
    let mut tokens = Vec::new();
    let mut line = 1;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        let simple = match c {
            ' ' | '\t' | '\r' => {
                i += 1;
                continue;
            }
            '\n' => {
                tokens.push(Token { tok: Tok::Newline, line });
                line += 1;
                i += 1;
                continue;
            }
            '#' => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            ';' => Some((Tok::Semi, 1)),
            '|' if next == Some('|') => Some((Tok::OrIf, 2)),
            '|' => Some((Tok::Pipe, 1)),
            '&' if next == Some('&') => Some((Tok::AndIf, 2)),
            '>' if next == Some('>') => Some((Tok::GtGt, 2)),
            '>' => Some((Tok::Gt, 1)),
            '<' => return Err(alloc::format!("line {line}: `<` (input redirection) is not supported")),
            _ => None,
        };
        if let Some((tok, length)) = simple {
            tokens.push(Token { tok, line });
            i += length;
            continue;
        }
        let mut parts: Word = Vec::new();
        let mut literal = String::new();
        while i < chars.len() && !ends_word(&chars, i) {
            match chars[i] {
                '\\' => match chars.get(i + 1) {
                    Some('\n') => {
                        line += 1;
                        i += 2;
                    }
                    Some(&escaped) => {
                        flush(&mut parts, &mut literal);
                        parts.push(Part::Lit { text: escaped.to_string(), quoted: true });
                        i += 2;
                    }
                    None => i += 1,
                },
                '\'' => {
                    flush(&mut parts, &mut literal);
                    let end = chars[i + 1..].iter().position(|&c| c == '\'').ok_or_else(|| alloc::format!("line {line}: unterminated '"))? + i + 1;
                    let quoted: String = chars[i + 1..end].iter().collect();
                    line += quoted.matches('\n').count();
                    parts.push(Part::Lit { text: quoted, quoted: true });
                    i = end + 1;
                }
                '"' => {
                    flush(&mut parts, &mut literal);
                    i += 1;
                    let mut inside = String::new();
                    let mut produced = false;
                    loop {
                        let Some(&c) = chars.get(i) else { return Err(alloc::format!("line {line}: unterminated \"")) };
                        match c {
                            '"' => {
                                i += 1;
                                break;
                            }
                            '\\' if matches!(chars.get(i + 1), Some('"' | '\\' | '$')) => {
                                inside.push(chars[i + 1]);
                                i += 2;
                            }
                            '$' => {
                                if !inside.is_empty() {
                                    parts.push(Part::Lit { text: core::mem::take(&mut inside), quoted: true });
                                }
                                dollar(&chars, &mut i, true, &mut parts).map_err(|why| alloc::format!("line {line}: {why}"))?;
                                produced = true;
                            }
                            c => {
                                if c == '\n' {
                                    line += 1;
                                }
                                inside.push(c);
                                i += 1;
                            }
                        }
                    }
                    if !inside.is_empty() || !produced {
                        parts.push(Part::Lit { text: inside, quoted: true });
                    }
                }
                '$' => {
                    flush(&mut parts, &mut literal);
                    dollar(&chars, &mut i, false, &mut parts).map_err(|why| alloc::format!("line {line}: {why}"))?;
                }
                c => {
                    literal.push(c);
                    i += 1;
                }
            }
        }
        flush(&mut parts, &mut literal);
        if !parts.is_empty() {
            tokens.push(Token { tok: Tok::Word(parts), line });
        }
    }
    Ok(tokens)
}
