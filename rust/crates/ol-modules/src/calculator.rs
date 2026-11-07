//! Port of modules/calculator/module.py: `calculate` evaluates an arithmetic expression with a
//! simpleeval-compatible evaluator (see calculator/eval.rs).

pub mod eval;

#[path = "../../../generated/modules/calculator.rs"]
pub mod meta;

use async_trait::async_trait;
use ol_core::module::{Module, ModuleBase, ToolArgs, ToolSpec};
use serde_json::Value;
use std::sync::Arc;

/// stack for the evaluator thread: the parser recurses once per nested bracket (up to 200) and the
/// evaluator once per nested node, which a 2 MiB worker stack cannot hold in a debug build
const EVAL_STACK_BYTES: usize = 64 << 20;

/// runs `eval::simple_eval` on its own thread, so deep (but legal) input cannot overflow the caller's stack.
/// The outer error is only for a failed thread start or a panic inside the evaluator.
pub fn evaluate(expression: &str) -> anyhow::Result<Result<eval::Val, eval::PyErr>> {
    let expression = expression.to_string();
    let handle = std::thread::Builder::new()
        .name("calculator".into())
        .stack_size(EVAL_STACK_BYTES)
        .spawn(move || eval::simple_eval(&expression))?;
    handle.join().map_err(|_| anyhow::anyhow!("calculator evaluation panicked"))
}

pub struct Calculator {
    base: ModuleBase,
}

impl Calculator {
    pub fn factory(base: ModuleBase) -> Arc<dyn Module> {
        Arc::new(Self::new(base))
    }

    pub fn new(base: ModuleBase) -> Self {
        Self { base }
    }

    /// python: `simpleeval.simple_eval(expression)`. An exception becomes an error result carrying
    /// `core.detail_error(e)`. A success carries the raw value, which the dispatcher json.dumps, so the
    /// model sees a number, bool or null rather than its str(). When json.dumps itself would raise
    /// (functions, classes), python lets that exception escape the module, so it is returned as an Err
    /// here and the dispatcher reports it as a tool error.
    pub async fn calculate(&self, expression: &str) -> anyhow::Result<Value> {
        Ok(match evaluate(expression)? {
            Ok(v) => match eval::to_json(&v) {
                Ok(content) => self.base.result(content, true),
                Err(e) => anyhow::bail!("{e}"),
            },
            Err(e) => self.base.result(ol_core::detail_error(&e), false),
        })
    }
}

#[async_trait]
impl Module for Calculator {
    fn base(&self) -> &ModuleBase {
        &self.base
    }

    fn tools(&self) -> Vec<ToolSpec> {
        meta::tools()
    }

    async fn call_tool(&self, method: &str, args: ToolArgs) -> anyhow::Result<Value> {
        match method {
            "calculate" => match args.0.get("expression").cloned().unwrap_or(Value::Null) {
                Value::String(expression) => self.calculate(&expression).await,
                // python: simple_eval() calls expression.strip() first, which fails for non-strings
                other => Ok(self.base.result(format!("'{}' object has no attribute 'strip'", crate::util::py_type_name(&other)), false)),
            },
            _ => anyhow::bail!("'Calculator' object has no attribute '{}'", method),
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::eval::{to_json, Func, Val};
    use super::*;

    /// (ok, content) the way the tool returns it: content is `json.dumps` of the value on success,
    /// the error text on failure
    fn run(expr: &str) -> (bool, String) {
        match evaluate(expr).expect("calculator thread") {
            Ok(v) => match to_json(&v) {
                Ok(content) => (true, ol_core::pyjson::dumps(&content)),
                Err(e) => (false, e.to_string()),
            },
            Err(e) => (false, ol_core::detail_error(&e)),
        }
    }

    #[test]
    fn arithmetic_results_are_json_values_like_python() {
        assert_eq!(run("2 + 2 * 3"), (true, "8".into()));
        assert_eq!(run("8 / 2"), (true, "4.0".into()));
        assert_eq!(run("7 / 2"), (true, "3.5".into()));
        assert_eq!(run("-7 // 2"), (true, "-4".into()));
        assert_eq!(run("-7 % 3"), (true, "2".into()));
        assert_eq!(run("2 ** 10"), (true, "1024".into()));
        assert_eq!(run("2 ** -1"), (true, "0.5".into()));
        assert_eq!(run("-2 ** 2"), (true, "-4".into()));
        assert_eq!(run("0.1 + 0.2"), (true, "0.30000000000000004".into()));
        assert_eq!(run("1e16"), (true, "1e+16".into()));
        assert_eq!(run("2 ** 60"), (true, "1152921504606846976".into()));
        assert_eq!(run("True + True"), (true, "2".into()));
        assert_eq!(run("1 < 2 < 3"), (true, "true".into()));
        assert_eq!(run("None"), (true, "null".into()));
        assert_eq!(run("'ab' + 'cd'"), (true, "\"abcd\"".into()));
        assert_eq!(run("int('42') + 1"), (true, "43".into()));
        assert_eq!(run("3 if 1 > 2 else 4"), (true, "4".into()));
    }

    #[test]
    fn division_and_shift_follow_simpleeval() {
        // correctly rounded int / int, even when both operands are beyond float range
        assert_eq!(run("10**400 / 10**399"), (true, "10.0".into()));
        assert_eq!(run("2**10000 / 2**9999"), (true, "2.0".into()));
        assert_eq!(run("1/10**400"), (true, "0.0".into()));
        assert_eq!(run("0/-5"), (true, "-0.0".into()));
        assert_eq!(run("10**400 / 3"), (false, "integer division result too large for a float".into()));
        assert_eq!(run("10**400 / 2.0"), (false, "int too large to convert to float".into()));
        // the shift guards apply to every operand type, floats included
        assert_eq!(run("1.0 << 100000"), (false, "Sorry! I don't want to evaluate 1.0 << 100000".into()));
        assert_eq!(run("1e300 << 2"), (false, "unsupported operand type(s) for <<: 'float' and 'int'".into()));
        assert_eq!(run("'a' << 2"), (false, "bad operand type for abs(): 'str'".into()));
    }

    #[test]
    fn digit_limit_and_nesting_limits_match_cpython() {
        assert_eq!(
            run("int('1'*5000)"),
            (false, "Exceeds the limit (4300 digits) for integer string conversion: value has 5000 digits; use sys.set_int_max_str_digits() to increase the limit".into())
        );
        assert_eq!(
            run("10**4300"),
            (false, "Exceeds the limit (4300 digits) for integer string conversion; use sys.set_int_max_str_digits() to increase the limit".into())
        );
        assert_eq!(run(&format!("{}1{}", "(".repeat(200), ")".repeat(200))), (true, "1".into()));
        assert_eq!(run(&format!("{}1{}", "(".repeat(201), ")".repeat(201))), (false, "too many nested parentheses (<unknown>, line 1)".into()));
        assert_eq!(run(&format!("{}1", "-".repeat(600))), (false, "maximum recursion depth exceeded".into()));
    }

    #[test]
    fn syntax_errors_match_cpython_hints() {
        assert_eq!(run("(1 2)"), (false, "invalid syntax. Perhaps you forgot a comma? (<unknown>, line 1)".into()));
        assert_eq!(run("(a b)"), (false, "invalid syntax. Perhaps you forgot a comma? (<unknown>, line 1)".into()));
        assert_eq!(run("1 2"), (false, "invalid syntax (<unknown>, line 1)".into()));
        assert_eq!(run("1__0"), (false, "invalid decimal literal (<unknown>, line 1)".into()));
        assert_eq!(run("1_000"), (true, "1000".into()));
    }

    #[test]
    fn errors_match_simpleeval_messages() {
        assert_eq!(run("1 / 0"), (false, "division by zero".into()));
        assert_eq!(run("foo + 1"), (false, "'foo' is not defined for expression 'foo + 1'".into()));
        assert_eq!(run("round(2.5)"), (false, "Function 'round' not defined, for expression 'round(2.5)'.".into()));
        assert_eq!(run("import os"), (false, "Sorry, 'import' is not allowed.".into()));
        assert_eq!(run("[1, 2]"), (false, "Sorry, List is not available in this evaluator".into()));
        assert_eq!(run("   "), (false, "Sorry, cannot evaluate empty string".into()));
        assert_eq!(run("2 ** 5000000"), (false, "Sorry! I don't want to evaluate 2 ** 5000000".into()));
        assert_eq!(run("1 + 'a'"), (false, "unsupported operand type(s) for +: 'int' and 'str'".into()));
        assert_eq!(run("0 ** -1"), (false, "zero to a negative power".into()));
        assert_eq!(run("(lambda: 1)()"), (false, "Lambda Functions not implemented".into()));
    }

    #[test]
    fn function_values_fail_like_json_dumps() {
        // python: json.dumps(int) raises TypeError outside the tool's try block
        assert_eq!(to_json(&Val::Func(Func::Int)).unwrap_err().to_string(), "Object of type type is not JSON serializable");
        assert_eq!(to_json(&Val::Func(Func::Rand)).unwrap_err().to_string(), "Object of type builtin_function_or_method is not JSON serializable");
        assert_eq!(to_json(&Val::Func(Func::RandInt)).unwrap_err().to_string(), "Object of type function is not JSON serializable");
    }
}
