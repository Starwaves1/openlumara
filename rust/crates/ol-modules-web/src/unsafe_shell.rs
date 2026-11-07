//! Port of modules/unsafe_shell/module.py: runs shell commands on the host with full access.
//! Python's `subprocess.run(cmd, capture_output=True, shell=True, text=True)` becomes
//! `/bin/sh -c <cmd>` with both pipes captured, decoded like text mode (strict UTF-8,
//! universal newlines) and stripped the way `str.strip()` strips.

#[path = "../../../generated/modules/unsafe_shell.rs"]
pub mod meta;

use anyhow::anyhow;
use async_trait::async_trait;
use ol_core::module::{CommandSpec, Module, ModuleBase, ToolArgs, ToolSpec};
use serde_json::{json, Value};
use std::os::unix::process::ExitStatusExt;
use std::process::{ExitStatus, Stdio};
use std::str::Utf8Error;
use std::sync::Arc;

pub struct UnsafeShell {
    base: ModuleBase,
}

/// python `str.strip()`: unicode whitespace plus the ASCII separators \x1c-\x1f
fn py_strip(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c))
}

/// python's strict utf-8 decode error text (`str(UnicodeDecodeError)`)
fn py_utf8_error(bytes: &[u8], e: &Utf8Error) -> String {
    let start = e.valid_up_to();
    let end = match e.error_len() {
        Some(n) => start + n,
        None => bytes.len(),
    };
    let reason = match e.error_len() {
        None => "unexpected end of data",
        Some(_) => {
            let b = bytes[start];
            if (0x80..=0xC1).contains(&b) || b >= 0xF5 {
                "invalid start byte"
            } else {
                "invalid continuation byte"
            }
        }
    };
    if end - start == 1 {
        format!("'utf-8' codec can't decode byte 0x{:02x} in position {}: {}", bytes[start], start, reason)
    } else {
        format!("'utf-8' codec can't decode bytes in position {}-{}: {}", start, end - 1, reason)
    }
}

/// `text=True` decoding: strict utf-8, then universal newlines (\r\n and \r become \n)
fn py_text(bytes: &[u8]) -> anyhow::Result<String> {
    let s = std::str::from_utf8(bytes).map_err(|e| anyhow!(py_utf8_error(bytes, &e)))?;
    Ok(s.replace("\r\n", "\n").replace('\r', "\n"))
}

/// python `returncode`: negative signal number when the process was killed by a signal
fn exit_code(status: ExitStatus) -> i64 {
    match (status.code(), status.signal()) {
        (Some(c), _) => c as i64,
        (None, Some(sig)) => -(sig as i64),
        (None, None) => 0,
    }
}

/// python type name of a json value, as it appears in TypeError messages
fn py_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// argv that python's `subprocess.run(cmd, shell=True)` builds as `['/bin/sh', '-c', *cmd]`.
/// a str is one script; a list (or a dict, iterated by its keys) supplies the script and $0..;
/// any other value raises the TypeError python raises before anything runs.
fn shell_argv(cmd: Option<&Value>) -> anyhow::Result<Vec<String>> {
    match cmd {
        None => anyhow::bail!("UnsafeShell.exec() missing 1 required positional argument: 'cmd'"),
        Some(Value::String(s)) => Ok(vec![s.clone()]),
        Some(Value::Array(items)) => items.iter().map(argv_item).collect(),
        Some(Value::Object(map)) => Ok(map.keys().cloned().collect()),
        Some(other) => anyhow::bail!("'{}' object is not iterable", py_type_name(other)),
    }
}

/// one element of a list cmd; popen only accepts str arguments. popen converts the items in
/// index order and stops at the first bad one, so the NUL check belongs here, per item, and
/// the first failing item decides the error (["a\0b", 3] is ValueError, [3, "a\0b"] is TypeError).
fn argv_item(v: &Value) -> anyhow::Result<String> {
    match v {
        Value::String(s) if s.contains('\0') => anyhow::bail!("embedded null byte"),
        Value::String(s) => Ok(s.clone()),
        other => anyhow::bail!("expected str, bytes or os.PathLike object, not {}", py_type_name(other)),
    }
}

/// python cmd_exec's output assembly: stdout, then "\n" + stderr, else "BLANK"
pub fn render_output(stdout: &str, stderr: &str) -> String {
    let mut output = String::new();
    if !stdout.is_empty() {
        output.push_str(stdout);
    }
    if !stderr.is_empty() {
        output.push('\n');
        output.push_str(stderr);
    }
    if output.is_empty() {
        "BLANK".to_string()
    } else {
        output
    }
}

impl UnsafeShell {
    pub fn factory(base: ModuleBase) -> Arc<dyn Module> {
        Arc::new(Self { base })
    }

    /// python exec(): runs the command and returns `self.result({stdout, stderr, exit_code})`
    pub async fn exec(&self, cmd: &str) -> anyhow::Result<Value> {
        self.run_argv(&[cmd.to_string()]).await
    }

    /// the tool-call path: `exec(cmd=<json value>)` with python's subprocess argument rules
    async fn exec_tool(&self, cmd: Option<&Value>) -> anyhow::Result<Value> {
        let argv = shell_argv(cmd)?;
        self.run_argv(&argv).await
    }

    /// `/bin/sh -c <argv...>` with stdout/stderr captured, as subprocess.run(capture_output=True).
    /// python's exec() is async in name only: subprocess.run blocks until the command ends, so the
    /// tool timeout can never interrupt it and the real output is returned. block_in_place runs the
    /// blocking wait the same way (the tool future does not yield until the command is done).
    async fn run_argv(&self, argv: &[String]) -> anyhow::Result<Value> {
        // popen rejects any argument containing a NUL byte before spawning anything
        if argv.iter().any(|a| a.contains('\0')) {
            anyhow::bail!("embedded null byte");
        }
        let out = tokio::task::block_in_place(|| {
            std::process::Command::new("/bin/sh")
                .arg("-c")
                .args(argv)
                .stdin(Stdio::inherit())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
        })?;
        let stdout = py_text(&out.stdout)?;
        let stderr = py_text(&out.stderr)?;
        Ok(self.base.result(
            json!({
                "stdout": py_strip(&stdout),
                "stderr": py_strip(&stderr),
                "exit_code": exit_code(out.status),
            }),
            true,
        ))
    }

    /// python cmd_exec(): arguments are re-joined with spaces; errors become a string
    pub async fn cmd_exec(&self, args: &[String]) -> String {
        let cmd = args.join(" ");
        let result = match self.exec(&cmd).await {
            Ok(r) => r,
            Err(e) => return format!("error while running shell command: {}", e),
        };
        let content = result.get("content").cloned().unwrap_or(Value::Null);
        let stdout = content.get("stdout").and_then(|v| v.as_str()).unwrap_or("");
        let stderr = content.get("stderr").and_then(|v| v.as_str()).unwrap_or("");
        render_output(stdout, stderr)
    }
}

#[async_trait]
impl Module for UnsafeShell {
    fn base(&self) -> &ModuleBase {
        &self.base
    }

    fn tools(&self) -> Vec<ToolSpec> {
        meta::tools()
    }

    async fn call_tool(&self, method: &str, args: ToolArgs) -> anyhow::Result<Value> {
        match method {
            "exec" => {
                // python `exec(**args)`: an unexpected keyword is bound before the missing-argument check
                if let Some(extra) = args.0.keys().find(|k| k.as_str() != "cmd") {
                    anyhow::bail!("UnsafeShell.exec() got an unexpected keyword argument '{}'", extra);
                }
                self.exec_tool(args.0.get("cmd")).await
            }
            _ => anyhow::bail!("'UnsafeShell' object has no attribute '{}'", method),
        }
    }

    fn commands(&self) -> Vec<CommandSpec> {
        meta::commands().into_iter().map(|(c, _)| c).collect()
    }

    async fn run_command(&self, name: &str, args: Vec<String>) -> anyhow::Result<Option<String>> {
        if name != "shell" {
            anyhow::bail!("unknown command {}", name);
        }
        Ok(Some(self.cmd_exec(&args).await))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_output_matches_python_assembly() {
        assert_eq!(render_output("", ""), "BLANK");
        assert_eq!(render_output("out", ""), "out");
        assert_eq!(render_output("", "err"), "\nerr");
        assert_eq!(render_output("a", "b"), "a\nb");
    }

    #[test]
    fn text_decode_translates_newlines_and_reports_python_utf8_errors() {
        assert_eq!(py_text(b"x\r\ny\rz").unwrap(), "x\ny\nz");
        let err = py_text(b"\xff").unwrap_err().to_string();
        assert_eq!(err, "'utf-8' codec can't decode byte 0xff in position 0: invalid start byte");
        let err = py_text(b"ab\xe2\x82").unwrap_err().to_string();
        assert_eq!(err, "'utf-8' codec can't decode bytes in position 2-3: unexpected end of data");
    }

    #[test]
    fn strip_follows_python_whitespace() {
        assert_eq!(py_strip("\x1c  hi \u{3000}\n"), "hi");
    }

    // block_in_place needs the multi-thread runtime (the app runs on one), so every test here uses it

    #[tokio::test(flavor = "multi_thread")]
    async fn exec_returns_stdout_stderr_and_exit_code() {
        let m = UnsafeShell::new_for_test();
        let r = m.exec("printf '  hi  '; echo oops >&2; exit 3").await.unwrap();
        assert_eq!(r, json!({"status": "success", "content": {"stdout": "hi", "stderr": "oops", "exit_code": 3}}));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn exec_waits_past_tool_timeout_like_python_subprocess_run() {
        // python: a 2s command under a 0.5s wait_for still returns its output
        let m = UnsafeShell::new_for_test();
        let r = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            m.exec("sleep 0.3; echo done"),
        )
        .await
        .expect("exec must not be interrupted by an outer timeout")
        .unwrap();
        assert_eq!(r["content"]["stdout"], json!("done"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn exec_nul_byte_raises_python_message_before_running() {
        let m = UnsafeShell::new_for_test();
        let err = m.exec_tool(Some(&json!("echo a\u{0}b"))).await.unwrap_err().to_string();
        assert_eq!(err, "embedded null byte");
        let err = m.exec_tool(Some(&json!(["echo", "a\u{0}b"]))).await.unwrap_err().to_string();
        assert_eq!(err, "embedded null byte");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn exec_tool_list_error_follows_python_item_order() {
        // python popen converts argv items in order: the first bad item decides the error
        let m = UnsafeShell::new_for_test();
        let err = m.exec_tool(Some(&json!(["a\u{0}b", 3]))).await.unwrap_err().to_string();
        assert_eq!(err, "embedded null byte");
        let err = m.exec_tool(Some(&json!([3, "a\u{0}b"]))).await.unwrap_err().to_string();
        assert_eq!(err, "expected str, bytes or os.PathLike object, not int");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn exec_reports_signal_as_negative_exit_code() {
        let m = UnsafeShell::new_for_test();
        let r = m.exec("kill -9 $$").await.unwrap();
        assert_eq!(r["content"]["exit_code"], json!(-9));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn shell_command_joins_args_and_renders_output() {
        let m = UnsafeShell::new_for_test();
        let out = m.run_command("shell", vec!["echo".into(), "hello".into()]).await.unwrap();
        assert_eq!(out.as_deref(), Some("hello"));
        let out = m.run_command("shell", vec!["true".into()]).await.unwrap();
        assert_eq!(out.as_deref(), Some("BLANK"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn exec_tool_call_returns_result_dict() {
        let m = UnsafeShell::new_for_test();
        let mut map = serde_json::Map::new();
        map.insert("cmd".into(), json!("echo tool"));
        let r = m.call_tool("exec", ToolArgs::new(map)).await.unwrap();
        assert_eq!(r["content"]["stdout"], json!("tool"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn exec_tool_list_cmd_runs_as_sh_argv() {
        // python: subprocess.run(["echo $0", "hi"], shell=True) runs `sh -c 'echo $0' hi`
        let m = UnsafeShell::new_for_test();
        let r = m.exec_tool(Some(&json!(["echo $0", "hi"]))).await.unwrap();
        assert_eq!(r["content"]["stdout"], json!("hi"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn exec_tool_unexpected_kwarg_reported_before_missing_cmd() {
        // python: exec(**{"extra": 1}) -> unexpected keyword is bound before the missing 'cmd' check
        let m = UnsafeShell::new_for_test();
        let mut map = serde_json::Map::new();
        map.insert("extra".into(), json!(1));
        let err = m.call_tool("exec", ToolArgs::new(map)).await.unwrap_err().to_string();
        assert_eq!(err, "UnsafeShell.exec() got an unexpected keyword argument 'extra'");

        // the first unexpected key in dict order is the one reported
        let mut map = serde_json::Map::new();
        map.insert("cmd".into(), json!("echo hi"));
        map.insert("x".into(), json!(1));
        map.insert("y".into(), json!(2));
        let err = m.call_tool("exec", ToolArgs::new(map)).await.unwrap_err().to_string();
        assert_eq!(err, "UnsafeShell.exec() got an unexpected keyword argument 'x'");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn exec_tool_non_string_cmd_raises_before_running() {
        let m = UnsafeShell::new_for_test();
        let err = m.exec_tool(Some(&json!(5))).await.unwrap_err().to_string();
        assert_eq!(err, "'int' object is not iterable");
        let err = m.exec_tool(Some(&Value::Null)).await.unwrap_err().to_string();
        assert_eq!(err, "'NoneType' object is not iterable");
        let err = m.exec_tool(Some(&json!(["echo", 3]))).await.unwrap_err().to_string();
        assert_eq!(err, "expected str, bytes or os.PathLike object, not int");
        let err = m.exec_tool(None).await.unwrap_err().to_string();
        assert_eq!(err, "UnsafeShell.exec() missing 1 required positional argument: 'cmd'");
    }

    impl UnsafeShell {
        fn new_for_test() -> Self {
            Self { base: ModuleBase::detached("unsafe_shell") }
        }
    }
}
