# OpenLumara → Rust port

This directory contains a Rust implementation of the OpenLumara backend. The Python
source in the repository root is kept intact as the reference. The Rust binary is
meant as a drop-in replacement for `python main.py`:

- same config file (`config.yml` at the project root, same keys, same sync/prune rules)
- same data layout (`data/chats/<channel>/index.mp`, `history/<id>.json`, `current`,
  `data/save.mp`, module storages, ...), readable/writable by both implementations
- same HTTP / WebSocket API used by the web UI, with the web UI's JS/HTML/CSS served
  unchanged from `channels/webui/` (templates rendered by minijinja instead of Jinja2)
- same command line flags (plus `--root`)

Build and run (from the repository root):

```sh
cd rust && cargo build --release
./target/release/openlumara            # project root = current dir if it is an openlumara checkout
./target/release/openlumara --root /path/to/openlumara --cli
```

The project root (where `config.yml`, `data/`, `channels/webui/` and `modules/*/webui/`
live) is resolved from `--root`, then `OPENLUMARA_ROOT`, then the current directory,
then the executable's ancestors.

## Architecture map

| Python | Rust |
|---|---|
| `main.py` | `crates/openlumara/src/{main,args}.rs` |
| `core/__init__.py`, `core/functions.py`, `core/exceptions.py` | `ol-core/src/{globals,paths,errors}.rs` |
| `core/config.py` | `ol-core/src/config.rs` |
| `core/storage.py` | `ol-core/src/storage.rs` |
| `core/search.py` | `ol-core/src/search.rs` |
| `core/api.py` | `ol-core/src/api.rs` |
| `core/chat.py`, `core/messages.py` | `ol-core/src/chat.rs` |
| `core/context.py` | `ol-core/src/context.rs` |
| `core/turns.py` | `ol-core/src/turns.rs` |
| `core/toolcalls.py` | `ol-core/src/toolcalls.rs` |
| `core/tool_loader.py` | `ol-core/src/tool_loader.rs` |
| `core/commands.py` | `ol-core/src/commands.rs` |
| `core/module.py` | `ol-core/src/module.rs` |
| `core/modules.py` (dynamic loader) | `ol-core/src/registry.rs` + `config::available_names` |
| `core/channel.py` | `ol-core/src/channel.rs` |
| `core/manager.py` | `ol-core/src/manager.rs` |
| `core/module_manager.py` | `ol-core/src/module_manager.rs` |
| `json_repair`, `partial_json_parser` | `ol-core/src/lenient_json.rs` |
| python `json.dumps`/`repr` formatting | `ol-core/src/pyjson.rs` |
| `channels/webui/*` | `crates/ol-webui` |
| `channels/{cli,cli_lite,logger,ntfy,api_bridge,simple_webui}` | `crates/ol-channels` |
| `channels/{telegram,discord_bot,matrix}` | `crates/ol-channels-chat` |
| `modules/{tutorial,docs,identity,models,channel,modules,chats,notes,lists,time,token_threshold,calculator,replacer,webui_extension_example,skills,writing_style}` | `crates/ol-modules` |
| `modules/{memory,scheduler,calendar,auto_backup,characters}` | `crates/ol-modules-data` |
| `modules/{http,web_reader,web_search,file_manager,unsafe_shell}` | `crates/ol-modules-web` |
| `modules/{shell,coder,mcp}` | `crates/ol-modules-dev` |

### Crate choices

| Need | Crate |
|---|---|
| async runtime | `tokio` |
| async generators (python `async for`/`yield`) | `async-stream`, `futures` |
| HTTP client (LLM API, web modules) | `reqwest` (rustls) |
| HTTP/WebSocket server (webui) | `axum` |
| Jinja2 templates | `minijinja` (+ `minijinja-contrib` pycompat) |
| JSON / YAML / msgpack | `serde_json` (`preserve_order`), `serde_yaml`, `rmp-serde` |
| ordered maps | `indexmap` |
| ULIDs, time | `ulid`, `chrono`, `chrono-tz` |
| file type sniffing (`filetype`) | `infer` |
| PDF text (`PyPDF2`) | `pdf-extract` |
| shell-style splitting (`shlex`) | `shlex` |

### Dynamic loading → compiled-in registry

Python discovers modules/channels by scanning folders and importing `module.py` /
`channel.py`. Rust can't import arbitrary source at runtime, so every module and
channel is compiled in and registered in `ol_core::registry` by the binary at startup
(each crate exposes `register(&mut Registry)`). Discovery still scans the folders on
disk so that config entries for items that exist on disk but are not compiled in
(unported modules, python user modules) are preserved and listed in the settings UI;
they are skipped at load time with a warning.

The python dependency auto-installer (`pip install` of module `dependencies`) has no
equivalent: everything is compiled in. `--disable_auto_installer` is accepted and ignored.

### Module / channel conventions

- A module implements `ol_core::module::Module`. Its state lives in the struct with
  interior mutability (`parking_lot::Mutex`/`tokio::sync::Mutex`); hooks are `&self`.
- Class-level metadata (docstring, `settings` schema, `unsafe`, `category`, tool
  methods + docstrings + parameter types, `@command`s, `@webui.route`s) is extracted
  from the python source by `rust/tools/gen_module_meta.py` into
  `rust/generated/{modules,channels}/<name>.rs`, so tool schemas sent to the model are
  byte-identical to python's. Re-run the generator if the python modules change.
- Tool methods are dispatched through `Module::call_tool(method, ToolArgs)`; the core
  validates arguments against the tool spec first and produces python-style
  `TypeError` messages for missing/unexpected arguments.
- `@core.module.command` → `Module::commands()` + `Module::run_command()`.
- `@webui.route` → `Module::ui_routes()` + `Module::ui_route()`; served by the webui at
  `/api/ext/<module>/<path>`.
- A channel implements `ol_core::channel::ChannelBehavior`; shared logic (send,
  send_stream, push, push_stream, format_stream_for_text, ...) lives in
  `ol_core::channel::Channel`.
- Python async generators become `TokenStream = Pin<Box<dyn Stream<Item = Value>>>`
  yielding the same token dicts. Python's `asyncio.CancelledError` propagation for
  `/stop` is modelled by a `{"type": "__cancelled__"}` sentinel token.

## Frontend API contract (served by `ol-webui`)

All JSON endpoints answer `{"data": ..., "success": bool}` unless noted.

- Pages: `GET /`, `GET|POST /login`, `GET /logout`, `GET /sw.js`, `/manifest.json`,
  `/icon-192.png`, `/icon-512.png`, `/favicon.ico`, static `/assets/*`,
  `/ext-assets/<module>/<path>`, `/themes/<name>.css`
- Chats: `GET /api/chat/load/{id}`, `GET /api/chat/current`, `GET /api/chat/export`,
  `GET /api/chat/prompt`, `POST /api/chat/new`, `POST /api/chat/rename/{id}`,
  `POST /api/chat/delete/{id}`, `POST /api/chat/set-category/{id}`,
  `GET /api/chats`, `GET /api/chats/days`, `GET /api/chats/day`,
  `GET /api/chats/categories`, `GET /api/chats/categories/count`, `GET /api/chats/tags`,
  `POST /api/chats/categories/create`, `POST /api/chats/categories/delete`,
  `POST /api/chats/search`
- Stream state: `GET /api/stream/state`; input history: `GET|POST /api/input_history`
- Settings/API: `GET /api/settings/load`, `POST /api/settings/save`,
  `GET /api/check_connection`, `GET /api/models`, `POST /api/model/set`, `POST /api/reconnect`
- Libraries: `GET /api/libraries/status`, `POST /api/libraries/{sync,install,update,uninstall}`
- System: `GET /api/system/data`, `GET /api/system/context_size`, `GET /api/system/logs`,
  `POST /api/system/restart`
- Themes: `GET /api/themes` (bare list), `GET /api/themes/{family}` (bare theme json)
- Module extensions: `GET|POST|PUT|DELETE /api/ext/{module}/{path}`
- WebSocket `/ws`: client → `stop`, `reload_messages`, `rename`, `switch_chat`, `new_chat`,
  `chat_delete`, `user_message` (`content`, `files[{name,data(b64)}]`), `message_edit`,
  `message_delete`, `message_regenerate`; server → `ready`, `stream_state`, `push`, `log`,
  `token`, `turn_stream`, `user_message_added`, `user_message_confirmed`,
  `stream_complete`, `sync`, `chat_switched`, `chat_metadata_updated`, `error`, `shutdown`
- Admin lockdown (`enable_admin_settings: false`) → 403 on the admin endpoint list;
  `require_login` → cookie session, 401 for `/api/*`, redirect to `/login` otherwise.

## Status

Legend: **ported** = feature-complete port; **partial** = works with documented gaps;
**not ported** = skipped (reason given).

### Core

| Python file | Status | Notes |
|---|---|---|
| main.py | ported | all flags; per-setting `--section.key value` overrides; restart loop. `--help` text simplified |
| core/__init__.py | ported | firstrun marker, user folders |
| core/functions.py | ported | `sandbox_path` semantics replicated (incl. quirks), unit tested |
| core/exceptions.py | ported | |
| core/config.py | ported | schema/sync/prune/reconcile identical; no `.module_cache.json` (schemas are compiled in); unported/python-only folders discovered statically (docstring/unsafe/category read from the .py file) |
| core/storage.py | ported | json/yaml/msgpack/markdown/json_folder/text; python-identical json output |
| core/search.py | ported | BM25 + prefix match + phrase boost + snippets |
| core/api.py | ported | reqwest instead of the openai SDK; same request body, token dicts, error strings; llama.cpp router load-progress feed; prompt warmup was already disabled in python and is not ported |
| core/chat.py | ported | incl. old-format migration |
| core/messages.py | ported | |
| core/context.py | ported | trimming, compression, size breakdown, debug prompt-cache diagnostics |
| core/turns.py | ported | |
| core/toolcalls.py | ported | live-tested against vLLM (Qwen3.8-27B) |
| core/tool_loader.py | ported | docstring parser identical; dynamic loading + per-chat persistence |
| core/commands.py | ported | `/library install` doesn't pip-install dependencies |
| core/module.py | ported | trait-based (see conventions) |
| core/modules.py | partial | dynamic import + pip auto-installer replaced by the compiled-in registry |
| core/channel.py | ported | |
| core/manager.py | ported | |
| core/module_manager.py | ported | git clone/update via `git`; installed entries are python code the Rust binary can't run |

### Channels and modules

(updated as the port progresses)

| Python file | Status | Notes |
|---|---|---|
| channels/webui | in progress | |
| channels/cli | in progress | |
| channels/cli_lite | in progress | |
| channels/logger | in progress | |
| channels/ntfy | in progress | |
| channels/api_bridge | in progress | |
| channels/simple_webui | in progress | |
| channels/telegram | in progress | |
| channels/discord_bot | in progress | |
| channels/matrix | in progress | |
| modules/* | in progress | |
