//! Port of modules/calculator/module.py: `calculate` evaluates an arithmetic expression with a
//! simpleeval-compatible evaluator (see calculator/eval.rs).

pub mod eval;

#[path = "../../../generated/modules/calculator.rs"]
pub mod meta;

use async_trait::async_trait;
use ol_core::module::{Module, ModuleBase, ToolArgs, ToolSpec};
use serde_json::Value;
use std::sync::Arc;

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

    /// python: `simpleeval.simple_eval(expression)`. Any exception becomes an error result carrying
    /// `core.detail_error(e)`; a success carries the value as python `str()` renders it.
    pub async fn calculate(&self, expression: &str) -> anyhow::Result<Value> {
        Ok(match eval::simple_eval(expression) {
            Ok(v) => match eval::format_result(&v) {
                Ok(text) => self.base.result(text, true),
                Err(e) => self.base.result(ol_core::detail_error(&e), false),
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
    use super::eval::simple_eval;
    use super::*;

    /// (ok, content) the way the tool returns it
    fn run(expr: &str) -> (bool, String) {
        match simple_eval(expr) {
            Ok(v) => (true, eval::format_result(&v).unwrap_or_default()),
            Err(e) => (false, ol_core::detail_error(&e)),
        }
    }

    #[test]
    fn arithmetic_results_match_python_str() {
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
        assert_eq!(run("2 ** 100"), (true, "1267650600228229401496703205376".into()));
        assert_eq!(run("True + True"), (true, "2".into()));
        assert_eq!(run("1 < 2 < 3"), (true, "True".into()));
        assert_eq!(run("None"), (true, "None".into()));
        assert_eq!(run("'ab' + 'cd'"), (true, "abcd".into()));
        assert_eq!(run("int('42') + 1"), (true, "43".into()));
        assert_eq!(run("3 if 1 > 2 else 4"), (true, "4".into()));
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
    }
}
