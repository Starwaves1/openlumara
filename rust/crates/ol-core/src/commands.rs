//! Port of core/commands.py: built-in slash commands and module command dispatch.

use crate::channel::Channel;
use crate::config;
use crate::errors::UnauthorizedException;
use crate::module::CommandHelp;
use crate::pyjson::{self, py_str, truthy};
use indexmap::IndexMap;
use serde_json::{json, Value};
use std::sync::Arc;

/// delete these after they are shown to the user once
pub const GHOST: [&str; 8] = ["help", "new", "clear", "context", "prompt", "tools", "chats", "chat"];
pub const PUBLIC_COMMANDS: [&str; 4] = ["new", "clear", "status", "stop"];

/// command definitions: (name, help). help None = spacer, Sub = subcommands
pub enum CmdDef {
    Text(&'static str),
    Sub(&'static [(&'static str, &'static str)]),
    Spacer,
}

pub const CMD_DEFS: &[(&str, CmdDef)] = &[
    ("new", CmdDef::Text("starts a new session")),
    ("clear", CmdDef::Text("clears chat history")),
    ("chats", CmdDef::Sub(&[("<page number>", "show previous chats, paginated")])),
    (
        "chat",
        CmdDef::Sub(&[
            ("", "show details about current chat"),
            ("<number>", "loads a chat by its number in the chats list"),
            ("rename <name>", "renames current chat"),
            ("category <category>", "puts chat in that category"),
        ]),
    ),
    ("search", CmdDef::Text("searches within your previous chats")),
    ("compress", CmdDef::Text("compresses your chat history")),
    ("export", CmdDef::Text("exports the current chat history to a file")),
    ("__SPACER__1", CmdDef::Spacer),
    ("prompt", CmdDef::Sub(&[("", "shows system prompt"), ("<module name>", "shows the system prompt for that module")])),
    ("prompts", CmdDef::Text("shows which prompts are active")),
    ("context", CmdDef::Text("shows full context being sent to AI")),
    ("history", CmdDef::Text("shows full chat history")),
    ("status", CmdDef::Text("shows status info")),
    ("__SPACER__2", CmdDef::Spacer),
    ("connect", CmdDef::Text("attempts to connect to the API")),
    ("reconnect", CmdDef::Text("reconnects to the API")),
    ("disconnect", CmdDef::Text("disconnects from the API")),
    ("__SPACER__3", CmdDef::Spacer),
    ("modules", CmdDef::Text("lists modules")),
    ("module", CmdDef::Text("enables/disables a module by name")),
    ("channel", CmdDef::Text("toggles a channel")),
    (
        "library",
        CmdDef::Sub(&[
            ("", "shows configured module libraries"),
            ("sync", "clones/updates all configured library repos"),
            ("list", "lists installable modules from the cloned libraries"),
            ("add <git url>", "adds a library repo url to the core settings"),
            ("remove <git url>", "removes a library repo url from the core settings"),
            ("install <name> [library]", "installs a module/channel from a library"),
            ("update <name> [library]", "updates an installed module from its library"),
            ("remove <name>", "removes an installed user module/channel (alias: uninstall)"),
        ]),
    ),
    (
        "tools",
        CmdDef::Sub(&[("", "lists tools available to the AI"), ("search <query>", "searches the tool catalog"), ("load <name> [name2 ...]", "loads tools by exact name")]),
    ),
    ("__SPACER__4", CmdDef::Spacer),
    ("config", CmdDef::Text("explore, view, and set config settings")),
    ("restart", CmdDef::Text("restarts the server")),
    ("stop", CmdDef::Text("stops the AI in it's tracks")),
    ("__SPACER__5", CmdDef::Spacer),
    ("ping", CmdDef::Text("test command that echoes Pong!")),
    ("help", CmdDef::Text("shows this help")),
];

fn cmd_prefix() -> String {
    config::get_str(&["core", "cmd_prefix"]).unwrap_or_else(|| "/".into())
}

/// all available commands: {topic: {"/cmd": description}}
pub fn get_commands(ch: &Channel) -> IndexMap<String, IndexMap<String, String>> {
    let prefix = cmd_prefix();
    let mut commands: IndexMap<String, IndexMap<String, String>> = IndexMap::new();
    let mut core = IndexMap::new();
    for (name, def) in CMD_DEFS {
        match def {
            // python: commands["core"][""] = "" (all spacers collapse onto one key)
            CmdDef::Spacer => {
                core.insert(String::new(), String::new());
            }
            CmdDef::Sub(subs) => {
                for (sub, desc) in subs.iter() {
                    core.insert(format!("{}{}", prefix, format!("{} {}", name, sub).trim()), desc.to_string());
                }
            }
            CmdDef::Text(desc) => {
                core.insert(format!("{}{}", prefix, name), desc.to_string());
            }
        }
    }
    commands.insert("core".into(), core);
    for (module_name, module) in ch.manager().modules().iter() {
        let mut module_cmds = IndexMap::new();
        for spec in module.commands() {
            match &spec.help {
                CommandHelp::Sub(subs) => {
                    for (sub, desc) in subs {
                        module_cmds.insert(format!("{}{}", prefix, format!("{} {}", spec.name, sub).trim()), desc.clone());
                    }
                }
                CommandHelp::Text(t) => {
                    module_cmds.insert(format!("{}{}", prefix, spec.name), t.clone());
                }
            }
        }
        if !module_cmds.is_empty() && !module_name.is_empty() {
            commands.entry(module_name.clone()).or_default().extend(module_cmds);
        }
    }
    commands
}

/// whether any compiled-in module marks the command as temporary (send_to_ai=False)
fn command_is_temporary(ch: &Channel, cmd: &str) -> bool {
    ch.manager().all_module_commands().iter().any(|c| c.name == cmd && c.temporary)
}

fn check_if_temporary(ch: &Channel, cmd: &str) -> bool {
    GHOST.contains(&cmd) || command_is_temporary(ch, cmd) || !config::get_bool(&["model", "use_tools"], false)
}

/// splits a command line. None when it is not a command or can't be parsed
pub fn extract_cmd(text: &str) -> Option<(String, String, Vec<String>)> {
    let content = text.trim();
    let prefix = cmd_prefix();
    let rest = content.strip_prefix(prefix.as_str())?;
    let parts = shlex::split(rest)?;
    let cmd = parts.first().cloned().unwrap_or_default();
    let args = parts.into_iter().skip(1).collect();
    Some((prefix, cmd, args))
}

/// process a command typed by the user. Ok(None) means "not a command".
pub async fn process_input(ch: &Arc<Channel>, content: &str, authorized: bool) -> anyhow::Result<Option<String>> {
    let Some((prefix, cmd, args)) = extract_cmd(content) else { return Ok(None) };
    let use_temporary = check_if_temporary(ch, &cmd);
    let mut args_display = String::new();
    if !args.is_empty() {
        args_display.push(' ');
        args_display.push_str(&args.join(" "));
    }
    ch.chat.messages_add(&json!({"role": "user", "content": format!("{}{}{}", prefix, cmd, args_display)}), true, use_temporary).await;

    if cmd.is_empty() {
        return Err(UnauthorizedException("Command was somehow zero length. Aborting for security reasons.".into()).into());
    }
    if !authorized && !PUBLIC_COMMANDS.contains(&cmd.as_str()) {
        return Err(UnauthorizedException("You are not authorized to run admin commands.".into()).into());
    }
    if !content.starts_with(&prefix) {
        return Ok(None);
    }
    let result = dispatch(ch, &cmd.to_lowercase().trim().to_string(), args).await;
    let shown = result.clone().unwrap_or_else(|| "None".into());
    ch.chat.messages_add(&json!({"role": "assistant", "content": shown}), true, use_temporary).await;
    Ok(result)
}

/// delegate to a core command or a module command
async fn dispatch(ch: &Arc<Channel>, cmd: &str, args: Vec<String>) -> Option<String> {
    let core_result = match cmd {
        "help" => Some(cmd_help(ch, &args)),
        "ping" => Some(Ok("pong!".to_string())),
        "new" => Some(cmd_new(ch).await),
        "clear" => Some(cmd_clear(ch).await),
        "chats" => Some(cmd_chats(ch, &args)),
        "search" => Some(cmd_search(ch, &args)),
        "chat" => Some(cmd_chat(ch, &args).await),
        "compress" => Some(cmd_compress(ch).await),
        "connect" => Some(cmd_connect(ch).await),
        "reconnect" => Some(cmd_reconnect(ch).await),
        "disconnect" => Some(cmd_disconnect(ch).await),
        "status" => Some(cmd_status(ch).await),
        "modules" => Some(cmd_modules(ch)),
        "module" => Some(cmd_module(ch, &args).await),
        "channel" => Some(cmd_channel(ch, &args).await),
        "library" => Some(cmd_library(ch, &args).await),
        "tools" => Some(cmd_tools(ch, &args)),
        "config" => Some(cmd_config(ch, args.clone()).await),
        "history" => Some(cmd_history(ch, &args).await),
        "context" => Some(cmd_context(ch).await),
        "prompt" => Some(cmd_prompt(ch, &args).await),
        "prompts" => Some(cmd_prompts(ch).await),
        "export" => Some(cmd_export(ch).await),
        "restart" => Some(cmd_restart(ch).await),
        "stop" => Some(cmd_stop(ch).await),
        _ => None,
    };
    if let Some(r) = core_result {
        return Some(match r {
            Ok(s) => s,
            Err(e) => {
                ch.log_error(&format!("error executing {}", cmd), &e.to_string());
                format!("error: {}", e)
            }
        });
    }

    let manager = ch.manager();
    for (_, module) in manager.modules().iter() {
        if module.commands().iter().any(|c| c.name == cmd) {
            match module.run_command(cmd, args.clone()).await {
                Ok(r) => return r,
                Err(e) => ch.log_error("error while executing command", &crate::detail_error(&e)),
            }
        }
    }
    Some("no such command! check /help".into())
}

type CmdResult = anyhow::Result<String>;

fn cmd_help(ch: &Channel, args: &[String]) -> CmdResult {
    let prefix = cmd_prefix();
    let help = get_commands(ch);
    if let Some(topic) = args.first() {
        let Some(cmds) = help.get(topic) else { return Ok(format!("that's not a valid topic! check {}help", prefix)) };
        let mut out = Vec::new();
        for (command, desc) in cmds {
            if desc.is_empty() {
                out.push(String::new());
            } else {
                out.push(format!("{:<30} {}", command, desc));
            }
        }
        return Ok(out.join("\n"));
    }
    let topics: Vec<String> = help.keys().map(|t| format!("- {}", t)).collect();
    Ok(format!("use {}help with one of the following topics:\n{}\n\nexample: {}help core", prefix, topics.join("\n"), prefix))
}

async fn cmd_new(ch: &Arc<Channel>) -> CmdResult {
    let id = ch.chat.new_chat("general", "New chat", None).await;
    Ok(if id.is_empty() { "Failed to start new session".into() } else { "New session started.".into() })
}

async fn cmd_clear(ch: &Arc<Channel>) -> CmdResult {
    Ok(if ch.chat.clear().await? { "Chat history wiped.".into() } else { "Failed to wipe chat history".into() })
}

fn sorted_chats(ch: &Channel) -> Vec<Value> {
    ch.chat.get_all()
}

fn cmd_chats(ch: &Channel, args: &[String]) -> CmdResult {
    let chats = sorted_chats(ch);
    if chats.is_empty() {
        return Ok("No saved chats found.".into());
    }
    let page_size = 10usize;
    let mut page: usize = 1;
    if let Some(a) = args.first() {
        if let Ok(p) = a.parse::<i64>() {
            page = if p < 1 { 1 } else { p as usize };
        }
    }
    let total = chats.len();
    let total_pages = if total > 0 { total.div_ceil(page_size) } else { 1 };
    let mut start = (page - 1) * page_size;
    let mut end = (start + page_size).min(total);
    if start >= total {
        page = total_pages.max(1);
        start = (page - 1) * page_size;
        end = (start + page_size).min(total);
    }
    let mut result = format!("saved chats for {} (page {}/{}, showing {}-{} of {}):\n", ch.name, page, total_pages, start + 1, end, total);
    for (i, c) in chats[start..end].iter().enumerate() {
        let title = c.get("title").map(py_str).unwrap_or_else(|| "None".into());
        result.push_str(&format!("- [{}] {}\n", start + i + 1, pyjson::take_chars(&title, 50)));
    }
    Ok(result)
}

fn cmd_search(ch: &Channel, args: &[String]) -> CmdResult {
    let query = args.join(" ");
    let found = ch.chat.search(&query, 20, true);
    if found.is_empty() {
        return Ok("no results found".into());
    }
    let mut out = format!("Found these chats containing '{}':\n\n", query);
    for c in found {
        let updated = c.get("updated").and_then(|u| u.as_str()).unwrap_or("");
        let date_str = crate::globals::parse_isoformat(updated).map(|d| d.format("%m/%d/%y %H:%M:%S").to_string()).unwrap_or_default();
        out.push_str(&format!("[{}] [{}] {}\n", date_str, c.get("id").map(py_str).unwrap_or_default(), c.get("title").map(py_str).unwrap_or_default()));
    }
    Ok(out)
}

async fn cmd_chat(ch: &Arc<Channel>, args: &[String]) -> CmdResult {
    if args.is_empty() {
        let tags = ch.chat.get("tags").filter(truthy).and_then(|t| t.as_array().cloned()).map(|a| a.iter().map(py_str).collect::<Vec<_>>().join(", ")).unwrap_or_else(|| "None".into());
        let meta = ch.chat.get("metadata").filter(truthy).and_then(|m| m.as_object().cloned()).unwrap_or_default();
        let meta_str = if meta.is_empty() {
            "None".to_string()
        } else {
            format!("\n{}", meta.iter().map(|(k, v)| format!("  {}: {}", k, py_str(v))).collect::<Vec<_>>().join("\n"))
        };
        return Ok(format!(
            "== chat info ==\ntitle: {}\ncategory: {}\ntags: {}\nmetadata: {}",
            ch.chat.get("title").map(|v| py_str(&v)).unwrap_or_else(|| "None".into()),
            ch.chat.get("category").map(|v| py_str(&v)).unwrap_or_else(|| "None".into()),
            tags,
            meta_str
        ));
    }
    match args[0].to_lowercase().trim() {
        "rename" => {
            let name = args[1..].join(" ");
            ch.chat.set("title", json!(name), None)?;
            Ok(format!("chat renamed to {}", name))
        }
        "category" => {
            let cat = args[1..].join(" ");
            ch.chat.set("category", json!(cat), None)?;
            Ok(format!("chat categorised into {}", cat))
        }
        _ => {
            let arg = &args[0];
            if let Ok(position) = arg.parse::<i64>() {
                let all = sorted_chats(ch);
                if position < 1 || position as usize > all.len() {
                    return Ok("chat with that position doesn't exist".into());
                }
                let target = all[position as usize - 1].get("id").map(py_str).unwrap_or_default();
                let loaded = ch.chat.load(&target).await?;
                return Ok(if loaded { "chat loaded".into() } else { "failed to load chat".into() });
            }
            let loaded = ch.chat.load(arg).await?;
            Ok(if loaded { "chat loaded".into() } else { "failed to load chat".into() })
        }
    }
}

async fn cmd_compress(ch: &Arc<Channel>) -> CmdResult {
    ch.push(json!("Compressing your chat history.."), true).await;
    match ch.context.compress().await {
        Some(_) => Ok("chat compressed".into()),
        None => Ok("ai returned a blank response!".into()),
    }
}

async fn cmd_connect(ch: &Arc<Channel>) -> CmdResult {
    let api = ch.manager().api();
    if api.connected() {
        return Ok("Already connected.".into());
    }
    Ok(match api.connect().await {
        Ok(()) => "Connected!".into(),
        Err(e) => format!("Error while connecting: {}", e),
    })
}

async fn cmd_reconnect(ch: &Arc<Channel>) -> CmdResult {
    Ok(match ch.manager().api().reconnect().await {
        Ok(()) => "Reconnected".into(),
        Err(e) => format!("Error while reconnecting: {}", e),
    })
}

async fn cmd_disconnect(ch: &Arc<Channel>) -> CmdResult {
    ch.manager().api().disconnect().await;
    Ok("Disconnected from API".into())
}

fn fmt_thousands(n: i64) -> String {
    let s = n.abs().to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 {
        format!("-{}", out)
    } else {
        out
    }
}

pub fn format_context_size(size: &Value) -> String {
    let n = |p: &[&str]| pyjson::get_in(size, p).and_then(|v| v.as_i64()).unwrap_or(0);
    [
        format!("Total: {} / {} tokens ({}% full)", fmt_thousands(n(&["total_tokens"])), fmt_thousands(n(&["max_context"])), n(&["percent_full"])),
        format!("System prompt: {} tokens | {} words", fmt_thousands(n(&["system_prompt", "tokens"])), fmt_thousands(n(&["system_prompt", "words"]))),
        format!("Tools: {} active | {} tokens | {} words", n(&["tools", "active"]), fmt_thousands(n(&["tools", "tokens"])), fmt_thousands(n(&["tools", "words"]))),
        format!("Message history: {} tokens | {} words", fmt_thousands(n(&["message_history", "tokens"])), fmt_thousands(n(&["message_history", "words"]))),
        format!("End prompt: {} tokens | {} words", fmt_thousands(n(&["end_prompt", "tokens"])), fmt_thousands(n(&["end_prompt", "words"]))),
    ]
    .join("\n")
}

async fn cmd_status(ch: &Arc<Channel>) -> CmdResult {
    let api = ch.manager().api();
    let status = api.get_status();
    let mut lines = vec!["== API Status ==".to_string()];
    lines.push(format!("Connected: {}", if api.connected() { "Yes" } else { "No" }));
    let model = status.get("model").filter(|v| truthy(v)).map(py_str).unwrap_or_else(|| "Not set".into());
    lines.push(format!("Model: {}", model));
    lines.push(format!("URL: {}", status.get("url").map(py_str).unwrap_or_else(|| "None".into())));
    if api.connected() {
        lines.push(String::new());
        lines.push("== Context Size ==".into());
        lines.push(format_context_size(&ch.context.get_size().await));
    }
    Ok(lines.join("\n"))
}

fn cmd_modules(ch: &Channel) -> CmdResult {
    let disabled = config::get_str_list(&["modules", "disabled"]).join("\n");
    let loaded = ch.manager().modules().keys().cloned().collect::<Vec<_>>().join("\n");
    Ok(format!("== loaded ==\n{}\n\n== disabled ==\n{}\n", loaded, disabled))
}

async fn cmd_module(ch: &Arc<Channel>, args: &[String]) -> CmdResult {
    let Some(name) = args.first() else { return Ok("please provide a name of the module to toggle".into()) };
    let mut all = config::get_str_list(&["modules", "enabled"]);
    all.extend(config::get_str_list(&["modules", "disabled"]));
    all.extend(config::get_str_list(&["user_modules", "enabled"]));
    all.extend(config::get_str_list(&["user_modules", "disabled"]));
    if !all.contains(name) {
        return Ok("module with that name doesn't exist".into());
    }
    let mut enabled = config::get_str_list(&["modules", "enabled"]);
    enabled.extend(config::get_str_list(&["user_modules", "enabled"]));
    let new_state = !enabled.contains(name);
    ch.manager().toggle_module(name);
    Ok(format!("module {}\nrestart required - run /restart", if new_state { "enabled" } else { "disabled" }))
}

async fn cmd_channel(ch: &Arc<Channel>, args: &[String]) -> CmdResult {
    let Some(name) = args.first() else { return Ok("please provide a name of the channel to toggle".into()) };
    if !crate::channel::get_available_channels().contains(name) {
        return Ok("channel with that name doesn't exist".into());
    }
    let mut enabled = config::get_str_list(&["channels", "enabled"]);
    enabled.extend(config::get_str_list(&["user_channels", "enabled"]));
    let new_state = !enabled.contains(name);
    ch.manager().toggle_channel(name);
    Ok(format!("channel {}\nrestart required - run /restart", if new_state { "enabled" } else { "disabled" }))
}

async fn cmd_library(ch: &Arc<Channel>, args: &[String]) -> CmdResult {
    let manager = ch.manager();
    let mm = manager.module_manager();
    if args.is_empty() {
        let mut lines = vec!["== module libraries ==".to_string()];
        let repos = mm.list_repos();
        if repos.is_empty() {
            lines.push("no repos configured - add git urls under core -> module_libraries".into());
        } else {
            lines.push("configured repos:".into());
            lines.extend(repos.iter().map(|r| format!("  - {}", r)));
        }
        let libs = mm.list_libraries();
        if libs.is_empty() {
            lines.push("nothing cloned yet - run /library sync".into());
        } else {
            lines.push(format!("cloned: {}", libs.keys().cloned().collect::<Vec<_>>().join(", ")));
        }
        lines.push("\nsubcommands: sync | list | add <git url> | remove <git url> | install <name> | update <name> | remove <name>".into());
        return Ok(lines.join("\n"));
    }
    let sub = args[0].to_lowercase();
    let resolve = |name: &str| -> Vec<String> {
        mm.list_libraries().iter().filter(|(_, p)| mm.find_source(p, name).is_some()).map(|(n, _)| n.clone()).collect()
    };
    match sub.as_str() {
        "sync" => {
            if mm.list_repos().is_empty() {
                return Ok("no module libraries configured - add git urls under core -> module_libraries first".into());
            }
            ch.push(json!("syncing module libraries.."), true).await;
            if !mm.load().await {
                return Ok("sync failed - check the logs".into());
            }
            let available = mm.list_available();
            let n = mm.list_libraries().len();
            Ok(format!(
                "synced {} librar{} - {} installable entries found. use /library list",
                n,
                if n == 1 { "y" } else { "ies" },
                available.len()
            ))
        }
        "list" => {
            let available = mm.list_available();
            if available.is_empty() {
                return Ok("no installable modules found - run /library sync first".into());
            }
            let mut url_by_lib: IndexMap<String, String> = mm.list_repos().into_iter().map(|r| (mm.repo_name(&r), r)).collect();
            for (lib, path) in mm.list_libraries() {
                if let Some(origin) = mm.git_origin(&path).await {
                    url_by_lib.insert(lib, origin);
                }
            }
            let mut groups: IndexMap<String, Vec<Value>> = IndexMap::new();
            for e in available {
                groups.entry(e["library"].as_str().unwrap_or("").to_string()).or_default().push(e);
            }
            let mut lines = vec!["== available from libraries ==".to_string()];
            for (lib, mut entries) in groups {
                entries.sort_by_key(|e| (!e["installed"].as_bool().unwrap_or(false), e["name"].as_str().unwrap_or("").to_lowercase()));
                lines.push(format!("\n{}:", url_by_lib.get(&lib).cloned().unwrap_or(lib.clone())));
                for e in entries {
                    let flag = if e["installed"].as_bool().unwrap_or(false) { " (installed)" } else { "" };
                    let doc = e["doc"].as_str().filter(|d| !d.is_empty()).map(|d| format!(" - {}", d)).unwrap_or_default();
                    lines.push(format!("  {} ({}){}{}", e["name"].as_str().unwrap_or(""), e["type"].as_str().unwrap_or(""), flag, doc));
                }
            }
            Ok(lines.join("\n"))
        }
        "add" => {
            let Some(url) = args.get(1).map(|s| s.trim().to_string()) else { return Ok("usage: /library add <git url>".into()) };
            if !(url.starts_with("http://") || url.starts_with("https://") || url.starts_with("git://") || url.starts_with("git@")) {
                return Ok("that doesn't look like a git url - expected http(s)://, git:// or git@...".into());
            }
            let mut repos = mm.list_repos();
            if repos.contains(&url) {
                return Ok(format!("'{}' is already in the library list", url));
            }
            repos.push(url.clone());
            config::update(|c| {
                if let Some(core) = c.get_mut("core").and_then(|v| v.as_object_mut()) {
                    core.insert("module_libraries".into(), json!(repos));
                }
            });
            Ok(format!("added {} - run /library sync to clone it", url))
        }
        "install" | "update" => {
            let usage = format!("usage: /library {} <name> [library]", sub);
            let Some(name) = args.get(1).cloned() else { return Ok(usage) };
            let library = match args.get(2) {
                Some(l) => l.clone(),
                None => {
                    let matches = resolve(&name);
                    if matches.is_empty() {
                        return Ok(if sub == "install" {
                            format!("'{}' not found in any cloned library - run /library sync first", name)
                        } else {
                            format!("'{}' not found in any cloned library - it may have been removed upstream", name)
                        });
                    }
                    if matches.len() > 1 {
                        return Ok(format!("'{}' exists in multiple libraries ({}) - add the library name to disambiguate", name, matches.join(", ")));
                    }
                    matches[0].clone()
                }
            };
            if sub == "install" {
                match mm.install(&library, &name) {
                    Ok(path) => Ok(format!("installed {} to {}\nrestart required - run /restart", name, path)),
                    Err(m) => Ok(m),
                }
            } else {
                match mm.update(&library, &name).await {
                    Ok(_) => Ok(format!("updated {} from library '{}'\nrestart required - run /restart", name, library)),
                    Err(m) => Ok(m),
                }
            }
        }
        "remove" | "uninstall" => {
            let Some(target) = args.get(1).map(|s| s.trim().to_string()) else {
                return Ok("usage: /library remove <git url> | /library remove <module name>".into());
            };
            if target.contains("://") || target.starts_with("git@") {
                let mut repos = mm.list_repos();
                if !repos.contains(&target) {
                    return Ok(format!("'{}' is not a configured repo url", target));
                }
                repos.retain(|r| r != &target);
                config::update(|c| {
                    if let Some(core) = c.get_mut("core").and_then(|v| v.as_object_mut()) {
                        core.insert("module_libraries".into(), json!(repos));
                    }
                });
                return Ok(format!("removed {} from the library list (any cloned copy stays on disk until deleted manually)", target));
            }
            match mm.uninstall(&target) {
                Ok(path) => Ok(format!("removed {} from {}\nrestart required - run /restart", target, path)),
                Err(m) => Ok(m),
            }
        }
        other => Ok(format!("unknown subcommand: {}. use sync | list | install | update | remove", other)),
    }
}

fn cmd_tools(ch: &Arc<Channel>, args: &[String]) -> CmdResult {
    if !config::get_bool(&["model", "use_tools"], false) {
        return Ok("Tools are turned off globally.".into());
    }
    let dynamic = config::get_bool(&["model", "dynamic_tool_loading"], true);
    if let Some(sub) = args.first() {
        let sub = sub.to_lowercase();
        if sub == "load" {
            if !dynamic {
                return Ok("Dynamic tool loading is disabled. All tools are already loaded at startup.".into());
            }
            let names = &args[1..];
            if names.is_empty() {
                return Ok("Usage: /tools load <module_name> [module_name2 ...]".into());
            }
            let mut lines = vec![format!("▣ Load result for {} module(s):", names.len())];
            for n in names {
                let r = ch.tool_loader.tools_load(std::slice::from_ref(n));
                if r.get("status").and_then(|s| s.as_str()) == Some("success") {
                    lines.push(format!("  ✔ {} tools loaded", n));
                } else {
                    lines.push(format!("   ✖ Failed to load {} tools!", n));
                }
            }
            return Ok(lines.join("\n"));
        }
        return Ok(format!("Unknown subcommand: {}. Use /tools load, or /tools (no args) for help.", sub));
    }
    let mut tool_map: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    for tool in ch.manager().tools() {
        let name = tool["function"]["name"].as_str().unwrap_or("").to_string();
        let module = if name == "tools_load" { "core".to_string() } else { name.split('_').next().unwrap_or("").to_string() };
        tool_map.entry(module).or_default().push(name);
    }
    let mut lines = vec!["== Active Tools ==".to_string()];
    if tool_map.is_empty() {
        lines.push("  (none)".into());
    } else {
        for (m, mut tools) in tool_map {
            tools.sort();
            lines.push(format!("  ▸ {}:", m));
            lines.extend(tools.iter().map(|t| format!("    • {}", t)));
        }
    }
    if dynamic {
        let active = ch.tool_loader.active_names();
        let mut by_module: std::collections::BTreeMap<String, Vec<String>> = Default::default();
        for (name, entry) in ch.tool_loader.catalog() {
            if active.contains(&name) {
                continue;
            }
            by_module.entry(entry.module).or_default().push(name);
        }
        lines.push("\n== Available Tools (Not Loaded) ==".into());
        if by_module.is_empty() {
            lines.push("  (none)".into());
        } else {
            for (m, mut names) in by_module {
                names.sort();
                lines.push(format!("  ▸ {}:", m));
                lines.extend(names.iter().map(|n| format!("    • {}", n)));
            }
        }
    } else {
        lines.push("\nℹ Dynamic tool loading is disabled. All tools are pre-loaded at startup.".into());
    }
    Ok(lines.join("\n"))
}

/// Converts string inputs from the CLI/Chat into appropriate types.
pub fn convert_type(value: &str) -> Value {
    let l = value.to_lowercase();
    if l == "true" || l == "on" {
        return json!(true);
    }
    if l == "false" || l == "off" {
        return json!(false);
    }
    let digits = value.trim_start_matches('-');
    if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
        if let Ok(i) = value.parse::<i64>() {
            return json!(i);
        }
    }
    if let Ok(f) = value.trim().parse::<f64>() {
        if !value.trim().is_empty() {
            return json!(f);
        }
    }
    json!(value)
}

async fn cmd_config(ch: &Arc<Channel>, mut args: Vec<String>) -> CmdResult {
    let root = Value::Object(config::snapshot());
    if args.is_empty() {
        return Ok(format!("Available settings: {}", root.as_object().unwrap().keys().cloned().collect::<Vec<_>>().join(", ")));
    }
    if matches!(args[0].trim(), "modules" | "user_modules" | "channels") {
        args.insert(1, "settings".into());
    }
    let mut is_set = false;
    let mut path = args.clone();
    let mut value: Option<String> = None;
    let mut current = root.clone();
    for (i, arg) in args.iter().enumerate() {
        match current.get(arg) {
            Some(v) if v.is_object() => current = v.clone(),
            Some(_) => {
                if i < args.len() - 1 {
                    is_set = true;
                    path = args[..=i].to_vec();
                    value = Some(args[i + 1..].join(" "));
                }
                break;
            }
            None => return Ok(format!("setting '{}' does not exist at that path.", arg)),
        }
    }
    if is_set {
        return Ok(set_config_value(ch, &path, value.as_deref().unwrap_or("")).await);
    }
    Ok(get_config_value(&path))
}

async fn set_config_value(ch: &Arc<Channel>, path: &[String], value: &str) -> String {
    if path.is_empty() {
        return "error: Path cannot be empty".into();
    }
    let typed = convert_type(value.trim());
    let path_v = path.to_vec();
    let typed2 = typed.clone();
    let res: Result<(), String> = config::update(move |root| {
        let mut cur = root;
        for key in &path_v[..path_v.len() - 1] {
            cur = match cur.get_mut(key).and_then(|v| v.as_object_mut()) {
                Some(m) => m,
                None => return Err(format!("Error: Path {:?} is invalid. '{}' is not a dictionary.", path_v, key)),
            };
        }
        let last = &path_v[path_v.len() - 1];
        if cur.get(last).map(|v| v.is_object()).unwrap_or(false) {
            return Err("That's a settings group! Check which settings are in it instead of trying to set its value".into());
        }
        cur.insert(last.clone(), typed2);
        Ok(())
    });
    if let Err(e) = res {
        return e;
    }
    let shown = format!("Config updated: {} = {}", path.join(" -> "), py_str(&typed));
    if path.len() >= 3 && path[0] == "modules" {
        let module_name = &path[2];
        ch.manager().reload_module(module_name).await;
    }
    shown
}

fn get_config_value(path: &[String]) -> String {
    let Some(root_item) = config::get(&[path[0].as_str()]) else { return format!("{} is not a valid settings category", path[0]) };
    let mut sub = root_item;
    let mut last = path[0].clone();
    for key in &path[1..] {
        match sub.get(key).cloned() {
            Some(v) if !v.is_null() => sub = v,
            _ => return format!("{} is not a valid setting", key),
        }
        last = key.clone();
    }
    if let Value::Object(m) = &sub {
        if path.len() == 3 && path[1] == "settings" {
            let structure = config::get_module_structure();
            if let Some(info) = structure.get(&path[2]) {
                let mut lines = Vec::new();
                if let Some(schema) = info["settings"].as_object() {
                    for (s_name, s_schema) in schema {
                        let mut desc = String::new();
                        if let Value::Object(ss) = s_schema {
                            desc = ss.get("description").map(py_str).unwrap_or_default();
                            if ss.get("unsafe").map(truthy).unwrap_or(false) {
                                desc.push_str("\n  !! UNSAFE SETTING - ENABLE AT YOUR OWN RISK !!");
                            }
                            if let Some(Value::Object(opts)) = ss.get("options") {
                                let list: Vec<String> = opts.iter().map(|(k, v)| format!("{}: {}", k, py_str(v))).collect();
                                if !list.is_empty() {
                                    desc.push_str(&format!("\nYou can set this to one of:\n- {}", list.join("\n- ")));
                                }
                            }
                        }
                        lines.push(if desc.is_empty() { s_name.clone() } else { format!("{}: {}", s_name, desc) });
                    }
                }
                return if lines.is_empty() { format!("No settings found for {}", path[2]) } else { lines.join("\n\n") };
            }
        }
        return format!("Available settings in {}: {}", last, m.keys().cloned().collect::<Vec<_>>().join(", "));
    }
    py_str(&sub)
}

async fn cmd_history(ch: &Arc<Channel>, args: &[String]) -> CmdResult {
    if !config::get(&["api", "context_window"]).map(|v| truthy(&v)).unwrap_or(true) {
        return Ok("CONTEXT DISABLED".into());
    }
    let full = args.first().map(|a| a == "full").unwrap_or(false);
    let ctx = ch.context.get(crate::context::GetOpts { system_prompt: full, ..Default::default() }).await;
    let context = match ctx {
        Ok(Some(c)) if !c.is_empty() => c,
        _ => return Ok("BLANK".into()),
    };
    let mut display = Vec::new();
    for message in context {
        let role = message.get("role").and_then(|r| r.as_str()).unwrap_or("");
        if role == "tool" || role == "developer" {
            continue;
        }
        let f = ch.format_message(&message);
        display.push(format!("== {} ==\n{}", f.get("role").map(py_str).unwrap_or_default(), f.get("content").map(py_str).unwrap_or_default()));
    }
    display.push("---".into());
    let disabled = config::get_str_list(&["modules", "disabled_prompts"]);
    if !disabled.is_empty() {
        display.push(format!("== disabled prompts ==\n{}", disabled.join("\n")));
    }
    display.push(format!("== context size ==\n{}", format_context_size(&ch.context.get_size().await)));
    Ok(display.join("\n\n"))
}

async fn cmd_context(ch: &Arc<Channel>) -> CmdResult {
    let ctx = ch.context.get(Default::default()).await;
    let v = match ctx {
        Ok(Some(c)) => Value::Array(c),
        Ok(None) => Value::Null,
        Err(e) => json!(e.to_string()),
    };
    Ok(pyjson::dumps_indent(&v, 2))
}

async fn cmd_prompt(ch: &Arc<Channel>, args: &[String]) -> CmdResult {
    if !config::get(&["api", "context_window"]).map(|v| truthy(&v)).unwrap_or(true) {
        return Ok("CONTEXT DISABLED".into());
    }
    if args.is_empty() {
        let mut sp = ch.chat.get_system_prompt().await;
        if sp.is_empty() {
            sp = "BLANK".into();
        }
        let mut out = format!("=== system prompt ===\n{}", sp);
        let disabled = config::get_str_list(&["modules", "disabled_prompts"]);
        if !disabled.is_empty() {
            out.push_str("\n\n=== disabled prompts ===\n");
            out.push_str(&disabled.join("\n"));
        }
        let end = ch.chat.get_end_prompt(false).await;
        if !end.is_empty() {
            out.push_str(&format!("\n\n=== end prompts ===\n{}", end));
        }
        return Ok(out);
    }
    let name = args[0].trim().replace(' ', "_");
    match ch.manager().module(&name) {
        Some(m) => Ok(m.on_system_prompt().await?.filter(|s| !s.is_empty()).unwrap_or_else(|| "BLANK".into())),
        None => Ok("module not found".into()),
    }
}

async fn cmd_prompts(ch: &Arc<Channel>) -> CmdResult {
    let mut enabled = Vec::new();
    for (name, m) in ch.manager().modules().iter() {
        if m.on_system_prompt().await.ok().flatten().map(|s| !s.is_empty()).unwrap_or(false) {
            enabled.push(name.clone());
        }
    }
    Ok(format!("== modules with active prompts ==\n{}", enabled.join("\n")))
}

async fn cmd_export(ch: &Arc<Channel>) -> CmdResult {
    let export = ch.chat.export().await?;
    let export_dir = crate::paths::get_data_path(Some(&format!("chat_exports/{}", ch.name)))?;
    let title = ch.chat.get("title").map(|v| py_str(&v)).unwrap_or_else(|| "None".into());
    let mut file_name = format!("{}_{}", chrono::Local::now().format("%Y%m%d"), title);
    file_name = file_name.trim_matches('/').replace(' ', "_").replace("..", "");
    file_name = pyjson::take_chars(&file_name, 50);
    let file_path = crate::paths::get_data_path(Some(&format!("chat_exports/{}/{}.txt", ch.name, file_name)))?;
    std::fs::create_dir_all(&export_dir)?;
    std::fs::write(&file_path, export)?;
    Ok(format!("chat exported to {}", file_path))
}

async fn cmd_restart(ch: &Arc<Channel>) -> CmdResult {
    ch.manager().restart();
    Ok("restarting server".into())
}

async fn cmd_stop(ch: &Arc<Channel>) -> CmdResult {
    ch.manager().api().cancel().await;
    Ok("stopped!".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convert() {
        assert_eq!(convert_type("on"), json!(true));
        assert_eq!(convert_type("-5"), json!(-5));
        assert_eq!(convert_type("0.5"), json!(0.5));
        assert_eq!(convert_type("hello"), json!("hello"));
    }

    #[test]
    fn thousands() {
        assert_eq!(fmt_thousands(16768), "16,768");
        assert_eq!(fmt_thousands(100), "100");
    }

    #[test]
    fn extract() {
        assert_eq!(extract_cmd("/chat rename \"my chat\"").unwrap().2, vec!["rename", "my chat"]);
        assert!(extract_cmd("hello").is_none());
    }
}
