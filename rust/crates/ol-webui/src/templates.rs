//! Jinja2 templates via minijinja, plus the webui extension template system
//! (python `setup_ui_extensions`: ChoiceLoader of the core templates and a
//! PrefixLoader exposing each enabled module's `webui/templates` as `<module>/..`,
//! and the `extension_slot` / `extension_modals` / `extension_sidebars` globals).

use minijinja::value::{Kwargs, Value as JValue};
use minijinja::{AutoEscape, Environment, Error, ErrorKind, State};
use parking_lot::RwLock;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

/// what the template system needs from the running app
pub trait ExtHost: Send + Sync + 'static {
    /// settings of a module for use inside its shipped templates (`module_config`)
    fn module_config(&self, module: &str) -> serde_json::Value;
    fn log(&self, category: &str, message: &str);
}

/// jinja2 FileSystemLoader lookup: `..` segments are rejected
fn load_file(base: &Path, name: &str) -> Option<String> {
    let mut path = base.to_path_buf();
    for piece in name.split('/') {
        if piece == ".." || piece.contains('\\') {
            return None;
        }
        if piece.is_empty() || piece == "." {
            continue;
        }
        path.push(piece);
    }
    if !path.is_file() {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

fn sorted_html_files(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().to_string()).collect())
        .unwrap_or_default();
    names.sort();
    names.into_iter().filter(|n| dir.join(n).is_file()).collect()
}

fn err_str(e: &Error) -> String {
    ol_core::detail_error(e)
}

/// module templates mirroring `relative_path` that compile, sorted by module name
fn extension_templates(env: &Environment, dirs: &BTreeMap<String, PathBuf>, host: &Arc<dyn ExtHost>, relative_path: &str) -> Vec<String> {
    let mut found = Vec::new();
    for (module_name, dir) in dirs {
        let template_name = format!("{}/{}", module_name, relative_path);
        if !dir.join(relative_path).is_file() {
            continue;
        }
        if let Err(e) = env.get_template(&template_name) {
            host.log("webui", &format!("skipping UI extension '{}': {}", template_name, err_str(&e)));
            continue;
        }
        found.push(template_name);
    }
    found
}

/// all variables visible to the caller (jinja2 `ctx.get_all()`)
fn context_snapshot(state: &State) -> JValue {
    let mut vars: BTreeMap<String, JValue> = BTreeMap::new();
    for name in state.known_variables() {
        if let Some(v) = state.lookup(&name) {
            vars.insert(name.to_string(), v);
        }
    }
    JValue::from(vars)
}

fn render_slot(
    state: &State,
    env: &Environment,
    host: &Arc<dyn ExtHost>,
    template_name: &str,
    slot_name: &str,
) -> Result<Option<String>, Error> {
    let module = template_name.split('/').next().unwrap_or("");
    let module_config = JValue::from_serialize(host.module_config(module));
    let tmpl = env.get_template(template_name)?;
    let module_state = tmpl.eval_to_state(minijinja::context! { module_config => module_config })?;
    if !module_state.exports().contains(&slot_name) {
        return Ok(None);
    }
    let Some(mac) = module_state.lookup(slot_name) else { return Ok(None) };
    let wants_ctx = mac
        .get_attr("arguments")
        .ok()
        .and_then(|a| a.try_iter().ok().map(|mut it| it.any(|x| x.as_str() == Some("ctx"))))
        .unwrap_or(false);
    let out = if wants_ctx {
        let kwargs = Kwargs::from_iter([("ctx", context_snapshot(state))]);
        mac.call(&module_state, &[JValue::from(kwargs)])?
    } else {
        mac.call(&module_state, &[])?
    };
    Ok(Some(out.to_string()))
}

/// build a fresh environment for the core template folder plus module template folders
pub fn build_env(template_path: PathBuf, dirs: BTreeMap<String, PathBuf>, host: Arc<dyn ExtHost>) -> Environment<'static> {
    let mut env = Environment::new();
    // starlette's Jinja2Templates enables autoescape for every template
    env.set_auto_escape_callback(|_| AutoEscape::Html);
    env.set_unknown_method_callback(minijinja_contrib::pycompat::unknown_method_callback);

    let loader_core = template_path.clone();
    let loader_dirs = dirs.clone();
    env.set_loader(move |name: &str| -> Result<Option<String>, Error> {
        if let Some(s) = load_file(&loader_core, name) {
            return Ok(Some(s));
        }
        if let Some((prefix, rest)) = name.split_once('/') {
            if let Some(dir) = loader_dirs.get(prefix) {
                return Ok(load_file(dir, rest));
            }
        }
        Ok(None)
    });

    env.add_function("url_for", |name: String, kwargs: Kwargs| -> Result<String, Error> {
        let path: String = kwargs.get("path")?;
        kwargs.assert_all_used()?;
        if name == "assets" {
            Ok(format!("/assets/{}", path))
        } else {
            Err(Error::new(ErrorKind::InvalidOperation, format!("No route exists for name \"{}\" and params \"path\".", name)))
        }
    });

    // extension_modals(): every .html directly in <module>/webui/templates/modals/
    {
        let dirs = dirs.clone();
        let host = host.clone();
        env.add_function("extension_modals", move |state: &State| -> Vec<JValue> {
            let env = state.env();
            let mut found = Vec::new();
            for (module_name, dir) in &dirs {
                let modal_root = dir.join("modals");
                if !modal_root.is_dir() {
                    continue;
                }
                let module_config = JValue::from_serialize(host.module_config(module_name));
                for filename in sorted_html_files(&modal_root) {
                    if !filename.ends_with(".html") || filename == "index.html" {
                        continue;
                    }
                    let template_name = format!("{}/modals/{}", module_name, filename);
                    if let Err(e) = env.get_template(&template_name) {
                        host.log("webui", &format!("skipping UI modal '{}': {}", template_name, err_str(&e)));
                        continue;
                    }
                    let mut entry: BTreeMap<&str, JValue> = BTreeMap::new();
                    entry.insert("template", JValue::from(template_name));
                    entry.insert("config", module_config.clone());
                    found.push(JValue::from(entry));
                }
            }
            found
        });
    }

    // extension_sidebars(): every .html directly in <module>/webui/templates/sidebars/,
    // with a same-named .svg inlined as the tab icon
    {
        let dirs = dirs.clone();
        let host = host.clone();
        env.add_function("extension_sidebars", move |state: &State| -> Vec<JValue> {
            let env = state.env();
            let mut found = Vec::new();
            for (module_name, dir) in &dirs {
                let sidebar_root = dir.join("sidebars");
                if !sidebar_root.is_dir() {
                    continue;
                }
                let module_config = JValue::from_serialize(host.module_config(module_name));
                for filename in sorted_html_files(&sidebar_root) {
                    if !filename.ends_with(".html") {
                        continue;
                    }
                    let name = filename[..filename.len() - 5].to_string();
                    let template_name = format!("{}/sidebars/{}", module_name, filename);
                    if let Err(e) = env.get_template(&template_name) {
                        host.log("webui", &format!("skipping UI sidebar '{}': {}", template_name, err_str(&e)));
                        continue;
                    }
                    let mut icon = String::new();
                    let icon_path = sidebar_root.join(format!("{}.svg", name));
                    if icon_path.is_file() {
                        match std::fs::read_to_string(&icon_path) {
                            Ok(s) => icon = s,
                            Err(e) => host.log("webui", &format!("skipping sidebar icon '{}': {}", icon_path.display(), e)),
                        }
                    }
                    let label = ol_core::pyjson::title(&name.replace(['_', '-'], " "));
                    let mut entry: BTreeMap<&str, JValue> = BTreeMap::new();
                    entry.insert("module", JValue::from(module_name.clone()));
                    entry.insert("name", JValue::from(name));
                    entry.insert("label", JValue::from(label));
                    entry.insert("template", JValue::from(template_name));
                    entry.insert("icon", JValue::from_safe_string(icon));
                    entry.insert("config", module_config.clone());
                    found.push(JValue::from(entry));
                }
            }
            found
        });
    }

    // extension_slot(name, relative_path=None): renders macro `name` from every module
    // template mirroring the calling template's path
    {
        let dirs = dirs.clone();
        let host = host.clone();
        env.add_function("extension_slot", move |state: &State, slot_name: String, relative_path: Option<String>| -> JValue {
            let relative_path = relative_path.filter(|p| !p.is_empty()).unwrap_or_else(|| state.name().to_string());
            if relative_path.is_empty() {
                return JValue::from_safe_string(String::new());
            }
            let env = state.env();
            let mut parts = Vec::new();
            for template_name in extension_templates(env, &dirs, &host, &relative_path) {
                match render_slot(state, env, &host, &template_name, &slot_name) {
                    Ok(Some(s)) => parts.push(s),
                    Ok(None) => {}
                    Err(e) => host.log("webui", &format!("skipping extension slot '{}' in '{}': {}", slot_name, template_name, err_str(&e))),
                }
            }
            JValue::from_safe_string(parts.join("\n"))
        });
    }

    env
}

/// (file count, newest mtime, total size) over the template folders: changes trigger a
/// rebuild, like jinja2's auto_reload
fn fingerprint(paths: &[&Path]) -> (u64, u128, u64) {
    fn walk(p: &Path, acc: &mut (u64, u128, u64)) {
        let Ok(rd) = std::fs::read_dir(p) else { return };
        for e in rd.filter_map(|e| e.ok()) {
            let Ok(md) = e.metadata() else { continue };
            if md.is_dir() {
                walk(&e.path(), acc);
            } else {
                acc.0 += 1;
                acc.2 += md.len();
                if let Ok(m) = md.modified() {
                    let ns = m.duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
                    acc.1 = acc.1.max(ns);
                }
            }
        }
    }
    let mut acc = (0, 0, 0);
    for p in paths {
        walk(p, &mut acc);
    }
    acc
}

struct Built {
    env: Arc<Environment<'static>>,
    dirs: BTreeMap<String, PathBuf>,
    fingerprint: (u64, u128, u64),
}

/// the live template environment; rebuilt when the extension folders change
/// (setup_ui_extensions) or when template files change on disk
pub struct TemplateStore {
    template_path: PathBuf,
    host: Arc<dyn ExtHost>,
    built: RwLock<Built>,
}

impl TemplateStore {
    pub fn new(template_path: PathBuf, dirs: BTreeMap<String, PathBuf>, host: Arc<dyn ExtHost>) -> Self {
        let built = Self::build(&template_path, dirs, &host);
        Self { template_path, host, built: RwLock::new(built) }
    }

    fn build(template_path: &Path, dirs: BTreeMap<String, PathBuf>, host: &Arc<dyn ExtHost>) -> Built {
        let mut paths: Vec<&Path> = vec![template_path];
        paths.extend(dirs.values().map(|p| p.as_path()));
        let fp = fingerprint(&paths);
        let env = Arc::new(build_env(template_path.to_path_buf(), dirs.clone(), host.clone()));
        Built { env, dirs, fingerprint: fp }
    }

    /// replace the module template folders (python setup_ui_extensions)
    pub fn set_dirs(&self, dirs: BTreeMap<String, PathBuf>) {
        let built = Self::build(&self.template_path, dirs, &self.host);
        *self.built.write() = built;
    }

    pub fn dirs(&self) -> BTreeMap<String, PathBuf> {
        self.built.read().dirs.clone()
    }

    pub fn env(&self) -> Arc<Environment<'static>> {
        let (env, dirs, fp) = {
            let b = self.built.read();
            (b.env.clone(), b.dirs.clone(), b.fingerprint)
        };
        let mut paths: Vec<&Path> = vec![&self.template_path];
        paths.extend(dirs.values().map(|p| p.as_path()));
        if fingerprint(&paths) == fp {
            return env;
        }
        let built = Self::build(&self.template_path, dirs, &self.host);
        let env = built.env.clone();
        *self.built.write() = built;
        env
    }

    pub fn render(&self, name: &str, ctx: JValue) -> Result<String, Error> {
        let env = self.env();
        let tmpl = env.get_template(name)?;
        tmpl.render(ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;

    #[derive(Default)]
    struct TestHost {
        logs: Mutex<Vec<String>>,
    }
    impl ExtHost for TestHost {
        fn module_config(&self, module: &str) -> serde_json::Value {
            serde_json::json!({"enable_webui_extension": true, "show_welcome_card": module != "hidden"})
        }
        fn log(&self, category: &str, message: &str) {
            self.logs.lock().push(format!("[{}] {}", category, message));
        }
    }

    fn write(p: &Path, s: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, s).unwrap();
    }

    #[test]
    fn extension_machinery() {
        let tmp = tempfile::tempdir().unwrap();
        let core = tmp.path().join("core");
        write(&core.join("page.html"), "A{{ extension_slot('top') }}|{% include 'part.html' %}|{% for m in extension_modals() %}{% set module_config = m.config %}{% include m.template %}{% endfor %}|{% for p in extension_sidebars() %}[{{ p.label }}:{{ p.icon }}]{% endfor %}|{{ url_for('assets', path='js/x.js') }}|{{ config.get('title') }}|{{ config.get('missing') is none }}|{{ version }}");
        write(&core.join("part.html"), "{% set message = 'hi' %}P{{ extension_slot('below') }}");
        let m1 = tmp.path().join("m1");
        write(&m1.join("page.html"), "{% macro top() %}<b>m1top {{ module_config.get('show_welcome_card') }}</b>{% endmacro %}");
        write(&m1.join("part.html"), "{% macro below(ctx) %}ctx:{{ ctx.get('message') }}{% endmacro %}");
        write(&m1.join("modals/a_modal.html"), "MODAL{{ module_config.get('enable_webui_extension') }}");
        write(&m1.join("modals/index.html"), "LEGACY");
        write(&m1.join("sidebars/my_panel.html"), "panel");
        write(&m1.join("sidebars/my_panel.svg"), "<svg/>");
        let m2 = tmp.path().join("m2");
        write(&m2.join("page.html"), "{% macro top() %}broken {{ {% endmacro %}");
        write(&m2.join("part.html"), "{% macro other() %}x{% endmacro %}");

        let host = Arc::new(TestHost::default());
        let mut dirs = BTreeMap::new();
        dirs.insert("m1".to_string(), m1.clone());
        dirs.insert("m2".to_string(), m2.clone());
        let store = TemplateStore::new(core.clone(), dirs, host.clone());
        let out = store
            .render(
                "page.html",
                minijinja::context! { config => JValue::from_serialize(serde_json::json!({"title": "T<"})), version => 2.0 },
            )
            .unwrap();
        assert_eq!(out, "A<b>m1top true</b>|Pctx:hi|MODALtrue|[My Panel:<svg/>]|/assets/js/x.js|T&lt;|true|2.0");
        let logs = host.logs.lock().join("\n");
        assert!(logs.contains("skipping UI extension 'm2/page.html'"), "{}", logs);

        // module templates are reachable by prefix, core templates win
        assert!(store.env().get_template("m1/modals/a_modal.html").is_ok());
        assert!(store.env().get_template("m1/../core/page.html").is_err());

        // templates are reloaded when they change on disk
        write(&core.join("part.html"), "CHANGED and longer");
        let out = store.render("page.html", minijinja::context! { config => JValue::from_serialize(serde_json::json!({})), version => 2.0 }).unwrap();
        assert!(out.contains("CHANGED"));
    }
}
