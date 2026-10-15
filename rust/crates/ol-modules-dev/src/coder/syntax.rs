//! Port of the parts of `tree_sitter_language_pack` the coder uses:
//! `detect_language_from_path`, `process(..., diagnostics=True)` and
//! `process(..., structure=True, imports=True)`.
//!
//! The python package is a binding over the Rust crate `tree-sitter-language-pack`
//! (which downloads/compiles ~370 grammars at build time). We compile in a curated set of
//! grammars instead and replicate the crate's diagnostics/structure/import extraction
//! (same messages, same spans). Files in languages that aren't compiled in are treated
//! like python treats languages it can't check: the syntax gate passes.

use serde_json::{json, Value};
use tree_sitter::{Language, Node, Parser};

/// extension -> language name, as tree-sitter-language-pack maps it (subset)
pub fn detect_language_from_path(path: &str) -> Option<&'static str> {
    let file_name = std::path::Path::new(path).file_name()?.to_str()?;
    let lower = file_name.to_ascii_lowercase();
    for (suffix, lang) in [(".app.src", "erlang"), (".rs.html", "rshtml"), ("kitty.conf", "kitty"), (".env", "dotenv")] {
        if lower.ends_with(suffix) {
            return Some(lang);
        }
    }
    let ext = std::path::Path::new(path).extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "py" | "pyi" | "pyw" => "python",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "tsx",
        "rs" => "rust",
        "go" => "go",
        "c" | "h" => "c",
        "cpp" | "cxx" | "cc" | "hpp" | "hxx" => "cpp",
        "java" => "java",
        "css" => "css",
        "json" => "json",
        "sh" | "bash" => "bash",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "html" | "htm" => "html",
        "txt" => "vimdoc",
        "md" | "markdown" => "markdown",
        "rb" => "ruby",
        "php" => "php",
        "cs" => "csharp",
        "kt" | "kts" => "kotlin",
        "swift" => "swift",
        "lua" => "lua",
        "scala" => "scala",
        "dart" => "dart",
        "zig" => "zig",
        "sql" => "sql",
        "xml" => "xml",
        "ini" => "ini",
        "scss" => "scss",
        "vue" => "vue",
        "svelte" => "svelte",
        "ex" | "exs" => "elixir",
        "erl" | "hrl" => "erlang",
        "hs" => "haskell",
        "ml" => "ocaml",
        "nix" => "nix",
        "r" => "r",
        "pl" | "pm" => "perl",
        "jl" => "julia",
        "clj" | "cljs" => "clojure",
        "el" => "elisp",
        "vim" => "vim",
        "proto" => "proto",
        "graphql" | "gql" => "graphql",
        "tf" | "hcl" => "hcl",
        "cmake" => "cmake",
        "mk" => "make",
        "dockerfile" => "dockerfile",
        "csv" => "csv",
        "tex" => "latex",
        "rst" => "rst",
        _ => return None,
    })
}

fn language(lang: &str) -> Option<Language> {
    Some(match lang {
        "python" => tree_sitter_python::LANGUAGE.into(),
        "javascript" => tree_sitter_javascript::LANGUAGE.into(),
        "typescript" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        "tsx" => tree_sitter_typescript::LANGUAGE_TSX.into(),
        "rust" => tree_sitter_rust::LANGUAGE.into(),
        "go" => tree_sitter_go::LANGUAGE.into(),
        "c" => tree_sitter_c::LANGUAGE.into(),
        "cpp" => tree_sitter_cpp::LANGUAGE.into(),
        "java" => tree_sitter_java::LANGUAGE.into(),
        "css" => tree_sitter_css::LANGUAGE.into(),
        "json" => tree_sitter_json::LANGUAGE.into(),
        "bash" => tree_sitter_bash::LANGUAGE.into(),
        "toml" => tree_sitter_toml_ng::LANGUAGE.into(),
        "yaml" => tree_sitter_yaml::LANGUAGE.into(),
        _ => return None,
    })
}

/// whether a parser for this language is compiled in
pub fn is_supported(lang: &str) -> bool {
    language(lang).is_some()
}

#[derive(Clone, Debug, PartialEq)]
pub struct Diagnostic {
    pub message: String,
    pub start_line: usize,
    pub start_column: usize,
    pub end_line: usize,
    pub end_column: usize,
}

const MAX_TREE_DEPTH: usize = 512;

/// pre-order walk of every node (tree-sitter-language-pack's walk_bounded)
fn walk<'t>(root: Node<'t>, mut visit: impl FnMut(Node<'t>, usize) -> bool) {
    let mut cursor = root.walk();
    let mut depth = 0usize;
    loop {
        let node = cursor.node();
        let descend = visit(node, depth);
        if descend && depth < MAX_TREE_DEPTH && cursor.goto_first_child() {
            depth += 1;
            continue;
        }
        loop {
            if depth == 0 {
                return;
            }
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return;
            }
            depth -= 1;
        }
    }
}

fn node_text<'a>(node: &Node, source: &'a str) -> &'a str {
    let s = node.start_byte().min(source.len());
    let e = node.end_byte().min(source.len());
    source.get(s..e).unwrap_or("")
}

fn parse(source: &str, lang: &str) -> Option<tree_sitter::Tree> {
    let language = language(lang)?;
    let mut parser = Parser::new();
    parser.set_language(&language).ok()?;
    parser.parse(source, None)
}

/// syntax diagnostics (None if the language isn't compiled in or parsing failed)
pub fn diagnostics(source: &str, lang: &str) -> Option<Vec<Diagnostic>> {
    let tree = parse(source, lang)?;
    let mut out = Vec::new();
    walk(tree.root_node(), |node, _| {
        let message = if node.is_error() {
            Some(format!("Syntax error: unexpected '{}'", node_text(&node, source)))
        } else if node.is_missing() {
            Some(format!("Missing expected node: {}", node.kind()))
        } else {
            None
        };
        if let Some(message) = message {
            let (s, e) = (node.start_position(), node.end_position());
            out.push(Diagnostic { message, start_line: s.row, start_column: s.column, end_line: e.row, end_column: e.column });
        }
        true
    });
    Some(out)
}

fn structure_kind(node: &Node, _lang: &str) -> Option<&'static str> {
    Some(match node.kind() {
        "function_definition" | "function_declaration" | "protocol_function_declaration" | "function_item" | "arrow_function" => "Function",
        "method_definition" | "method_declaration" => "Method",
        "class_definition" | "class_declaration" | "class" => "Class",
        "struct_item" | "struct_definition" | "struct_declaration" => "Struct",
        "interface_declaration" | "interface_definition" | "protocol_declaration" => "Interface",
        "enum_item" | "enum_definition" | "enum_declaration" => "Enum",
        "module_definition" | "mod_item" | "package_header" | "package_declaration" => "Module",
        "internal_module" if matches!(_lang, "typescript" | "tsx") => "Namespace",
        "trait_item" => "Trait",
        "impl_item" => "Impl",
        _ => return None,
    })
}

fn resolve_name(node: &Node, source: &str) -> Option<String> {
    if node.kind() == "arrow_function" {
        let mut value = *node;
        let mut parent = value.parent()?;
        while parent.kind() == "parenthesized_expression" {
            value = parent;
            parent = value.parent()?;
        }
        if parent.kind() != "variable_declarator" || parent.child_by_field_name("value")?.id() != value.id() {
            return None;
        }
        let name = parent.child_by_field_name("name")?;
        return (name.kind() == "identifier").then(|| node_text(&name, source).to_string());
    }
    if let Some(n) = node.child_by_field_name("name") {
        let t = node_text(&n, source);
        if !t.is_empty() {
            return Some(t.to_string());
        }
    }
    for target in ["type_identifier", "identifier", "scoped_identifier"] {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == target {
                let t = node_text(&child, source);
                if !t.is_empty() {
                    return Some(t.to_string());
                }
            }
        }
    }
    None
}

fn is_doc_comment(node: &Node, source: &str) -> bool {
    if !(node.kind().contains("comment")) {
        return false;
    }
    let t = node_text(node, source);
    t.starts_with("///") || t.starts_with("//!") || (t.starts_with("/**") && !t.starts_with("/**/"))
}

fn doc_comment(node: &Node, source: &str) -> Option<String> {
    let mut lines = Vec::new();
    let mut boundary = node.start_position().row;
    let mut current = node.prev_named_sibling();
    while let Some(sib) = current {
        if !is_doc_comment(&sib, source) {
            break;
        }
        let text = node_text(&sib, source);
        let trimmed = text.trim_end();
        let end_row = sib.start_position().row + trimmed.matches('\n').count();
        if boundary.saturating_sub(end_row) > 1 {
            break;
        }
        boundary = sib.start_position().row;
        lines.push(trimmed.to_string());
        current = sib.prev_named_sibling();
    }
    if lines.is_empty() {
        return None;
    }
    lines.reverse();
    Some(lines.join("\n"))
}

fn is_import(node: &Node, lang: &str) -> bool {
    let k = node.kind();
    match lang {
        "python" => k == "import_statement" || k == "import_from_statement",
        "javascript" | "typescript" | "tsx" => k == "import_statement",
        "rust" => k == "use_declaration",
        "go" => k == "import_spec",
        "java" | "kotlin" => k == "import_declaration",
        _ => false,
    }
}

/// `_extract_code_structure` result: {"symbols": [...], "metrics": {...}, "imports": [...]}
/// or None when there's nothing to outline.
pub fn outline(source: &str, lang: &str) -> Option<Value> {
    let tree = parse(source, lang)?;
    let root = tree.root_node();
    let mut symbols = Vec::new();
    let mut imports = Vec::new();
    let mut error_count = 0usize;
    // c is handled by a dedicated adapter in the language pack; we use the generic matcher
    let mut covered_until: Option<usize> = None;
    walk(root, |node, _| {
        if node.is_error() || node.is_missing() {
            error_count += 1;
        }
        if is_import(&node, lang) {
            imports.push(Value::String(node_text(&node, source).to_string()));
        }
        // only top-level structure items are reported (python iterates extraction.structure)
        if covered_until.map(|end| node.start_byte() < end).unwrap_or(false) {
            return true;
        }
        if let Some(kind) = structure_kind(&node, lang) {
            let name = resolve_name(&node, source);
            let body = node.child_by_field_name("body");
            let end = body.map(|b| b.start_byte()).unwrap_or(node.end_byte());
            let sig = source.get(node.start_byte()..end).map(|s| s.trim_end().to_string()).filter(|s| !s.is_empty());
            symbols.push(json!({
                "name": name.unwrap_or_else(|| "None".into()),
                "kind": kind,
                "start_line": node.start_position().row,
                "end_line": node.end_position().row,
                "doc_comment": doc_comment(&node, source),
                "signature": sig,
            }));
            covered_until = Some(node.end_byte());
        }
        true
    });
    if symbols.is_empty() {
        return None;
    }
    let total_lines = source.lines().count();
    Some(json!({
        "symbols": symbols,
        "metrics": {"total_lines": total_lines, "error_count": error_count},
        "imports": imports,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects() {
        assert_eq!(detect_language_from_path("/a/b.py"), Some("python"));
        assert_eq!(detect_language_from_path("/a/b.txt"), Some("vimdoc"));
        assert_eq!(detect_language_from_path("/a/Makefile"), None);
    }

    #[test]
    fn python_diagnostics() {
        assert!(diagnostics("def f():\n    return 1\n", "python").unwrap().is_empty());
        let d = diagnostics("def f(:\n    return 1\n", "python").unwrap();
        assert!(!d.is_empty());
        assert!(d[0].message.starts_with("Syntax error: unexpected") || d[0].message.starts_with("Missing expected node"));
        for lang in ["javascript", "typescript", "tsx", "rust", "go", "c", "cpp", "java", "css", "json", "bash", "toml", "yaml"] {
            assert!(diagnostics("", lang).is_some(), "{}", lang);
        }
        assert!(!diagnostics("{\"a\": }", "json").unwrap().is_empty());
    }

    #[test]
    fn outline_python() {
        let src = "import os\nfrom x import y\n\nclass A:\n    def m(self):\n        pass\n\ndef f(a, b):\n    return a\n";
        let o = outline(src, "python").unwrap();
        assert_eq!(o["symbols"].as_array().unwrap().len(), 2);
        assert_eq!(o["symbols"][0]["name"], "A");
        assert_eq!(o["symbols"][0]["kind"], "Class");
        assert_eq!(o["symbols"][0]["signature"], "class A:");
        assert_eq!(o["symbols"][1]["start_line"], 7);
        assert_eq!(o["imports"], json!(["import os", "from x import y"]));
        assert_eq!(o["metrics"]["total_lines"], 9);
        let o = outline("/// doc\nfn main() {}\n", "rust").unwrap();
        assert_eq!(o["symbols"][0]["doc_comment"], "/// doc");
        assert!(outline("x = 1\n", "python").is_none());
    }
}
