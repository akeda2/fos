//! Tokens to a tree: `if`/`elif`/`else`/`fi`, `while`/`until`/`do`/`done`, `for X in ...; do ... done`, `&&`/`||`, one `>`/`>>`.

use crate::lex::{plain, Tok, Token, Word};
use alloc::string::{String, ToString};
use alloc::vec::Vec;

pub enum Cmd {
    Simple { words: Vec<Word>, redirect: Option<(Word, bool)>, line: usize },
    If { arms: Vec<(List, List)>, other: Option<List> },
    Loop { cond: List, body: List, until: bool },
    For { var: String, words: Vec<Word>, body: List },
}

pub struct AndOr {
    pub first: Cmd,
    pub rest: Vec<(bool, Cmd)>,
}

pub type List = Vec<AndOr>;

const RESERVED: [&str; 7] = ["then", "elif", "else", "fi", "do", "done", "in"];

struct Parser {
    tokens: Vec<Token>,
    at: usize,
}

pub fn parse(tokens: Vec<Token>) -> Result<List, String> {
    let mut parser = Parser { tokens, at: 0 };
    let list = parser.list(&[])?;
    match parser.tokens.get(parser.at) {
        None => Ok(list),
        Some(token) => Err(alloc::format!("line {}: unexpected token", token.line)),
    }
}

impl Parser {
    fn line(&self) -> usize {
        self.tokens.get(self.at).or(self.tokens.last()).map_or(1, |t| t.line)
    }

    fn is_keyword(&self, keyword: &str) -> bool {
        matches!(self.tokens.get(self.at), Some(Token { tok: Tok::Word(w), .. }) if plain(w) == Some(keyword))
    }

    fn skip_separators(&mut self) {
        while matches!(self.tokens.get(self.at), Some(Token { tok: Tok::Semi | Tok::Newline, .. })) {
            self.at += 1;
        }
    }

    fn expect(&mut self, keyword: &str) -> Result<(), String> {
        self.skip_separators();
        if self.is_keyword(keyword) {
            self.at += 1;
            Ok(())
        } else {
            Err(alloc::format!("line {}: expected `{keyword}`", self.line()))
        }
    }

    fn list(&mut self, stops: &[&str]) -> Result<List, String> {
        let mut list = Vec::new();
        loop {
            self.skip_separators();
            if self.at >= self.tokens.len() || stops.iter().any(|stop| self.is_keyword(stop)) {
                return Ok(list);
            }
            if !stops.is_empty() || !RESERVED.iter().any(|word| self.is_keyword(word)) {
                list.push(self.and_or()?);
            } else {
                return Err(alloc::format!("line {}: unexpected keyword", self.line()));
            }
        }
    }

    fn and_or(&mut self) -> Result<AndOr, String> {
        let first = self.command()?;
        let mut rest = Vec::new();
        loop {
            let and = match self.tokens.get(self.at) {
                Some(Token { tok: Tok::AndIf, .. }) => true,
                Some(Token { tok: Tok::OrIf, .. }) => false,
                _ => break,
            };
            self.at += 1;
            while matches!(self.tokens.get(self.at), Some(Token { tok: Tok::Newline, .. })) {
                self.at += 1;
            }
            rest.push((and, self.command()?));
        }
        Ok(AndOr { first, rest })
    }

    fn command(&mut self) -> Result<Cmd, String> {
        if self.is_keyword("if") {
            self.at += 1;
            let mut arms = Vec::new();
            let mut other = None;
            loop {
                let cond = self.list(&["then"])?;
                self.expect("then")?;
                let body = self.list(&["elif", "else", "fi"])?;
                arms.push((cond, body));
                if self.is_keyword("elif") {
                    self.at += 1;
                } else if self.is_keyword("else") {
                    self.at += 1;
                    other = Some(self.list(&["fi"])?);
                    self.expect("fi")?;
                    break;
                } else {
                    self.expect("fi")?;
                    break;
                }
            }
            return Ok(Cmd::If { arms, other });
        }
        for (keyword, until) in [("while", false), ("until", true)] {
            if self.is_keyword(keyword) {
                self.at += 1;
                let cond = self.list(&["do"])?;
                self.expect("do")?;
                let body = self.list(&["done"])?;
                self.expect("done")?;
                return Ok(Cmd::Loop { cond, body, until });
            }
        }
        if self.is_keyword("for") {
            self.at += 1;
            let line = self.line();
            let var = match self.tokens.get(self.at) {
                Some(Token { tok: Tok::Word(w), .. }) => plain(w).map(|s| s.to_string()),
                _ => None,
            }
            .ok_or_else(|| alloc::format!("line {line}: `for` needs a variable name"))?;
            self.at += 1;
            let mut words = Vec::new();
            if self.is_keyword("in") {
                self.at += 1;
                while let Some(Token { tok: Tok::Word(w), .. }) = self.tokens.get(self.at) {
                    words.push(w.clone());
                    self.at += 1;
                }
            }
            self.expect("do")?;
            let body = self.list(&["done"])?;
            self.expect("done")?;
            return Ok(Cmd::For { var, words, body });
        }
        self.simple()
    }

    fn simple(&mut self) -> Result<Cmd, String> {
        let line = self.line();
        let mut words = Vec::new();
        let mut redirect = None;
        loop {
            match self.tokens.get(self.at) {
                Some(Token { tok: Tok::Word(w), .. }) => {
                    words.push(w.clone());
                    self.at += 1;
                }
                Some(Token { tok: tok @ (Tok::Gt | Tok::GtGt), .. }) => {
                    let append = *tok == Tok::GtGt;
                    self.at += 1;
                    match self.tokens.get(self.at) {
                        Some(Token { tok: Tok::Word(w), .. }) => {
                            redirect = Some((w.clone(), append));
                            self.at += 1;
                        }
                        _ => return Err(alloc::format!("line {line}: `>` needs a file name")),
                    }
                }
                Some(Token { tok: Tok::Pipe, .. }) => {
                    return Err(alloc::format!("line {line}: `|` is not supported in scripts (use a file and `>`)"));
                }
                _ => break,
            }
        }
        if words.is_empty() {
            return Err(alloc::format!("line {line}: missing command"));
        }
        Ok(Cmd::Simple { words, redirect, line })
    }
}
