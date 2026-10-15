//! Settings structure (backend-side): port of the python `build_settings_structure`,
//! `build_field_settings`, `flatten_categories` & friends from channels/webui/channel.py.
//! /api/settings/load returns a ready-to-render category tree, /api/settings/save
//! accepts that same (edited) tree and flattens it back into raw config.

use indexmap::IndexMap;
use ol_core::pyjson::truthy;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Map, Value};

/// special keys that render with a bespoke widget instead of a generic field
pub const SPECIAL_FIELD_TYPES: [(&str, &str); 4] = [
    ("model.name", "model_select"),
    ("api.url", "api_url"),
    ("api.key", "api_key"),
    ("model.reasoning_effort", "reasoning_effort_slider"),
];

pub const MODULE_CATEGORY_KEYS: [&str; 4] = ["modules", "user_modules", "channels", "user_channels"];

static WORD_START: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b\w").unwrap());

/// python format_label: underscores to spaces, capitalize every word start
pub fn format_label(key: &str) -> String {
    let spaced = key.replace('_', " ");
    WORD_START.replace_all(&spaced, |c: &regex::Captures| c[0].to_uppercase()).into_owned()
}

fn label_value(key: &str) -> Value {
    json!(format_label(key))
}

pub fn is_toggle_list(data: &Value) -> bool {
    matches!(data, Value::Object(m) if m.get("enabled").map(|v| v.is_array()).unwrap_or(false)
        && m.get("disabled").map(|v| v.is_array()).unwrap_or(false))
}

pub fn detect_field_type(value: &Value, key: &str) -> String {
    if let Some((_, t)) = SPECIAL_FIELD_TYPES.iter().find(|(k, _)| *k == key) {
        return t.to_string();
    }
    match value {
        Value::Null => "text",
        Value::Bool(_) => "boolean",
        Value::Number(_) if !key.to_lowercase().ends_with("id") => "number",
        Value::Array(_) => "array",
        Value::String(s) => {
            if s.starts_with("http://") || s.starts_with("https://") {
                "url"
            } else if s.contains('\n') {
                "textarea"
            } else {
                "text"
            }
        }
        _ => "text",
    }
    .to_string()
}

/// `d.get(key) or None`
fn or_none(m: &Map<String, Value>, key: &str) -> Value {
    m.get(key).filter(|v| truthy(v)).cloned().unwrap_or(Value::Null)
}

/// `d.get(key)`
fn get_null(m: &Map<String, Value>, key: &str) -> Value {
    m.get(key).cloned().unwrap_or(Value::Null)
}

pub fn build_field_settings(obj: &Value, schema: &Value, prefix: &str) -> Map<String, Value> {
    let Value::Object(obj) = obj else { return Map::new() };
    if obj.is_empty() {
        return Map::new();
    }
    let empty = Map::new();
    let schema = schema.as_object().unwrap_or(&empty);
    let mut settings = Map::new();

    for (key, value) in obj {
        let full_key = if prefix.is_empty() { key.clone() } else { format!("{}.{}", prefix, key) };
        let field_schema = match schema.get(key) {
            Some(Value::Object(m)) if !m.is_empty() => m.clone(),
            _ => Map::new(),
        };
        let has_schema_definition = ["type", "default", "description"].iter().any(|k| field_schema.contains_key(*k));

        let entry = if has_schema_definition {
            let schema_value = field_schema.get("default").unwrap_or(value);
            let field_type = match field_schema.get("type") {
                Some(Value::String(s)) if s == "long_text" => json!("textarea"),
                Some(t) if !t.is_null() => t.clone(),
                _ => json!(detect_field_type(schema_value, &full_key)),
            };
            json!({
                "title": label_value(key),
                "type": field_type,
                "description": or_none(&field_schema, "description"),
                "unsafe": field_schema.get("unsafe").cloned().unwrap_or(json!(false)),
                "value": value,
                "options": or_none(&field_schema, "options"),
                "min": get_null(&field_schema, "min"),
                "max": get_null(&field_schema, "max"),
                "step": get_null(&field_schema, "step"),
                "depends": or_none(&field_schema, "depends"),
            })
        } else if value.is_object() && !is_toggle_list(value) {
            json!({
                "type": "object",
                "title": label_value(key),
                "description": or_none(&field_schema, "description"),
                "depends": or_none(&field_schema, "depends"),
                "settings": build_field_settings(value, &Value::Object(field_schema.clone()), &full_key),
            })
        } else if is_toggle_list(value) {
            json!({
                "type": "toggle_list",
                "title": label_value(key),
                "description": or_none(&field_schema, "description"),
                "value": value,
            })
        } else if value.is_array() {
            json!({
                "type": "array",
                "title": label_value(key),
                "description": or_none(&field_schema, "description"),
                "value": value,
            })
        } else {
            json!({
                "title": label_value(key),
                "type": detect_field_type(value, &full_key),
                "description": or_none(&field_schema, "description"),
                "unsafe": field_schema.get("unsafe").cloned().unwrap_or(json!(false)),
                "depends": or_none(&field_schema, "depends"),
                "value": value,
                "options": or_none(&field_schema, "options"),
                "min": get_null(&field_schema, "min"),
                "max": get_null(&field_schema, "max"),
                "step": get_null(&field_schema, "step"),
            })
        };
        settings.insert(key.clone(), entry);
    }
    settings
}

fn theme_category(title: &str, description: &str, order: i64) -> Value {
    json!({"title": title, "description": description, "order": order, "isThemeCategory": true})
}

/// module_info: {name: {"description","unsafe","category","settings_schema"}};
/// core_sections: {section: {"description","unsafe","settings_schema"}}
pub fn build_settings_structure(
    original_data: &Map<String, Value>,
    module_info: &IndexMap<String, Value>,
    core_sections: &Map<String, Value>,
) -> Map<String, Value> {
    let mut categories = Map::new();
    let mut order: i64 = 0;

    categories.insert("appearance".into(), theme_category("Appearance", "Theme and interface customization", order));
    order += 1;
    categories.insert("audio".into(), theme_category("Audio", "Audio settings", order));
    categories.insert("system_prompt".into(), theme_category("System Prompt", "See the current system prompt", 100));
    categories.insert("system_logs".into(), theme_category("System Logs", "Peek into the great unknown", 999));

    let section_order = |k: &str| -> Option<i64> {
        match k {
            "channels" => Some(10),
            "modules" => Some(11),
            "user_channels" => Some(13),
            "user_modules" => Some(14),
            _ => None,
        }
    };
    let module_library_order = 12;
    let module_library = || theme_category("Get Modules", "Install modules from external libraries", module_library_order);

    for (top_key, top_value) in original_data {
        let lower = top_key.to_lowercase();
        if lower == "theme" || lower == "theme_mode" {
            continue;
        }
        if top_key == "channels" && !categories.contains_key("module_library") {
            categories.insert("module_library".into(), module_library());
        }

        let display_title = match top_key.as_str() {
            "user_channels" => "Installed Channels".to_string(),
            "user_modules" => "Installed Modules".to_string(),
            _ => format_label(top_key),
        };
        let mut category = Map::new();
        category.insert("title".into(), json!(display_title));
        category.insert("description".into(), json!(format!("Configure {}", display_title.to_lowercase())));
        category.insert("order".into(), json!(section_order(top_key).unwrap_or(order)));
        order += 1;

        let empty = Map::new();
        // `(top_value or {})`
        let top_map = match top_value {
            Value::Object(m) => m,
            _ => &empty,
        };

        if MODULE_CATEGORY_KEYS.contains(&top_key.as_str()) {
            category.insert("isModuleCategory".into(), json!(true));
            category.insert("enabled".into(), top_map.get("enabled").filter(|v| truthy(v)).cloned().unwrap_or(json!([])));
            category.insert("disabled".into(), top_map.get("disabled").filter(|v| truthy(v)).cloned().unwrap_or(json!([])));

            let mut descriptions = Map::new();
            let mut unsafe_modules = Map::new();
            let mut module_categories = Map::new();
            for (item_name, info) in module_info {
                if let Some(d) = info.get("description").filter(|v| truthy(v)) {
                    descriptions.insert(item_name.clone(), d.clone());
                }
                if info.get("unsafe").map(truthy).unwrap_or(false) {
                    unsafe_modules.insert(item_name.clone(), json!(true));
                }
                if let Some(c) = info.get("category").filter(|v| truthy(v)) {
                    module_categories.insert(item_name.clone(), c.clone());
                }
            }
            category.insert("descriptions".into(), Value::Object(descriptions));
            category.insert("unsafeModules".into(), Value::Object(unsafe_modules));
            category.insert("moduleCategories".into(), Value::Object(module_categories));

            let mut settings = Map::new();
            if let Some(Value::Object(top_settings)) = top_map.get("settings") {
                for (item_name, item_settings) in top_settings {
                    if !truthy(item_settings) {
                        continue;
                    }
                    let item_info = module_info.get(item_name).filter(|v| truthy(v)).cloned().unwrap_or(json!({}));
                    let item_schema = item_info.get("settings_schema").filter(|v| truthy(v)).cloned().unwrap_or(json!({}));
                    settings.insert(
                        item_name.clone(),
                        json!({
                            "title": format_label(item_name),
                            "description": item_info.get("description").filter(|v| truthy(v)).cloned().unwrap_or(json!("")),
                            "unsafe": item_info.get("unsafe").cloned().unwrap_or(json!(false)),
                            "value": build_field_settings(item_settings, &item_schema, item_name),
                        }),
                    );
                }
            }
            category.insert("settings".into(), Value::Object(settings));
        } else {
            let section_schema = core_sections
                .get(top_key)
                .filter(|v| truthy(v))
                .and_then(|s| s.get("settings_schema"))
                .filter(|v| truthy(v))
                .cloned()
                .unwrap_or(json!({}));
            let settings = if top_value.is_object() && truthy(top_value) {
                build_field_settings(top_value, &section_schema, top_key)
            } else {
                Map::new()
            };
            category.insert("settings".into(), Value::Object(settings));
        }

        categories.insert(top_key.clone(), Value::Object(category));
    }

    if !categories.contains_key("module_library") {
        categories.insert("module_library".into(), module_library());
    }
    categories
}

/// reduces a (possibly edited) field tree back to plain key -> value config
pub fn flatten_field_settings(settings: &Map<String, Value>) -> Map<String, Value> {
    let mut result = Map::new();
    for (key, setting) in settings {
        let Value::Object(setting) = setting else { continue };
        let nested = setting.get("settings").filter(|v| truthy(v));
        if setting.get("type").and_then(|t| t.as_str()) == Some("object") && nested.is_some() {
            let nested = nested.and_then(|n| n.as_object()).cloned().unwrap_or_default();
            result.insert(key.clone(), Value::Object(flatten_field_settings(&nested)));
        } else {
            result.insert(key.clone(), setting.get("value").cloned().unwrap_or(Value::Null));
        }
    }
    result
}

/// inverse of build_settings_structure: category tree -> raw config dict
pub fn flatten_categories(categories: &Map<String, Value>) -> Map<String, Value> {
    let mut result = Map::new();
    for (cat_key, category) in categories {
        let Value::Object(category) = category else { continue };
        let has_settings = category.get("settings").map(truthy).unwrap_or(false);
        let has_enabled = category.contains_key("enabled");
        let has_disabled = category.contains_key("disabled");
        if !has_settings && !has_enabled && !has_disabled {
            continue;
        }
        let settings = category.get("settings").and_then(|s| s.as_object()).cloned().unwrap_or_default();
        if category.get("isModuleCategory").map(truthy).unwrap_or(false) {
            let mut entry = Map::new();
            if has_enabled {
                entry.insert("enabled".into(), category["enabled"].clone());
            }
            if has_disabled {
                entry.insert("disabled".into(), category["disabled"].clone());
            }
            if has_settings {
                let mut module_settings = Map::new();
                for (name, module) in &settings {
                    if let Value::Object(m) = module {
                        if let Some(v) = m.get("value").filter(|v| truthy(v)) {
                            let v = v.as_object().cloned().unwrap_or_default();
                            module_settings.insert(name.clone(), Value::Object(flatten_field_settings(&v)));
                        }
                    }
                }
                entry.insert("settings".into(), Value::Object(module_settings));
            }
            result.insert(cat_key.clone(), Value::Object(entry));
        } else {
            result.insert(cat_key.clone(), Value::Object(flatten_field_settings(&settings)));
        }
    }
    result
}

/// (module_info, core_sections) - python get_module_info()
pub fn get_module_info() -> (IndexMap<String, Value>, Map<String, Value>) {
    let mut module_info = IndexMap::new();
    for (name, data) in ol_core::config::get_module_structure() {
        let metadata = data.get("metadata").cloned().unwrap_or(json!({}));
        module_info.insert(
            name,
            json!({
                "description": metadata.get("doc").cloned().unwrap_or(json!("")),
                "unsafe": metadata.get("unsafe").cloned().unwrap_or(json!(false)),
                "category": metadata.get("category").cloned().unwrap_or(json!("")),
                "settings_schema": data.get("settings").cloned().unwrap_or(json!({})),
            }),
        );
    }
    let mut core_sections = Map::new();
    for (section, data) in ol_core::config::get_core_settings_structure() {
        let metadata = data.get("metadata").cloned().unwrap_or(json!({}));
        core_sections.insert(
            section,
            json!({
                "description": metadata.get("doc").cloned().unwrap_or(json!("")),
                "unsafe": metadata.get("unsafe").cloned().unwrap_or(json!(false)),
                "settings_schema": data.get("settings").cloned().unwrap_or(json!({})),
            }),
        );
    }
    (module_info, core_sections)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_and_types() {
        assert_eq!(format_label("enable_chat_header"), "Enable Chat Header");
        assert_eq!(format_label("api"), "Api");
        assert_eq!(format_label("max_context"), "Max Context");
        assert_eq!(detect_field_type(&json!(5), "model.max_tokens"), "number");
        assert_eq!(detect_field_type(&json!(5), "chat_id"), "text");
        assert_eq!(detect_field_type(&json!("https://x"), "a"), "url");
        assert_eq!(detect_field_type(&json!("a\nb"), "a"), "textarea");
        assert_eq!(detect_field_type(&json!(null), "a"), "text");
        assert_eq!(detect_field_type(&json!(true), "a"), "boolean");
        assert_eq!(detect_field_type(&json!("x"), "model.name"), "model_select");
        assert_eq!(detect_field_type(&json!({}), "a"), "text");
    }

    #[test]
    fn field_settings_shapes() {
        let obj = json!({
            "port": 3000,
            "network_mode": "local",
            "nested": {"a": 1},
            "lst": {"enabled": ["x"], "disabled": []},
            "arr": [1, 2],
            "plain": "text",
        });
        let schema = json!({
            "port": {"default": 3000, "description": "the port"},
            "network_mode": {"type": "select", "options": {"local": "l"}, "default": "local"},
            "plain": {"depends": "x"},
        });
        let s = build_field_settings(&obj, &schema, "webui");
        let keys: Vec<&String> = s["port"].as_object().unwrap().keys().collect();
        assert_eq!(keys, ["title", "type", "description", "unsafe", "value", "options", "min", "max", "step", "depends"]);
        assert_eq!(s["port"]["type"], "number");
        assert_eq!(s["port"]["description"], "the port");
        assert_eq!(s["port"]["unsafe"], false);
        assert_eq!(s["network_mode"]["type"], "select");
        assert_eq!(s["network_mode"]["options"], json!({"local": "l"}));
        assert_eq!(s["nested"]["type"], "object");
        assert_eq!(s["nested"]["settings"]["a"]["type"], "number");
        let keys: Vec<&String> = s["nested"].as_object().unwrap().keys().collect();
        assert_eq!(keys, ["type", "title", "description", "depends", "settings"]);
        assert_eq!(s["lst"]["type"], "toggle_list");
        assert_eq!(s["arr"]["type"], "array");
        // primitive without schema definition (only 'depends' present)
        let keys: Vec<&String> = s["plain"].as_object().unwrap().keys().collect();
        assert_eq!(keys, ["title", "type", "description", "unsafe", "depends", "value", "options", "min", "max", "step"]);
        assert_eq!(s["plain"]["depends"], "x");
    }

    #[test]
    fn structure_round_trip() {
        let config = json!({
            "core": {"cmd_prefix": "/", "auto_resume_chats": true},
            "api": {"url": "http://x/v1", "key": "k", "max_context": 16768},
            "model": {"name": "m", "nested": {"deep": 1}},
            "channels": {"enabled": ["webui"], "disabled": ["cli"], "settings": {"webui": {"port": 3000}, "cli": {}}},
            "user_channels": {"enabled": [], "disabled": [], "settings": {}},
            "modules": {"enabled": ["memory"], "disabled": [], "settings": {"memory": {"enable_webui_extension": true}}},
            "user_modules": {"enabled": [], "disabled": [], "settings": {}},
            "theme": "x",
        });
        let mut module_info = IndexMap::new();
        module_info.insert("webui".to_string(), json!({"description": "web", "unsafe": false, "category": "", "settings_schema": {"port": {"default": 3000}}}));
        module_info.insert("memory".to_string(), json!({"description": "mem", "unsafe": true, "category": "data", "settings_schema": {}}));
        let core_sections = json!({"api": {"settings_schema": {"url": {"default": "", "description": "", "type": null}}}});
        let cats = build_settings_structure(config.as_object().unwrap(), &module_info, core_sections.as_object().unwrap());
        let keys: Vec<&String> = cats.keys().collect();
        assert_eq!(
            keys,
            ["appearance", "audio", "system_prompt", "system_logs", "core", "api", "model", "module_library", "channels", "user_channels", "modules", "user_modules"]
        );
        assert_eq!(cats["core"]["order"], 0);
        assert_eq!(cats["api"]["order"], 1);
        assert_eq!(cats["channels"]["order"], 10);
        assert_eq!(cats["module_library"]["order"], 12);
        assert_eq!(cats["user_modules"]["title"], "Installed Modules");
        assert_eq!(cats["user_modules"]["description"], "Configure installed modules");
        assert_eq!(cats["api"]["settings"]["url"]["type"], "api_url");
        assert_eq!(cats["channels"]["descriptions"], json!({"webui": "web", "memory": "mem"}));
        assert_eq!(cats["channels"]["unsafeModules"], json!({"memory": true}));
        assert_eq!(cats["channels"]["moduleCategories"], json!({"memory": "data"}));
        // empty module settings are skipped
        assert!(cats["channels"]["settings"].get("cli").is_none());
        assert_eq!(cats["channels"]["settings"]["webui"]["value"]["port"]["value"], 3000);

        let flat = flatten_categories(&cats);
        assert_eq!(flat["api"], config["api"]);
        assert_eq!(flat["model"], config["model"]);
        assert_eq!(flat["core"], config["core"]);
        assert_eq!(flat["channels"], json!({"enabled": ["webui"], "disabled": ["cli"], "settings": {"webui": {"port": 3000}}}));
        assert_eq!(flat["modules"]["settings"]["memory"], json!({"enable_webui_extension": true}));
        assert!(flat.get("appearance").is_none());
        assert!(flat.get("theme").is_none());
        let keys: Vec<&String> = flat.keys().collect();
        assert_eq!(keys, ["core", "api", "model", "channels", "user_channels", "modules", "user_modules"]);
    }
}
