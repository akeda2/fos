#![no_std]
//! The shell's scripting language: variables, `if`/`while`/`until`/`for`, `&&`/`||`/`;`, `$(cmd)`, `$((n+1))`, `>`/`>>`,
//! and the builtins `echo set exit shift test [ true false break continue`. Everything else goes to the port's `Env::run`.
//! Documented in docs/shell.md "Scripting".

extern crate alloc;

mod lex;
mod parse;

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use lex::{Part, Word};
use parse::{AndOr, Cmd, List};

/// What the interpreter needs from the machine it runs on.
pub trait Env {
    /// Runs one command line of the port's shell. With `capture` the output is appended there instead of printed.
    fn run(&mut self, line: &str, capture: Option<&mut Vec<u8>>) -> i32;
    fn print(&mut self, text: &str);
    /// Expands a pattern with `*`/`?`; empty when nothing matches.
    fn glob(&mut self, pattern: &str) -> Vec<String>;
    /// `Some(is_directory)` when the path exists.
    fn stat(&mut self, path: &str) -> Option<bool>;
    fn write(&mut self, path: &str, data: &[u8], append: bool) -> bool;
    /// True once the user asked to stop (Escape / Ctrl-C); checked every loop round.
    fn interrupted(&mut self) -> bool {
        false
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Flow {
    Run,
    Break(u32),
    Continue(u32),
    Exit,
}

pub struct Interp {
    vars: BTreeMap<String, String>,
    args: Vec<String>,
    status: i32,
    /// `$?`: the status of the previous command, fixed before the current one starts.
    previous: i32,
    trace: bool,
    flow: Flow,
    captures: Vec<Vec<u8>>,
    depth: u32,
    rounds: u32,
}

const MAX_DEPTH: u32 = 8;
/// Loop rounds one `run` may take before it stops (a runaway `while true` must not hang a machine that cannot be interrupted).
const MAX_ROUNDS: u32 = 1_000_000;

impl Default for Interp {
    fn default() -> Self {
        Self::new()
    }
}

impl Interp {
    pub fn new() -> Self {
        Interp { vars: BTreeMap::new(), args: Vec::new(), status: 0, previous: 0, trace: false, flow: Flow::Run, captures: Vec::new(), depth: 0, rounds: 0 }
    }

    /// `$1`, `$2`... (`$0` is not set).
    pub fn set_args(&mut self, args: Vec<String>) {
        self.args = args;
    }

    pub fn set_trace(&mut self, on: bool) {
        self.trace = on;
    }

    pub fn set_var(&mut self, name: &str, value: &str) {
        self.vars.insert(name.to_string(), value.to_string());
    }

    pub fn var(&self, name: &str) -> Option<&str> {
        self.vars.get(name).map(String::as_str)
    }

    /// Runs a script text; the result is the exit status.
    pub fn run(&mut self, env: &mut dyn Env, source: &str) -> i32 {
        let parsed = lex::lex(source).and_then(parse::parse);
        match parsed {
            Err(why) => {
                self.out(env, &alloc::format!("script: {why}\n"));
                self.status = 2;
            }
            Ok(list) => {
                self.flow = Flow::Run;
                self.rounds = 0;
                self.exec_list(env, &list);
                self.flow = Flow::Run;
            }
        }
        self.status
    }

    fn out(&mut self, env: &mut dyn Env, text: &str) {
        match self.captures.last_mut() {
            Some(buffer) => buffer.extend_from_slice(text.as_bytes()),
            None => env.print(text),
        }
    }

    fn exec_list(&mut self, env: &mut dyn Env, list: &List) {
        for item in list {
            self.exec_and_or(env, item);
            if self.flow != Flow::Run {
                return;
            }
        }
    }

    fn exec_and_or(&mut self, env: &mut dyn Env, item: &AndOr) {
        self.exec_cmd(env, &item.first);
        for (and, cmd) in &item.rest {
            if self.flow != Flow::Run {
                return;
            }
            if *and == (self.status == 0) {
                self.exec_cmd(env, cmd);
            }
        }
    }

    fn exec_cmd(&mut self, env: &mut dyn Env, cmd: &Cmd) {
        match cmd {
            Cmd::Simple { words, redirect, line } => self.simple(env, words, redirect.as_ref(), *line),
            Cmd::If { arms, other } => {
                for (cond, body) in arms {
                    self.exec_list(env, cond);
                    if self.flow != Flow::Run {
                        return;
                    }
                    if self.status == 0 {
                        self.exec_list(env, body);
                        return;
                    }
                }
                self.status = 0;
                if let Some(body) = other {
                    self.exec_list(env, body);
                }
            }
            Cmd::Loop { cond, body, until } => {
                self.status = 0;
                loop {
                    if self.stop(env) {
                        return;
                    }
                    self.exec_list(env, cond);
                    if self.flow != Flow::Run || (self.status == 0) == *until {
                        break;
                    }
                    self.exec_list(env, body);
                    if !self.after_body() {
                        break;
                    }
                }
                if self.flow == Flow::Run && !*until {
                    // the condition failed: the loop's own status is that of the last body (0 when it never ran)
                }
            }
            Cmd::For { var, words, body } => {
                let items = self.fields(env, words);
                self.status = 0;
                for item in items {
                    if self.stop(env) {
                        return;
                    }
                    self.vars.insert(var.clone(), item);
                    self.exec_list(env, body);
                    if !self.after_body() {
                        break;
                    }
                }
            }
        }
    }

    /// Called once per loop round: true when the user interrupted or the round limit is reached.
    fn stop(&mut self, env: &mut dyn Env) -> bool {
        self.rounds += 1;
        let limit = self.rounds > MAX_ROUNDS;
        if limit || env.interrupted() {
            if limit {
                self.out(env, "script: stopped after too many loop rounds\n");
            }
            self.status = 130;
            self.flow = Flow::Exit;
            return true;
        }
        false
    }

    /// Handles break/continue after a loop body; false when the loop must stop.
    fn after_body(&mut self) -> bool {
        match self.flow {
            Flow::Run => true,
            Flow::Break(n) => {
                self.flow = if n > 1 { Flow::Break(n - 1) } else { Flow::Run };
                false
            }
            Flow::Continue(n) => {
                if n > 1 {
                    self.flow = Flow::Continue(n - 1);
                    false
                } else {
                    self.flow = Flow::Run;
                    true
                }
            }
            Flow::Exit => false,
        }
    }

    fn simple(&mut self, env: &mut dyn Env, words: &[Word], redirect: Option<&(Word, bool)>, line: usize) {
        let mut rest = words;
        self.previous = self.status;
        self.status = 0;
        while let Some((first, tail)) = rest.split_first() {
            let Some((name, value)) = assignment(first) else { break };
            let value = self.text(env, &value);
            self.vars.insert(name, value);
            rest = tail;
        }
        if rest.is_empty() {
            return;
        }
        let fields = self.fields(env, rest);
        if fields.is_empty() {
            return;
        }
        if self.trace {
            let shown = quote_join(&fields);
            self.out(env, &alloc::format!("+ {shown}\n"));
        }
        let target = redirect.map(|(word, append)| (self.text(env, word), *append));
        if target.is_some() {
            self.captures.push(Vec::new());
        }
        self.dispatch(env, &fields, line);
        if let Some((path, append)) = target {
            let data = self.captures.pop().unwrap_or_default();
            if !env.write(&path, &data, append) {
                self.out(env, &alloc::format!("script: cannot write {path}\n"));
                self.status = 1;
            }
        }
    }

    fn dispatch(&mut self, env: &mut dyn Env, fields: &[String], line: usize) {
        let args = &fields[1..];
        match fields[0].as_str() {
            ":" | "true" => self.status = 0,
            "false" => self.status = 1,
            "echo" => {
                let (newline, args) = match args.first().map(String::as_str) {
                    Some("-n") => (false, &args[1..]),
                    _ => (true, args),
                };
                let mut text = args.join(" ");
                if newline {
                    text.push('\n');
                }
                self.out(env, &text);
            }
            "exit" => {
                self.status = args.first().and_then(|s| s.parse().ok()).unwrap_or(self.status);
                self.flow = Flow::Exit;
            }
            "break" => self.flow = Flow::Break(args.first().and_then(|s| s.parse().ok()).unwrap_or(1).max(1)),
            "continue" => self.flow = Flow::Continue(args.first().and_then(|s| s.parse().ok()).unwrap_or(1).max(1)),
            "shift" => {
                let count = args.first().and_then(|s| s.parse().ok()).unwrap_or(1usize);
                if count > self.args.len() {
                    self.status = 1;
                } else {
                    self.args.drain(..count);
                }
            }
            "set" => self.set(env, args),
            "test" => self.test(env, args),
            "[" => match args.split_last() {
                Some((last, inner)) if last == "]" => self.test(env, inner),
                _ => {
                    self.out(env, "script: [ needs a closing ]\n");
                    self.status = 2;
                }
            },
            _ => {
                let command = quote_join(fields);
                if self.depth == 0 && command.is_empty() {
                    return;
                }
                let _ = line;
                self.status = if self.captures.is_empty() {
                    env.run(&command, None)
                } else {
                    let mut buffer = Vec::new();
                    let status = env.run(&command, Some(&mut buffer));
                    if let Some(top) = self.captures.last_mut() {
                        top.extend_from_slice(&buffer);
                    }
                    status
                };
            }
        }
    }

    fn set(&mut self, env: &mut dyn Env, args: &[String]) {
        match args.first().map(String::as_str) {
            Some("-x") => self.trace = true,
            Some("+x") => self.trace = false,
            Some("--") => self.args = args[1..].to_vec(),
            None => {
                let lines: Vec<String> = self.vars.iter().map(|(k, v)| alloc::format!("{k}={v}\n")).collect();
                for line in lines {
                    self.out(env, &line);
                }
            }
            Some(_) => {
                for arg in args {
                    match arg.split_once('=') {
                        Some((name, value)) if valid_name(name) => {
                            self.vars.insert(name.to_string(), value.to_string());
                        }
                        _ => {
                            self.out(env, "script: set NAME=VALUE, set -x, set +x or set -- ARGS\n");
                            self.status = 2;
                            return;
                        }
                    }
                }
            }
        }
    }

    fn test(&mut self, env: &mut dyn Env, args: &[String]) {
        let (negate, args) = match args.first().map(String::as_str) {
            Some("!") => (true, &args[1..]),
            _ => (false, args),
        };
        let result = match args {
            [] => Some(false),
            [one] => Some(!one.is_empty()),
            [op, value] => match op.as_str() {
                "-z" => Some(value.is_empty()),
                "-n" => Some(!value.is_empty()),
                "-e" => Some(env.stat(value).is_some()),
                "-f" => Some(env.stat(value) == Some(false)),
                "-d" => Some(env.stat(value) == Some(true)),
                _ => None,
            },
            [left, op, right] => match op.as_str() {
                "=" | "==" => Some(left == right),
                "!=" => Some(left != right),
                "-eq" | "-ne" | "-lt" | "-le" | "-gt" | "-ge" => match (left.trim().parse::<i64>(), right.trim().parse::<i64>()) {
                    (Ok(a), Ok(b)) => Some(match op.as_str() {
                        "-eq" => a == b,
                        "-ne" => a != b,
                        "-lt" => a < b,
                        "-le" => a <= b,
                        "-gt" => a > b,
                        _ => a >= b,
                    }),
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        };
        match result {
            Some(value) => self.status = if value != negate { 0 } else { 1 },
            None => {
                self.out(env, "script: test: bad expression\n");
                self.status = 2;
            }
        }
    }

    fn lookup(&self, name: &str) -> String {
        match name {
            "?" => self.previous.to_string(),
            "#" => self.args.len().to_string(),
            "*" | "@" => self.args.join(" "),
            _ if name.chars().all(|c| c.is_ascii_digit()) => {
                let n: usize = name.parse().unwrap_or(0);
                n.checked_sub(1).and_then(|i| self.args.get(i)).cloned().unwrap_or_default()
            }
            _ => self.vars.get(name).cloned().unwrap_or_default(),
        }
    }

    fn text(&mut self, env: &mut dyn Env, word: &Word) -> String {
        self.expand(env, word, false).into_iter().map(|(s, _)| s).collect::<Vec<_>>().join(" ")
    }

    /// Words to fields: splitting of unquoted expansions, then globbing of unquoted `*` and `?`.
    fn fields(&mut self, env: &mut dyn Env, words: &[Word]) -> Vec<String> {
        let mut out = Vec::new();
        for word in words {
            for (field, glob) in self.expand(env, word, true) {
                if glob {
                    let found = env.glob(&field);
                    if found.is_empty() {
                        out.push(field);
                    } else {
                        out.extend(found);
                    }
                } else {
                    out.push(field);
                }
            }
        }
        out
    }

    fn expand(&mut self, env: &mut dyn Env, word: &Word, split: bool) -> Vec<(String, bool)> {
        let mut done: Vec<(String, bool)> = Vec::new();
        let mut current: Option<(String, bool)> = None;
        for part in word {
            let (value, quoted, literal) = match part {
                Part::Lit { text, quoted } => (text.clone(), *quoted, true),
                Part::Var { name, quoted } => {
                    if name == "@" && *quoted && split {
                        let args = self.args.clone();
                        for (index, arg) in args.into_iter().enumerate() {
                            if index > 0 {
                                done.extend(current.take());
                            }
                            current.get_or_insert_with(Default::default).0.push_str(&arg);
                        }
                        continue;
                    }
                    (self.lookup(name), *quoted, false)
                }
                Part::Cmd { source, quoted } => (self.substitute(env, source), *quoted, false),
                Part::Arith { source, quoted } => {
                    let value = match self.arith(source) {
                        Ok(n) => n.to_string(),
                        Err(why) => {
                            self.out(env, &alloc::format!("script: arithmetic: {why}\n"));
                            self.status = 2;
                            "0".to_string()
                        }
                    };
                    (value, *quoted, false)
                }
            };
            if quoted || !split {
                let entry = current.get_or_insert_with(Default::default);
                entry.0.push_str(&value);
                continue;
            }
            if literal {
                let entry = current.get_or_insert_with(Default::default);
                entry.0.push_str(&value);
                entry.1 |= value.contains(['*', '?']);
                continue;
            }
            if value.starts_with(char::is_whitespace) {
                done.extend(current.take());
            }
            let mut pieces = value.split_whitespace().peekable();
            while let Some(piece) = pieces.next() {
                current.get_or_insert_with(Default::default).0.push_str(piece);
                if pieces.peek().is_some() {
                    done.extend(current.take());
                }
            }
            if value.ends_with(char::is_whitespace) {
                done.extend(current.take());
            }
        }
        done.extend(current);
        done
    }

    /// `$(source)`: the output of the script, without trailing newlines.
    fn substitute(&mut self, env: &mut dyn Env, source: &str) -> String {
        if self.depth >= MAX_DEPTH {
            return String::new();
        }
        let (flow, status) = (self.flow, self.status);
        self.depth += 1;
        self.captures.push(Vec::new());
        let parsed = lex::lex(source).and_then(parse::parse);
        match parsed {
            Ok(list) => {
                self.flow = Flow::Run;
                self.exec_list(env, &list);
            }
            Err(why) => {
                self.out(env, &alloc::format!("script: {why}\n"));
                self.status = 2;
            }
        }
        let captured = self.captures.pop().unwrap_or_default();
        self.depth -= 1;
        self.flow = flow;
        let _ = status;
        let mut text = String::from_utf8_lossy(&captured).into_owned();
        while text.ends_with('\n') {
            text.pop();
        }
        text
    }

    fn arith(&self, source: &str) -> Result<i64, String> {
        let mut parser = Arith { chars: source.chars().collect(), at: 0, interp: self };
        let value = parser.expr(0)?;
        parser.skip_space();
        if parser.at < parser.chars.len() {
            return Err("unexpected character".to_string());
        }
        Ok(value)
    }
}

struct Arith<'a> {
    chars: Vec<char>,
    at: usize,
    interp: &'a Interp,
}

const OPERATORS: [(&str, u8); 13] =
    [("||", 1), ("&&", 2), ("==", 3), ("!=", 3), ("<=", 4), (">=", 4), ("<", 4), (">", 4), ("+", 5), ("-", 5), ("*", 6), ("/", 6), ("%", 6)];

impl Arith<'_> {
    fn skip_space(&mut self) {
        while self.chars.get(self.at).is_some_and(|c| c.is_whitespace()) {
            self.at += 1;
        }
    }

    fn operator(&mut self) -> Option<(&'static str, u8)> {
        self.skip_space();
        OPERATORS.iter().copied().find(|(text, _)| text.chars().enumerate().all(|(i, c)| self.chars.get(self.at + i) == Some(&c)))
    }

    fn expr(&mut self, minimum: u8) -> Result<i64, String> {
        let mut left = self.unary()?;
        while let Some((op, precedence)) = self.operator() {
            if precedence <= minimum {
                break;
            }
            self.at += op.len();
            let right = self.expr(precedence)?;
            left = match op {
                "||" => (left != 0 || right != 0) as i64,
                "&&" => (left != 0 && right != 0) as i64,
                "==" => (left == right) as i64,
                "!=" => (left != right) as i64,
                "<=" => (left <= right) as i64,
                ">=" => (left >= right) as i64,
                "<" => (left < right) as i64,
                ">" => (left > right) as i64,
                "+" => left.wrapping_add(right),
                "-" => left.wrapping_sub(right),
                "*" => left.wrapping_mul(right),
                "/" | "%" if right == 0 => return Err("division by zero".to_string()),
                "/" => left.wrapping_div(right),
                _ => left.wrapping_rem(right),
            };
        }
        Ok(left)
    }

    fn unary(&mut self) -> Result<i64, String> {
        self.skip_space();
        match self.chars.get(self.at).copied() {
            Some('-') => {
                self.at += 1;
                Ok(self.unary()?.wrapping_neg())
            }
            Some('!') => {
                self.at += 1;
                Ok((self.unary()? == 0) as i64)
            }
            Some('(') => {
                self.at += 1;
                let value = self.expr(0)?;
                self.skip_space();
                if self.chars.get(self.at) != Some(&')') {
                    return Err("missing )".to_string());
                }
                self.at += 1;
                Ok(value)
            }
            Some(c) if c.is_ascii_digit() => {
                let start = self.at;
                while self.chars.get(self.at).is_some_and(|c| c.is_ascii_digit()) {
                    self.at += 1;
                }
                self.chars[start..self.at].iter().collect::<String>().parse().map_err(|_| "number too large".to_string())
            }
            Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$' || c == '?' || c == '#' => {
                if c == '$' {
                    self.at += 1;
                }
                let start = self.at;
                if matches!(self.chars.get(self.at), Some('?' | '#')) {
                    self.at += 1;
                } else {
                    while self.chars.get(self.at).is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_') {
                        self.at += 1;
                    }
                }
                let name: String = self.chars[start..self.at].iter().collect();
                Ok(self.interp.lookup(&name).trim().parse().unwrap_or(0))
            }
            _ => Err("expression expected".to_string()),
        }
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && !name.starts_with(|c: char| c.is_ascii_digit()) && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `NAME=value` at the start of a command: the name and the word that is the value.
fn assignment(word: &Word) -> Option<(String, Word)> {
    let Some(Part::Lit { text, quoted: false }) = word.first() else { return None };
    let (name, value) = text.split_once('=')?;
    if !valid_name(name) {
        return None;
    }
    let mut parts: Word = Vec::new();
    if !value.is_empty() {
        parts.push(Part::Lit { text: value.to_string(), quoted: true });
    }
    parts.extend(word[1..].iter().cloned());
    Some((name.to_string(), parts))
}

/// A command line from fields: those with spaces (or none) are put in double quotes so the port's shell sees them whole.
pub fn quote_join(fields: &[String]) -> String {
    let mut line = String::new();
    for (index, field) in fields.iter().enumerate() {
        if index > 0 {
            line.push(' ');
        }
        if field.is_empty() || field.contains(char::is_whitespace) {
            line.push('"');
            line.push_str(field);
            line.push('"');
        } else {
            line.push_str(field);
        }
    }
    line
}

#[cfg(test)]
mod tests;
