#!/usr/bin/env python3
"""
Dev tool for the Rust port: statically extracts class-level metadata from the python
modules/channels (docstring, settings schema, unsafe, category, tool methods with their
docstrings + parameter types, @command decorators, @webui.route handlers) and writes
them as Rust source into rust/crates/<crate>/src/generated/<name>.rs.

Generated, never imported at runtime: the Rust binary has no python dependency.

Docstrings are reproduced with CPython 3.13+ compile-time semantics (common leading
whitespace stripped from every line after the first), which is what `func.__doc__`
returns on current python and therefore what the python tool loader parses.

usage: python3 rust/tools/gen_module_meta.py
"""
import ast
import json
import os
import sys

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))

TYPE_MAP = {"str": "String", "int": "Integer", "bool": "Boolean", "list": "Array", "dict": "Object"}


def compile_cleandoc(doc):
    """CPython 3.13+ _PyCompile_CleanDoc"""
    lines = doc.expandtabs().split("\n")
    margin = None
    for line in lines[1:]:
        stripped = line.lstrip(" ")
        if stripped:
            indent = len(line) - len(stripped)
            margin = indent if margin is None else min(margin, indent)
    margin = margin or 0
    out = [lines[0].lstrip(" ")]
    for line in lines[1:]:
        out.append(line[margin:] if line.strip() else "")
    return "\n".join(out)


def getdoc_clean(doc):
    """inspect.getdoc / inspect.cleandoc"""
    import inspect
    return inspect.cleandoc(doc) if doc else ""


def rust_str(s):
    # raw string with enough hashes
    hashes = "#"
    while f'"{hashes}' in s:
        hashes += "#"
    return f'r{hashes}"{s}"{hashes}'


def dotted(node):
    parts = []
    while isinstance(node, ast.Attribute):
        parts.append(node.attr)
        node = node.value
    if isinstance(node, ast.Name):
        parts.append(node.id)
    return ".".join(reversed(parts))


def literal(node):
    try:
        return ast.literal_eval(node)
    except Exception:
        return None


def find_class(tree, base_hint):
    for node in tree.body:
        if isinstance(node, ast.ClassDef):
            for b in node.bases:
                d = dotted(b)
                if base_hint in d or d.split(".")[-1] in ("Http",):
                    return node
    for node in tree.body:
        if isinstance(node, ast.ClassDef):
            return node
    return None


def param_type(annotation):
    if annotation is None:
        return "String"
    name = dotted(annotation) if isinstance(annotation, (ast.Name, ast.Attribute)) else None
    return TYPE_MAP.get(name, "String")


def extract(path, base_hint):
    with open(path, encoding="utf-8") as f:
        src = f.read()
    tree = ast.parse(src)
    cls = find_class(tree, base_hint)
    if cls is None:
        return None

    info = {
        "class_name": cls.name,
        "bases": [dotted(b) for b in cls.bases],
        "doc": getdoc_clean(ast.get_docstring(cls, clean=False) or ""),
        "settings": None,
        "settings_error": None,
        "unsafe": False,
        "category": "",
        "header": None,
        "tools": [],
        "commands": [],
        "routes": [],
        "dependencies": [],
    }

    for item in cls.body:
        if isinstance(item, ast.Assign) and len(item.targets) == 1 and isinstance(item.targets[0], ast.Name):
            name = item.targets[0].id
            if name == "settings":
                val = literal(item.value)
                if val is None and not (isinstance(item.value, ast.Constant) and item.value.value is None):
                    info["settings_error"] = "settings is not a pure literal; port by hand"
                else:
                    info["settings"] = val
            elif name == "unsafe":
                info["unsafe"] = bool(literal(item.value))
            elif name == "category":
                info["category"] = literal(item.value) or ""
            elif name == "header":
                info["header"] = literal(item.value)
            elif name == "dependencies":
                info["dependencies"] = literal(item.value) or []

        if isinstance(item, (ast.FunctionDef, ast.AsyncFunctionDef)):
            is_command = False
            route = None
            for dec in item.decorator_list:
                if isinstance(dec, ast.Call):
                    dname = dotted(dec.func)
                    if dname.endswith("module.command"):
                        is_command = True
                        cname = literal(dec.args[0]) if dec.args else None
                        help_val = None
                        send_to_ai = True
                        for kw in dec.keywords:
                            if kw.arg == "help":
                                help_val = literal(kw.value)
                            elif kw.arg == "send_to_ai":
                                send_to_ai = bool(literal(kw.value))
                        if len(dec.args) > 1:
                            help_val = literal(dec.args[1])
                        if help_val is None:
                            d = ast.get_docstring(item, clean=False)
                            if d:
                                help_val = d.strip().split("\n")[0]
                        info["commands"].append({
                            "name": (cname or "").lower().strip(),
                            "method": item.name,
                            "help": help_val if help_val is not None else "",
                            "temporary": not send_to_ai,
                        })
                    elif dname.endswith("webui.route") or dname.endswith(".route"):
                        rpath = literal(dec.args[0]) if dec.args else ""
                        method = "GET"
                        for kw in dec.keywords:
                            if kw.arg == "method":
                                method = literal(kw.value)
                        if len(dec.args) > 1:
                            method = literal(dec.args[1])
                        route = {"path": str(rpath).strip("/").lower(), "method": str(method).upper(), "handler": item.name}
            if route:
                info["routes"].append(route)

            fname = item.name
            if fname.startswith("_") or fname == "result" or fname.startswith("on_") or is_command:
                continue

            params = []
            args = item.args
            positional = args.posonlyargs + args.args
            defaults_start = len(positional) - len(args.defaults)
            for i, a in enumerate(positional):
                if i == 0 and a.arg == "self":
                    continue
                params.append({"name": a.arg, "type": param_type(a.annotation), "required": i < defaults_start})
            if args.vararg:
                params.append({"name": args.vararg.arg, "type": param_type(args.vararg.annotation), "required": True})
            for a, d in zip(args.kwonlyargs, args.kw_defaults):
                params.append({"name": a.arg, "type": param_type(a.annotation), "required": d is None})
            if args.kwarg:
                params.append({"name": args.kwarg.arg, "type": param_type(args.kwarg.annotation), "required": True})

            raw = ast.get_docstring(item, clean=False)
            info["tools"].append({
                "method": fname,
                "doc": compile_cleandoc(raw) if raw else "",
                "params": params,
                "is_route": route is not None,
            })
    return info


def render(info, rel_src):
    out = []
    out.append(f"// @generated by rust/tools/gen_module_meta.py from {rel_src} -- do not edit by hand")
    out.append("#![allow(dead_code, clippy::all)]")
    out.append("use ol_core::module::{CommandHelp, CommandSpec, ParamType, ToolParam, ToolSpec, UiRoute};")
    out.append("use serde_json::Value;")
    out.append("")
    out.append(f"pub const CLASS_NAME: &str = {rust_str(info['class_name'])};")
    out.append(f"pub const DOC: &str = {rust_str(info['doc'])};")
    out.append(f"pub const UNSAFE: bool = {'true' if info['unsafe'] else 'false'};")
    out.append(f"pub const CATEGORY: &str = {rust_str(info['category'] or '')};")
    header = info["header"]
    out.append(f"pub const HEADER: Option<&str> = {('Some(' + rust_str(header) + ')') if header else 'None'};")
    deps = ", ".join(rust_str(d) for d in info["dependencies"])
    out.append(f"/// python pip dependencies of the original module (informational)")
    out.append(f"pub const PY_DEPENDENCIES: &[&str] = &[{deps}];")
    out.append("")
    if info["settings_error"]:
        out.append(f"// NOTE: {info['settings_error']}")
        out.append("pub const SETTINGS_JSON: &str = \"{}\";")
    else:
        settings = info["settings"] if info["settings"] is not None else {}
        out.append(f"pub const SETTINGS_JSON: &str = {rust_str(json.dumps(settings, indent=2, ensure_ascii=False))};")
    out.append("/// the class-level `settings` schema dict")
    out.append("pub fn settings() -> Value {")
    out.append("    serde_json::from_str(SETTINGS_JSON).expect(\"generated settings json\")")
    out.append("}")
    out.append("")
    out.append("/// tool methods in class-definition order (python tool loader order)")
    out.append("pub fn tools() -> Vec<ToolSpec> {")
    out.append("    vec![")
    for t in info["tools"]:
        params = ", ".join(
            f"ToolParam {{ name: {rust_str(p['name'])}, ty: ParamType::{p['type']}, required: {'true' if p['required'] else 'false'} }}"
            for p in t["params"]
        )
        out.append(f"        ToolSpec::new({rust_str(t['method'])}, {rust_str(t['doc'])}, vec![{params}]),")
    out.append("    ]")
    out.append("}")
    out.append("")
    out.append("/// @core.module.command declarations: (command spec, python method name)")
    out.append("pub fn commands() -> Vec<(CommandSpec, &'static str)> {")
    out.append("    vec![")
    for c in info["commands"]:
        h = c["help"]
        if isinstance(h, dict):
            items = ", ".join(f"({rust_str(str(k))}.to_string(), {rust_str(str(v))}.to_string())" for k, v in h.items())
            help_expr = f"CommandHelp::Sub(vec![{items}])"
        else:
            help_expr = f"CommandHelp::Text({rust_str(str(h))}.to_string())"
        out.append(
            f"        (CommandSpec {{ name: {rust_str(c['name'])}.to_string(), help: {help_expr}, temporary: {'true' if c['temporary'] else 'false'} }}, {rust_str(c['method'])}),"
        )
    out.append("    ]")
    out.append("}")
    out.append("")
    out.append("/// @webui.route handlers: (route, python handler method name)")
    out.append("pub fn ui_routes() -> Vec<(UiRoute, &'static str)> {")
    out.append("    vec![")
    for r in info["routes"]:
        out.append(f"        (UiRoute {{ path: {rust_str(r['path'])}.to_string(), method: {rust_str(r['method'])}.to_string() }}, {rust_str(r['handler'])}),")
    out.append("    ]")
    out.append("}")
    return "\n".join(out) + "\n"


def main():
    jobs = [
        ("modules", "module.py", "Module", "ol-modules"),
        ("channels", "channel.py", "Channel", "ol-channels"),
    ]
    for folder, entry, base_hint, crate in jobs:
        out_dir = os.path.join(ROOT, "rust", "crates", crate, "src", "generated")
        os.makedirs(out_dir, exist_ok=True)
        names = []
        for name in sorted(os.listdir(os.path.join(ROOT, folder))):
            path = os.path.join(ROOT, folder, name, entry)
            if not os.path.isfile(path):
                continue
            info = extract(path, base_hint)
            if info is None:
                print(f"skip {folder}/{name}: no class", file=sys.stderr)
                continue
            rel = os.path.relpath(path, ROOT)
            with open(os.path.join(out_dir, f"{name}.rs"), "w", encoding="utf-8") as f:
                f.write(render(info, rel))
            names.append(name)
            if info["settings_error"]:
                print(f"WARNING {folder}/{name}: {info['settings_error']}", file=sys.stderr)
        with open(os.path.join(out_dir, "mod.rs"), "w", encoding="utf-8") as f:
            f.write("// @generated by rust/tools/gen_module_meta.py -- do not edit by hand\n")
            for n in names:
                f.write(f"pub mod {n};\n")
        print(f"{crate}: generated {len(names)} files")


if __name__ == "__main__":
    main()
