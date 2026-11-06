//! Port of the static side of channels/webui/channel.py: the `/assets` mount, `/ext-assets/<module>/`
//! (module-shipped assets), `/themes/<file>.css` (theme sidecar css), `/api/themes` (+ `/{family}`),
//! the service worker `/sw.js` (cache version and precache list generated per request), the PWA
//! manifest, the icons and the favicon.

use crate::server::{api_result, detail_response, internal_error, Shared};
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Json;
use axum::Router;
use serde_json::{json, Value};
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;
use tower::ServiceExt;
use tower_http::services::{ServeDir, ServeFile};

/// the static / theme / pwa routes plus the `/assets` mount
pub fn add_routes(router: Router<Arc<Shared>>, app: &Arc<Shared>) -> Router<Arc<Shared>> {
    router
        .nest_service("/assets", ServeDir::new(app.webui_dir.join("assets")).append_index_html_on_directories(false))
        .route("/ext-assets/{module_name}/{*asset_path}", get(ext_module_assets))
        .route("/api/themes", get(get_themes))
        .route("/api/themes/{family_name}", get(get_theme))
        .route("/themes/{file_name}", get(theme_sidecar_css))
        .route("/sw.js", get(service_worker))
        .route("/manifest.json", get(manifest))
        .route("/icon-192.png", get(icon_192))
        .route("/icon-512.png", get(icon_512))
        .route("/favicon.ico", get(favicon))
}

/// serves one file with the headers starlette's FileResponse sends (content type, etag, ranges)
async fn serve_file(path: PathBuf, req: Request) -> Response {
    match ServeFile::new(path).oneshot(req).await {
        Ok(resp) => resp.map(Body::new),
        Err(never) => match never {},
    }
}

fn real(path: &FsPath) -> Option<PathBuf> {
    std::fs::canonicalize(path).ok()
}

/// python `module_dir` lookup + realpath containment check for `/ext-assets/<module>/<path>`
async fn ext_module_assets(State(app): State<Arc<Shared>>, Path((module_name, asset_path)): Path<(String, String)>, req: Request) -> Response {
    let Some(module_dir) = app.asset_dirs().remove(&module_name) else {
        return detail_response(StatusCode::NOT_FOUND, "Unknown module or no assets");
    };
    let root = real(&module_dir).unwrap_or(module_dir.clone());
    let full = real(&module_dir.join(&asset_path));
    match full {
        Some(full) if full.starts_with(&root) && full != root && full.is_file() => serve_file(full, req).await,
        _ => detail_response(StatusCode::NOT_FOUND, "Asset not found"),
    }
}

/// python `theme.get("dark")`-style membership test (`"dark" in theme_data`)
fn has_mode(theme: &Value, mode: &str) -> Result<bool, String> {
    match theme {
        Value::Object(map) => Ok(map.contains_key(mode)),
        Value::Array(items) => Ok(items.iter().any(|v| v.as_str() == Some(mode))),
        Value::String(s) => Ok(s.contains(mode)),
        other => Err(format!("argument of type '{}' is not iterable", crate::util::py_type_name(other))),
    }
}

/// python `get_themes`: every theme family (except base.json) with the modes it supports
async fn get_themes(State(app): State<Arc<Shared>>) -> Response {
    let Some(ch) = app.ch() else { return internal_error() };
    let themes_dir = app.webui_dir.join("themes");
    let Ok(entries) = std::fs::read_dir(&themes_dir) else { return internal_error() };

    let mut theme_list: Vec<Value> = Vec::new();
    for entry in entries.filter_map(|e| e.ok()) {
        let file_name = entry.file_name().to_string_lossy().to_string();
        if !file_name.ends_with(".json") || file_name == "base.json" {
            continue;
        }
        let family = &file_name[..file_name.len() - 5];
        let filepath = themes_dir.join(&file_name);
        let parsed = std::fs::read_to_string(&filepath)
            .map_err(|e| e.to_string())
            .and_then(|s| serde_json::from_str::<Value>(&s).map_err(|e| e.to_string()))
            .and_then(|data| Ok((has_mode(&data, "dark")?, has_mode(&data, "light")?)));
        match parsed {
            Ok((dark, light)) => theme_list.push(json!({"name": family, "dark": dark, "light": light})),
            Err(e) => ch.log(&ch.name, &format!("failed to read theme {}: {}", filepath.display(), e)),
        }
    }
    theme_list.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Json(theme_list).into_response()
}

/// python `get_theme`: the full theme json of one family, or an api envelope describing the failure
async fn get_theme(State(app): State<Arc<Shared>>, Path(family_name): Path<String>) -> Response {
    let Some(ch) = app.ch() else { return internal_error() };
    let filepath = app.webui_dir.join("themes").join(format!("{}.json", family_name));
    if !filepath.exists() {
        return Json(api_result(json!(format!("Theme family '{}' not found", family_name)), false)).into_response();
    }
    let loaded = std::fs::read_to_string(&filepath)
        .map_err(|e| e.to_string())
        .and_then(|s| serde_json::from_str::<Value>(&s).map_err(|e| e.to_string()));
    match loaded {
        Ok(theme) => Json(theme).into_response(),
        Err(e) => {
            ch.log(&ch.name, &format!("failed to load theme {}: {}", filepath.display(), e));
            Json(api_result(json!(format!("Failed to load theme: {}", e)), false)).into_response()
        }
    }
}

/// python `theme_sidecar_css`: only `.css` files, and only from the themes folder
async fn theme_sidecar_css(State(app): State<Arc<Shared>>, Path(file_name): Path<String>, req: Request) -> Response {
    if !file_name.ends_with(".css") {
        return detail_response(StatusCode::NOT_FOUND, "Only CSS files are served");
    }
    let themes_dir = real(&app.webui_dir.join("themes")).unwrap_or_else(|| app.webui_dir.join("themes"));
    match real(&themes_dir.join(&file_name)) {
        Some(full) if full.starts_with(&themes_dir) && full.is_file() => serve_file(full, req).await,
        _ => detail_response(StatusCode::NOT_FOUND, "Theme CSS not found"),
    }
}

/// every file below `dir` (os.walk: symlinked directories are not descended into)
fn walk_files(dir: &FsPath, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for entry in rd.filter_map(|e| e.ok()) {
        let path = entry.path();
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            walk_files(&path, out);
        } else if !path.is_dir() {
            out.push(path);
        }
    }
}

fn mtime_secs(path: &FsPath) -> Option<f64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    modified.duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs_f64())
}

/// python `generate_cache_version`: `v<newest mtime in channels/webui>`, falling back to channel.py
fn generate_cache_version(webui_dir: &FsPath) -> String {
    let mut latest = mtime_secs(&webui_dir.join("channel.py")).unwrap_or(0.0);
    let mut files = Vec::new();
    walk_files(webui_dir, &mut files);
    for file in files {
        if let Some(m) = mtime_secs(&file) {
            if m > latest {
                latest = m;
            }
        }
    }
    format!("v{}", latest as i64)
}

/// python `service_worker`: fills `{{VERSION}}` and `{{FILE_LIST}}` in sw.js
async fn service_worker(State(app): State<Arc<Shared>>) -> Response {
    let base = &app.webui_dir;
    let assets_base = base.join("assets");

    let mut files_to_cache: Vec<String> = Vec::new();
    for sub in ["js", "css"] {
        let dir_path = assets_base.join(sub);
        if !dir_path.is_dir() {
            continue;
        }
        let mut files = Vec::new();
        walk_files(&dir_path, &mut files);
        for full in files {
            if let Ok(rel) = full.strip_prefix(&assets_base) {
                files_to_cache.push(format!("/assets/{}", rel.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/")));
            }
        }
    }
    files_to_cache.sort();

    let Ok(template) = std::fs::read_to_string(base.join("sw.js")) else { return internal_error() };
    let version = generate_cache_version(base);
    let file_list = format!("{}\n", files_to_cache.iter().map(|f| format!("\"{}\"", f)).collect::<Vec<_>>().join(",\n    "));
    let code = template.replace("{{VERSION}}", &version).replace("{{FILE_LIST}}", &file_list);

    match Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/javascript")
        .header(header::CACHE_CONTROL, "no-cache, no-store, must-revalidate")
        .header(header::PRAGMA, "no-cache")
        .header(header::EXPIRES, "0")
        .body(Body::from(code))
    {
        Ok(r) => r,
        Err(_) => internal_error(),
    }
}

/// python `manifest`: the PWA manifest, parsed and re-encoded as json
async fn manifest(State(app): State<Arc<Shared>>) -> Response {
    let parsed = std::fs::read_to_string(app.webui_dir.join("manifest.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok());
    match parsed {
        Some(v) => Json(v).into_response(),
        None => internal_error(),
    }
}

async fn icon_192(State(app): State<Arc<Shared>>, req: Request) -> Response {
    serve_file(app.webui_dir.join("icon-192.png"), req).await
}

async fn icon_512(State(app): State<Arc<Shared>>, req: Request) -> Response {
    serve_file(app.webui_dir.join("icon-512.png"), req).await
}

async fn favicon(State(app): State<Arc<Shared>>, req: Request) -> Response {
    serve_file(app.webui_dir.join("favicon.ico"), req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_modes_follow_python_membership() {
        assert_eq!(has_mode(&json!({"dark": {}, "light": {}}), "dark"), Ok(true));
        assert_eq!(has_mode(&json!({"dark": {}}), "light"), Ok(false));
        assert_eq!(has_mode(&json!(["dark"]), "dark"), Ok(true));
        assert_eq!(has_mode(&json!(7), "dark"), Err("argument of type 'int' is not iterable".to_string()));
    }

    #[test]
    fn cache_version_is_v_plus_newest_mtime() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("sw.js"), "x").unwrap();
        std::fs::create_dir_all(dir.path().join("assets").join("js")).unwrap();
        std::fs::write(dir.path().join("assets").join("js").join("a.js"), "y").unwrap();
        let version = generate_cache_version(dir.path());
        assert!(version.starts_with('v'));
        assert!(version[1..].parse::<i64>().unwrap() > 1_000_000_000);
    }

    #[test]
    fn walk_collects_nested_files_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("js").join("stores")).unwrap();
        std::fs::write(dir.path().join("js").join("stores").join("s.js"), "").unwrap();
        std::fs::write(dir.path().join("js").join("main.js"), "").unwrap();
        let mut files = Vec::new();
        walk_files(&dir.path().join("js"), &mut files);
        let mut names: Vec<String> = files.iter().map(|p| p.file_name().unwrap().to_string_lossy().to_string()).collect();
        names.sort();
        assert_eq!(names, vec!["main.js", "s.js"]);
    }
}
