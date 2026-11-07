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
            "calculate" => match args.0.get("expression") {
                // python: a missing argument is a TypeError at the call site, outside the module's try
                None => anyhow::bail!("Calculator.calculate() missing 1 required positional argument: 'expression'"),
                Some(Value::String(expression)) => self.calculate(expression).await,
                // python: simple_eval() calls expression.strip() first, which fails for non-strings
                Some(other) => Ok(self.base.result(format!("'{}' object has no attribute 'strip'", crate::util::py_type_name(other)), false)),
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

    #[test]
    fn builtin_call_errors_match_cpython_text() {
        // rand is random.random, so its messages name Random.random
        assert_eq!(run("rand(1)"), (false, "Random.random() takes no arguments (1 given)".into()));
        assert_eq!(run("rand(x=1)"), (false, "Random.random() takes no keyword arguments".into()));
        // int() counts keyword arguments with the positional ones before it looks at their names
        assert_eq!(run("int('1', base=2, x=3)"), (false, "int() takes at most 2 arguments (3 given)".into()));
        assert_eq!(run("int('1', 2, base=2)"), (false, "int() takes at most 2 arguments (3 given)".into()));
        // str(obj, encoding) checks the encoding type first
        assert_eq!(run("str(1, 2)"), (false, "str() argument 'encoding' must be str, not int".into()));
        assert_eq!(run("str(1, 'utf8')"), (false, "decoding to str: need a bytes-like object, int found".into()));
    }

    #[test]
    fn index_conversion_and_refused_displays_match_cpython() {
        // the index is converted to a Py_ssize_t before the range check
        assert_eq!(run("'abc'[10**400]"), (false, "cannot fit 'int' into an index-sized integer".into()));
        assert_eq!(run("'abc'[-10**400]"), (false, "cannot fit 'int' into an index-sized integer".into()));
        assert_eq!(run("'abc'[10]"), (false, "string index out of range".into()));
        assert_eq!(run("'abc'[-1]"), (true, "\"c\"".into()));
        // unpacking inside a display makes the display node that simpleeval refuses
        assert_eq!(run("{**x}"), (false, "Sorry, Dict is not available in this evaluator".into()));
        assert_eq!(run("[*'ab']"), (false, "Sorry, List is not available in this evaluator".into()));
        assert_eq!(run("{*x}"), (false, "Sorry, Set is not available in this evaluator".into()));
        // a tuple index: the container is looked up first, then the Tuple node is refused
        assert_eq!(run("x[1:2, 3]"), (false, "'x' is not defined for expression 'x[1:2, 3]'".into()));
        assert_eq!(run("'abc'[1:2, 3]"), (false, "Sorry, Tuple is not available in this evaluator".into()));
    }

    #[test]
    fn numeric_literal_errors_match_cpython_tokenizer() {
        assert_eq!(run("0x"), (false, "invalid hexadecimal literal (<unknown>, line 1)".into()));
        assert_eq!(run("0o8"), (false, "invalid digit '8' in octal literal (<unknown>, line 1)".into()));
        assert_eq!(run("0b2"), (false, "invalid digit '2' in binary literal (<unknown>, line 1)".into()));
        assert_eq!(run("1e"), (false, "invalid decimal literal (<unknown>, line 1)".into()));
        assert_eq!(run("1e+"), (false, "invalid decimal literal (<unknown>, line 1)".into()));
        assert_eq!(run("1e5"), (true, "100000.0".into()));
    }

    #[test]
    fn string_escape_and_lexer_errors_match_cpython_text() {
        assert_eq!(
            run("'\\x4'"),
            (false, "(unicode error) 'unicodeescape' codec can't decode bytes in position 0-2: truncated \\xXX escape (<unknown>, line 1)".into())
        );
        assert_eq!(
            run("'ab\\x4'"),
            (false, "(unicode error) 'unicodeescape' codec can't decode bytes in position 2-4: truncated \\xXX escape (<unknown>, line 1)".into())
        );
        assert_eq!(
            run("'\\U00110000'"),
            (false, "(unicode error) 'unicodeescape' codec can't decode bytes in position 0-9: illegal Unicode character (<unknown>, line 1)".into())
        );
        assert_eq!(run("1 +\\"), (false, "unexpected EOF while parsing (<unknown>, line 1)".into()));
        assert_eq!(run("(\u{1})"), (false, "invalid non-printable character U+0001 (<unknown>, line 1)".into()));
        assert_eq!(run("1 +\u{a0}2"), (false, "invalid non-printable character U+00A0 (<unknown>, line 1)".into()));
        assert_eq!(run("1\u{3000}+ 1"), (false, "invalid non-printable character U+3000 (<unknown>, line 1)".into()));
        // an operator at the end of a line is what cpython's parser fails on, before the indent
        assert_eq!(run("1 +\n 2"), (false, "invalid syntax (<unknown>, line 1)".into()));
        assert_eq!(run("1\n 2"), (false, "unexpected indent (<unknown>, line 2)".into()));
        // printable non-ascii characters keep their own message
        assert_eq!(run("1 € 2"), (false, "invalid character '€' (U+20AC) (<unknown>, line 1)".into()));
    }

    #[test]
    fn assignment_targets_match_cpython_compile_errors() {
        // simpleeval evaluates only the value of an assignment, so a valid tuple target gives the value
        assert_eq!(run("a, b = 1"), (true, "1".into()));
        assert_eq!(run("(a, b) = 1"), (true, "1".into()));
        assert_eq!(run("a, b = 1, 2"), (false, "Sorry, Tuple is not available in this evaluator".into()));
        assert_eq!(run("a, 1 = 2"), (false, "cannot assign to literal here. Maybe you meant '==' instead of '='? (<unknown>, line 1)".into()));
        // the '==' hint only applies to targets that cpython parses as a bitwise_or
        assert_eq!(run("x and y = 1"), (false, "cannot assign to expression (<unknown>, line 1)".into()));
        assert_eq!(run("not x = 1"), (false, "cannot assign to expression (<unknown>, line 1)".into()));
        assert_eq!(run("-x = 1"), (false, "cannot assign to expression here. Maybe you meant '==' instead of '='? (<unknown>, line 1)".into()));
        assert_eq!(run("(a, b) += 1"), (false, "'tuple' is an illegal expression for augmented assignment (<unknown>, line 1)".into()));
    }

    fn call(args: Value) -> anyhow::Result<Value> {
        let calc = Calculator::new(ModuleBase::detached("calculator"));
        let args = ToolArgs::new(args.as_object().unwrap().clone());
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(calc.call_tool("calculate", args))
    }

    #[test]
    fn call_tool_returns_the_python_result_and_argument_errors() {
        let base = ModuleBase::detached("calculator");
        let ok = call(serde_json::json!({"expression": "2 ** 10"})).unwrap();
        assert_eq!(ok, base.result(serde_json::json!(1024), true));
        // a non-string expression fails inside python's try (str.strip is missing), as an error result
        let bad = call(serde_json::json!({"expression": 5})).unwrap();
        assert_eq!(bad, base.result("'int' object has no attribute 'strip'".to_string(), false));
        // a missing argument is a TypeError at the call site, outside the module's try, so it escapes
        let missing = call(serde_json::json!({})).unwrap_err().to_string();
        assert_eq!(missing, "Calculator.calculate() missing 1 required positional argument: 'expression'");
    }

    #[test]
    fn parsed_but_refused_nodes_name_their_node() {
        assert_eq!(run("yield 1"), (false, "Sorry, Yield is not available in this evaluator".into()));
        assert_eq!(run("await x"), (false, "Sorry, Await is not available in this evaluator".into()));
        assert_eq!(run("(x := 1)"), (false, "Sorry, NamedExpr is not available in this evaluator".into()));
    }

    #[test]
    fn unpacked_arguments_are_evaluated_before_the_call_like_simpleeval() {
        // the `**` operand is evaluated (so an undefined name is reported), then python refuses the None key
        assert_eq!(run("int(**x)"), (false, "'x' is not defined for expression 'int(**x)'".into()));
        assert_eq!(run("int(**1)"), (false, "keywords must be strings".into()));
        assert_eq!(run("randint(1, 2, **1)"), (false, "keywords must be strings".into()));
        assert_eq!(run("int(**x, 1)"), (false, "positional argument follows keyword argument unpacking (<unknown>, line 1)".into()));
        assert_eq!(run("int(**x, *y)"), (false, "iterable argument unpacking follows keyword argument unpacking (<unknown>, line 1)".into()));
        // a `*` operand is refused without being evaluated
        assert_eq!(run("int(*x)"), (false, "Sorry, Starred is not available in this evaluator".into()));
        assert_eq!(run("*a"), (false, "Sorry, Starred is not available in this evaluator".into()));
        assert_eq!(run("x = *a"), (false, "Sorry, Starred is not available in this evaluator".into()));
        // a starred target is valid, and simpleeval evaluates only the value
        assert_eq!(run("*a = 1"), (true, "1".into()));
        assert_eq!(run("*a, b = 1"), (true, "1".into()));
        assert_eq!(run("*a += 1"), (false, "'starred' is an illegal expression for augmented assignment (<unknown>, line 1)".into()));
    }

    #[test]
    fn type_objects_and_builtin_binding_match_cpython() {
        assert_eq!(run("int[0]"), (false, "type 'int' is not subscriptable".into()));
        assert_eq!(run("str[0]"), (false, "type 'str' is not subscriptable".into()));
        assert_eq!(run("float[1:2]"), (false, "type 'float' is not subscriptable".into()));
        assert_eq!(run("rand[0]"), (false, "'builtin_function_or_method' object is not subscriptable".into()));
        // without keywords int reports its positional-only count
        assert_eq!(run("int(1,2,3)"), (false, "int expected at most 2 arguments, got 3".into()));
        assert_eq!(run("int('1',2,3)"), (false, "int expected at most 2 arguments, got 3".into()));
        // randint: keywords are bound before the positional count is checked; a repeated top= keeps the last
        assert_eq!(run("randint(1, top=3)"), (false, "random_int() got multiple values for argument 'top'".into()));
        assert_eq!(run("randint(1, 2)"), (false, "random_int() takes 1 positional argument but 2 were given".into()));
        assert_eq!(run("randint(top=2, top=0)"), (true, "0".into()));
        // str: keywords bind the same parameters as positions, and a missing object gives ''
        assert_eq!(run("str(1, encoding=1)"), (false, "str() argument 'encoding' must be str, not int".into()));
        assert_eq!(run("str(1, encoding=None)"), (false, "str() argument 'encoding' must be str, not None".into()));
        assert_eq!(run("str(encoding='x')"), (true, "\"\"".into()));
        assert_eq!(run("str(errors='x')"), (true, "\"\"".into()));
        assert_eq!(run("str(1, object=2)"), (false, "argument for str() given by name ('object') and position (1)".into()));
        assert_eq!(run("str(None, 'x')"), (false, "decoding to str: need a bytes-like object, NoneType found".into()));
        assert_eq!(run("str(1,'x','y',errors='z')"), (false, "str() takes at most 3 arguments (4 given)".into()));
    }

    #[test]
    fn assignment_hint_only_applies_to_the_first_target() {
        assert_eq!(run("1 = 2"), (false, "cannot assign to literal here. Maybe you meant '==' instead of '='? (<unknown>, line 1)".into()));
        assert_eq!(run("x = 2 = 3"), (false, "cannot assign to literal (<unknown>, line 1)".into()));
        assert_eq!(run("x = 1 = 2"), (false, "cannot assign to literal (<unknown>, line 1)".into()));
    }
}
