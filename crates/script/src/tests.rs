use super::*;
use alloc::string::String;

#[derive(Default)]
struct Mock {
    printed: String,
    ran: Vec<String>,
    files: Vec<(String, String)>,
}

impl Env for Mock {
    fn run(&mut self, line: &str, capture: Option<&mut Vec<u8>>) -> i32 {
        self.ran.push(line.to_string());
        if line == "fail" {
            return 1;
        }
        let text = alloc::format!("<{line}>\n");
        match capture {
            Some(buffer) => buffer.extend_from_slice(text.as_bytes()),
            None => self.printed.push_str(&text),
        }
        0
    }
    fn print(&mut self, text: &str) {
        self.printed.push_str(text);
    }
    fn glob(&mut self, pattern: &str) -> Vec<String> {
        if pattern == "*.txt" { alloc::vec!["a.txt".to_string(), "b c.txt".to_string()] } else { Vec::new() }
    }
    fn stat(&mut self, path: &str) -> Option<bool> {
        match path {
            "dir" => Some(true),
            "file" => Some(false),
            _ => None,
        }
    }
    fn write(&mut self, path: &str, data: &[u8], append: bool) -> bool {
        let text = String::from_utf8_lossy(data).into_owned();
        match self.files.iter_mut().find(|(name, _)| name == path) {
            Some((_, old)) if append => old.push_str(&text),
            Some((_, old)) => *old = text,
            None => self.files.push((path.to_string(), text)),
        }
        true
    }
}

fn run(source: &str) -> (String, i32, Mock) {
    let mut env = Mock::default();
    let mut interp = Interp::new();
    interp.set_args(alloc::vec!["one".to_string(), "two words".to_string()]);
    let status = interp.run(&mut env, source);
    (env.printed.clone(), status, env)
}

#[test]
fn variables_and_quoting() {
    let (out, _, _) = run("X=hello\necho $X \"$X world\" '$X' ${X}s $1 \"$2\" $#");
    assert_eq!(out, "hello hello world $X hellos one two words 2\n");
}

#[test]
fn unquoted_expansion_splits_and_quoted_does_not() {
    let (_, _, env) = run("L=\"a b\"\nfoo $L\nfoo \"$L\"\nfoo \"$@\"");
    assert_eq!(env.ran, ["foo a b", "foo \"a b\"", "foo one \"two words\""]);
}

#[test]
fn if_elif_else_and_status() {
    let (out, _, _) = run("if fail; then echo A\nelif true; then echo B\nelse echo C\nfi\nfail\necho $?");
    assert_eq!(out, "B\n1\n");
}

#[test]
fn and_or_lists() {
    let (out, _, _) = run("fail && echo no || echo yes\ntrue && echo ok");
    assert_eq!(out, "yes\nok\n");
}

#[test]
fn test_builtin() {
    let (out, _, _) = run("[ -f file ] && echo f\n[ -d dir ] && echo d\n[ -e nope ] || echo n\n[ 3 -lt 10 ] && echo lt\n[ a = a ] && echo eq\ntest -z \"\" && echo z\n[ ! -e nope ] && echo neg");
    assert_eq!(out, "f\nd\nn\nlt\neq\nz\nneg\n");
}

#[test]
fn loops() {
    let (out, _, _) = run("for i in a b c; do echo $i; done\nN=0\nwhile [ $N -lt 3 ]; do N=$((N+1)); echo n$N; done\nuntil [ $N -eq 0 ]; do N=$((N-1)); done\necho $N");
    assert_eq!(out, "a\nb\nc\nn1\nn2\nn3\n0\n");
}

#[test]
fn break_continue_exit() {
    let (out, status, _) = run("for i in 1 2 3 4; do [ $i -eq 2 ] && continue; [ $i -eq 4 ] && break; echo $i; done\nexit 7\necho never");
    assert_eq!(out, "1\n3\n");
    assert_eq!(status, 7);
}

#[test]
fn nested_break_two() {
    let (out, _, _) = run("for i in 1 2; do for j in a b; do echo $i$j; break 2; done; done\necho end");
    assert_eq!(out, "1a\nend\n");
}

#[test]
fn command_substitution_and_arithmetic() {
    let (out, _, _) = run("X=$(echo hi; echo there)\necho \"[$X]\" $((2 + 3 * 4)) $(((1+2)*3)) $((10 % 4)) $((-5 + 2))\necho $(ls)");
    assert_eq!(out, "[hi\nthere] 14 9 2 -3\n<ls>\n");
}

#[test]
fn redirection_captures_output() {
    let (out, _, env) = run("echo one > f\necho two >> f\nls > g\necho done");
    assert_eq!(out, "done\n");
    assert_eq!(env.files, [("f".to_string(), "one\ntwo\n".to_string()), ("g".to_string(), "<ls>\n".to_string())]);
}

#[test]
fn globbing_keeps_names_whole() {
    let (_, _, env) = run("for f in *.txt; do cp \"$f\" /x; done\nrm *.zip");
    assert_eq!(env.ran, ["cp a.txt /x", "cp \"b c.txt\" /x", "rm *.zip"]);
}

#[test]
fn trace_and_set() {
    let (out, _, _) = run("set -x\necho hi\nset +x\nset A=1 B=2\necho $A$B\nshift\necho $1");
    assert_eq!(out, "+ echo hi\nhi\n+ set +x\n12\ntwo words\n");
}

#[test]
fn syntax_errors_are_reported() {
    for bad in ["if true; then echo", "fi", "echo 'x", "echo > ", "a | b", "for do"] {
        let (out, status, _) = run(bad);
        assert_eq!(status, 2, "{bad}");
        assert!(out.starts_with("script: "), "{bad}: {out}");
    }
}

#[test]
fn division_by_zero_does_not_panic() {
    let (out, status, _) = run("echo $((1/0))");
    assert!(out.contains("division by zero"));
    assert_eq!(status, 2);
}
