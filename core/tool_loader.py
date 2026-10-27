import core
import inspect
import json_repair
import regex as re
import weakref

META_TOOL_NAMES = {"tools_load", "tools_call"}

_TYPE_MAP = {str: "string", int: "integer", bool: "boolean", list: "array", dict: "object"}
_ARG_SECTION = "Args:"
_END_SECTIONS = ("Returns:", "Raises:", "Note:", "Example:")
_ALL_SECTIONS = (_ARG_SECTION,) + _END_SECTIONS
_PARAM_RE = re.compile(r"(\w+)(?:\s*\([^)]*\))?\s*:\s*(.+)")

# caches tool dicts to speed stuff up
_TOOL_DICT_CACHE = weakref.WeakKeyDictionary()


class ToolLoader:
    """Manages dynamic tool loading: catalog, active set, the tools_load meta tool,
    and the cache-stable tool bridge (tools_call)."""

    def __init__(self, channel):
        self.channel = channel
        self.catalog = {}  # name -> {"tool": dict, "module": str, "method": str, "description": str}
        self.active_tools = []  # tool dicts sent to the API
        self.active_names = []  # active tool names
        self._meta_def = None  # tools_load meta tool dict (part of the baseline)
        self._bridge_defs = []  # tools_call bridge defs (part of the baseline)

    # ------------------------------------------------------------------
    # Internal helpers
    # ------------------------------------------------------------------

    def parse_tool_docstring(self, docstring):
        """Parse a Google-style docstring into (param_descriptions, clean_description)."""
        if not docstring:
            return {}, ""

        lines = docstring.split("\n")

        # clean description: drop section headers and their indented bodies
        clean_lines = []
        skip = False
        for line in lines:
            stripped = line.strip()
            if stripped.startswith(_ALL_SECTIONS):
                skip = True
            elif skip and (not stripped or not line[:1].isspace()):
                skip = False
                if stripped:
                    clean_lines.append(line)
            elif not skip:
                clean_lines.append(line)

        # param descriptions from the Args: section
        descriptions = {}
        in_args = False
        current_param = None
        current_desc = []
        for line in lines:
            stripped = line.strip()
            if stripped.startswith(_ARG_SECTION):
                in_args = True
            elif in_args and stripped in _END_SECTIONS:
                break
            elif in_args and stripped:
                match = _PARAM_RE.match(stripped)
                if match:
                    if current_param and current_desc:
                        descriptions[current_param] = " ".join(current_desc)
                    current_param = match.group(1)
                    current_desc = [match.group(2)]
                elif current_param:
                    current_desc.append(stripped)
        if current_param and current_desc:
            descriptions[current_param] = " ".join(current_desc)

        return descriptions, "\n".join(clean_lines).strip()

    def _tool_dict_from_func(self, func, tool_name):
        """Build (or reuse) an API tool dict from a callable."""
        # bound methods are recreated on every getattr, so the cache keys
        # on the underlying class-level function instead
        key = getattr(func, "__func__", func)
        by_name = _TOOL_DICT_CACHE.get(key)
        if by_name is None:
            by_name = {}
            try:
                _TOOL_DICT_CACHE[key] = by_name
            except TypeError:
                pass  # exotic non-weakref-able callable: build without caching
        else:
            cached = by_name.get(tool_name)
            if cached is not None:
                return cached

        param_descriptions, docstring = self.parse_tool_docstring(func.__doc__)

        properties = {}
        required = []
        for name, param in inspect.signature(func).parameters.items():
            prop = {"type": _TYPE_MAP.get(param.annotation, "string")}
            desc = param_descriptions.get(name)
            if desc:
                prop["description"] = desc
            properties[name] = prop
            if param.default == inspect.Parameter.empty:
                required.append(name)

        tool = {
            "type": "function",
            "function": {
                "name": tool_name,
                "parameters": {
                    "type": "object",
                    "properties": properties,
                    "required": required,
                    "additionalProperties": False,
                },
                "strict": False,
            },
        }
        if docstring:
            tool["function"]["description"] = docstring
        by_name[tool_name] = tool
        return tool

    def _loadable_entry(self, name):
        """Return the catalog entry for a tool if loadable, else None.

        Loadable means: in the catalog, its module is enabled, and the tool
        itself is not disabled.
        """
        entry = self.catalog.get(name)
        if entry is None:
            return None
        module = self.channel.manager.modules.get(entry["module"])
        if module is None or entry["method"] in module.disabled_tools:
            return None
        return entry

    def _preloaded_modules(self):
        """Module names whose tools are preloaded at startup (model.preloaded_modules)."""
        modules = core.config.get("model", "preloaded_modules", default=[])
        return {str(m).lower() for m in modules} if isinstance(modules, list) else set()

    def _default_tools(self):
        """Cataloged tools belonging to the preloaded modules, as {name: entry}."""
        preloaded = self._preloaded_modules()
        return {
            name: entry for name, entry in self.catalog.items()
            if entry["module"] in preloaded
        }

    # ------------------------------------------------------------------
    # Catalog management
    # ------------------------------------------------------------------

    def register_module(self, module):
        """Catalog all @core.module.tool decorated methods of a module."""
        registered = []
        for func_name in type(module).__dict__:
            func_obj = getattr(module, func_name, None)
            
            if not callable(func_obj) or not getattr(func_obj, "_is_tool", False):
                continue
            if getattr(func_obj, "_is_command", False):
                continue  # a command is never a tool, even if double-decorated
            if func_name in module.disabled_tools:
                # don't register disabled tools
                continue

            tool_name = f"{module.name}_{func_name}"
            tool_dict = self._tool_dict_from_func(func_obj, tool_name)
            self.catalog[tool_name] = {
                "tool": tool_dict,
                "module": module.name,
                "method": func_name,
                "description": tool_dict["function"].get("description", ""),
            }
            registered.append(func_name)
        self._warn_missed_tools(module, registered)

    def _warn_missed_tools(self, module, registered):
        """
        a module that registers zero tools while still
        holding public callables most likely predates @core.module.tool and
        was never migrated
        """
        if registered:
            return
        
        candidates = []
        for name in type(module).__dict__:
            if name.startswith(("_", "on_")) or name == "result":
                continue
            if name in module.disabled_tools:
                continue
                
            func_obj = getattr(module, name, None)
            if callable(func_obj) and not getattr(func_obj, "_is_command", False):
                candidates.append(name)
                
        if candidates:
            shown = ", ".join(sorted(candidates)[:8])
            if len(candidates) > 8:
                shown += ", ..."
            self.channel.log(
                "warning",
                f"{module.name}: registered 0 tools but has public callables "
                f"({shown}). Please add @core.module.tool decorators to your tools."
            )

    def unregister_module(self, module):
        """Remove all catalog entries for a module (the caller refreshes)."""
        prefix = f"{module.name}_"
        for k in [k for k in self.catalog if k.startswith(prefix)]:
            del self.catalog[k]

    # ------------------------------------------------------------------
    # Meta tool
    # ------------------------------------------------------------------

    @property
    def meta_tool_names(self):
        return META_TOOL_NAMES

    def _enabled_module_names(self):
        """Names of all currently enabled (loaded) modules that have tools"""
        modules_with_tools = {entry["module"] for entry in self.catalog.values()}
        return sorted(
            name for name in self.channel.manager.modules
            if name in modules_with_tools
        )

    def _build_meta_def(self):
        """Build the tools_load meta tool definition."""
        modules = ", ".join(self._enabled_module_names()) or "(none currently enabled)"
        return {
            "type": "function",
            "function": {
                "name": "tools_load",
                "description": (
                    "Returns full schemas of all tools in the modules you request"
                    ", as array. Tools are NOT added to your tool list: call "
                    "them with tools_call using returned schemas."
                    f"Currently enabled modules: {modules}"
                ),
                "parameters": {
                    "type": "object",
                    "properties": {
                        "module_names": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "Names of the modules whose tool schemas to return",
                        }
                    },
                    "required": ["module_names"],
                    "additionalProperties": False,
                },
                "strict": False,
            },
        }

    # ------------------------------------------------------------------
    # Tool bridge (cache-stable dynamic invocation)
    # ------------------------------------------------------------------

    def _build_bridge_defs(self):
        """Build the tools_call bridge tool definition."""
        call = {
            "type": "function",
            "function": {
                "name": "tools_call",
                "description": (
                    "Calls tool directly by name. If do not know tool's "
                    "params, call tools_load with module name first to get "
                    "schemas. When writing the call, ALWAYS emit the 'name' key "
                    "FIRST, then 'params'. Arguments are validated against tool's schema before it "
                    "runs; invalid calls are rejected with valid parameter "
                    "list."
                ),
                "parameters": {
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "description": "Exact tool name to call. Always output this key first"},
                        "params": {
                            "type": "string",
                            "description": (
                                "JSON-encoded arguments object for the tool, matching "
                                "its schema. Output AFTER name. Example: "
                                "{\"path\": \"foo.py\", \"content\": \"...\"}"
                            ),
                        },
                    },
                    "required": ["name", "params"],
                    "additionalProperties": False,
                },
                "strict": False,
            },
        }
        return [call]



    def register_meta_tools(self):
        """Build the meta tool + bridge defs when dynamic loading is enabled."""
        if not core.config.get("model", "dynamic_tool_loading", default=False):  # AI Written: Claude Opus 5.5: match the schema default
            return
        if self._meta_def:
            return
        self._meta_def = self._build_meta_def()
        self._bridge_defs = self._build_bridge_defs()
        self.refresh()

    def refresh(self):
        """Rebuild meta tool/bridge descriptions and the active tools array
        from the current catalog state.

        Called on module-lifecycle events only (startup, module
        register/unregister) - the active array NEVER changes mid-chat:
        dynamic loading delivers schemas via tools_load results and runs
        everything through the tools_call bridge, so the request prefix and
        the provider prompt cache stay intact for the whole conversation.
        """
        if self._meta_def:
            # dynamic loading on: meta tools + preloaded modules' tools
            self._meta_def = self._build_meta_def()
            self._bridge_defs = self._build_bridge_defs()
            defs = [self._meta_def] + self._bridge_defs
            source = self._default_tools()
        else:
            # dynamic loading off: every loadable tool registers natively
            defs = []
            source = self.catalog
        for name, entry in source.items():
            if self._loadable_entry(name) is not None:
                defs.append(entry["tool"])
        self.active_tools = defs
        self.active_names = [t["function"]["name"] for t in defs]

    def get_meta_callable(self, tool_name):
        """Return the bound method for a meta tool name."""
        if tool_name == "tools_load":
            return self.tools_load
        if tool_name == "tools_call":
            return self.tools_call
        return None

    # ------------------------------------------------------------------
    # Meta tool implementation
    # ------------------------------------------------------------------

    async def tools_load(self, module_names: list):
        """Returns the full parameter schemas of all tools in the given modules.

        The tools are not added to your tool list; call them with tools_call
        using the returned schemas.

        Args:
        module_names (list): Names of the modules whose tool schemas to return
        """
        if isinstance(module_names, str):
            module_names = [module_names]
        if not isinstance(module_names, list):
            return {"status": "error", "content": "module_names must be a list of module names"}

        tools = []
        unknown = []
        disabled = []
        for module_name in module_names:
            name = str(module_name).lower().strip()
            module = self.channel.manager.modules.get(name)
            if module is None:
                unknown.append(name)
                continue
            for tool_name, entry in sorted(self.catalog.items()):
                if entry["module"] != name:
                    continue
                if entry["method"] in module.disabled_tools:
                    disabled.append(tool_name)
                    continue
                func = entry["tool"]["function"]
                tools.append({
                    "name": tool_name,
                    "description": func.get("description", ""),
                    "parameters": func.get("parameters", {}),
                })

        if not tools:
            if unknown:
                return {
                    "status": "error",
                    "content": (
                        f"No enabled module named '{unknown[0]}'"
                        + (f" (also unknown: {', '.join(unknown[1:])})" if len(unknown) > 1 else "")
                        + f". Enabled modules: {', '.join(self._enabled_module_names())}."
                    ),
                }
            return {
                "status": "error",
                "content": "None of those modules have loadable tools.",
            }

        result = {
            "status": "success",
            "tools": tools,
            "note": "These tools are not in your tool list; call them with tools_call(name, params) - name first.",
        }
        if unknown:
            result["not_found"] = unknown
            result["enabled_modules"] = self._enabled_module_names()
        if disabled:
            result["disabled"] = disabled
        return result

    _NAME_TO_TYPE = {"str": str, "int": int, "bool": bool, "list": list, "dict": dict}

    def _simple_annotation(self, annotation):
        """Return the raw type for simple annotations (incl. stringized ones), else None."""
        if isinstance(annotation, str):
            return self._NAME_TO_TYPE.get(annotation)
        try:
            if annotation in _TYPE_MAP:
                return annotation
        except TypeError:  # unhashable annotation
            return None
        return None

    @staticmethod
    def _coerce_arg(value, target):
        """Leniently coerce a JSON value to a simply-annotated parameter type. Returns (value, ok)."""
        if target is int and isinstance(value, bool):
            return value, False
        if isinstance(value, target):
            return value, True
        if target is int:
            if isinstance(value, str) and re.fullmatch(r"[+-]?\d+", value.strip()):
                return int(value.strip()), True
            if isinstance(value, float) and value.is_integer():
                return int(value), True
        elif target is bool:
            if isinstance(value, int):
                return bool(value), True
            if isinstance(value, str) and value.strip().lower() in ("true", "false"):
                return value.strip().lower() == "true", True
        elif target is str:
            if isinstance(value, (int, float)):
                return str(value), True
        return value, False

    def _param_summary(self, sig):
        """Render 'name: type (required)' for each parameter of a signature."""
        parts = []
        for name, param in sig.parameters.items():
            target = self._simple_annotation(param.annotation)
            label = _TYPE_MAP[target] if target else "any"
            marker = "" if param.default is not inspect.Parameter.empty else " (required)"
            parts.append(f"{name}: {label}{marker}")
        return ", ".join(parts) if parts else "(none)"

    def _preflight_args(self, func, tool_name, arguments):
        """Check arguments against the target function's signature before dispatch.

        Returns (clean_arguments, None) if the call can proceed (possibly with
        coerced values), or (None, error_message) if it would raise before
        reaching the tool body.
        """
        try:
            sig = inspect.signature(func)
        except (TypeError, ValueError):
            return arguments, None  # exotic callable; don't block on introspection
        try:
            sig.bind(**arguments)
        except TypeError as exc:
            return None, (
                f"Invalid arguments for '{tool_name}': {exc}. "
                f"Valid parameters: {self._param_summary(sig)}. "
                "Call tools_load with this tool's module name to get the full schema."
            )

        coerced = {}
        mismatches = []
        for key, value in arguments.items():
            param = sig.parameters.get(key)
            target = self._simple_annotation(param.annotation) if param is not None else None
            if target is None or value is None:
                # unknown extra keys already rejected by sig.bind; anything
                # consumed by *args/**kwargs, unannotated, or an explicit None
                # (Optional-style params) passes through untouched
                coerced[key] = value
                continue
            new_value, ok = self._coerce_arg(value, target)
            if ok:
                coerced[key] = new_value
            else:
                mismatches.append(
                    f"'{key}' expects {_TYPE_MAP[target]}, got {type(value).__name__}: {value!r}"
                )
        if mismatches:
            return None, (
                f"Invalid arguments for '{tool_name}': " + "; ".join(mismatches)
                + f". Expected: {self._param_summary(sig)}."
            )
        return coerced, None

    async def tools_call(self, name: str, params=None, arguments=None):
        """Calls a cataloged tool directly by name without loading it into your toolset.

        Args:
        name (str): Exact name of the tool to call
        params (str): JSON-encoded arguments object for the tool, matching its schema (a raw object is accepted too)
        """
        if params is None:
            params = arguments
            
        if isinstance(params, str):
            # transport format: JSON-encoded string (streams token by token)
            try:
                params = json_repair.loads(params)
            except Exception:
                return {"status": "error", "content": "params is not valid JSON"}
            
        if params is None:
            params = {}
            
        if not isinstance(params, dict):
            return {"status": "error", "content": "params must be an object"}

        name = str(name).strip()
        if name in META_TOOL_NAMES:
            return {"status": "error", "content": f"'{name}' is a meta tool; call it directly."}

        entry = self._loadable_entry(name)
        if entry is None:
            if name in self.catalog:
                return {"status": "error", "content": f"Tool '{name}' is disabled (module or tool setting)."}
            return {
                "status": "error",
                "content": (
                    f"No tool named '{name}'. Call tools_load with a module name to get its "
                    f"tools' schemas. Enabled modules: {', '.join(self._enabled_module_names())}."
                ),
            }

        module = self.channel.manager.modules.get(entry["module"])
        if module is None:
            return {"status": "error", "content": f"Module '{entry['module']}' is not enabled."}

        func = getattr(module, entry["method"])
        params, preflight_error = self._preflight_args(func, name, params)
        if preflight_error:
            self.channel.log("toolcall", f"bridge rejected '{name}': {preflight_error}")
            return {"status": "error", "content": preflight_error}

        result = func(**params)
        if inspect.isawaitable(result):
            result = await result
        return result
