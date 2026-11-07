//! Port of channels/simple_webui: a javascript-less HTML chat page (Flask + werkzeug
//! in python, axum here). `GET|POST /` renders the page (POST queues a message and
//! answers immediately with an auto-refreshing "processing" page), `GET /clear`
//! clears the chat. The page template is python's `HTML_TEMPLATE`, verbatim, rendered
//! with minijinja using Jinja2/markupsafe autoescaping rules.

#[path = "../../../generated/channels/simple_webui.rs"]
mod meta;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use ol_core::channel::{Channel, ChannelBehavior, SendArgs};
use ol_core::pyjson::py_str;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use regex::Regex;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

/// python `HTML_TEMPLATE`
pub const HTML_TEMPLATE: &str = include_str!("simple_webui.html");

/// python's `<.*?>` tag-stripping pattern
static HTML_CLEAN_PATTERN: Lazy<Regex> = Lazy::new(|| Regex::new("<.*?>").unwrap());

/// the class-level `settings` (not a pure literal in python, so ported by hand)
pub fn settings() -> Value {
    json!({
        "network_mode": {
            "type": "select",
            "options": {
                "local": "Allows only the device OpenLumara is running on to access the WebUI (sets hostname to `localhost`)",
                "internet": "Allows any device to access the WebUI (sets hostname to `0.0.0.0`)",
                "custom": "Use the custom hostname defined below"
            },
            "default": "local"
        },
        "custom_host": {
            "description": "If you want to use a custom hostname, set it here. If you don't know what that is, don't bother with this! Just use the network mode setting on either local or internet.",
            "default": null
        },
        "port": {
            "description": "What port to run the WebUI on. Set this to 80 to be able to access it like a normal website, and anything else to access it on that port (for example http://yourdomain.org:3000)",
            "default": 5000
        },
        "allow_commands": {
            "description": "Whether to allow running `/commands` in this instance. Do NOT enable this if your instance is exposed to the public.",
            "default": false
        },
        "enable_logging": {
            "description": "Whether to log all messages flowing through this channel to the console. Helps you see what's going on if you have exposed it as a public instance!",
            "default": true
        },
        "name": {
            "default": "OpenLumara",
            "description": "The name of your webUI instance. Shows up in headers and the like."
        },
        "welcome_message": {
            "default": "<h2>Welcome!</h2>\n<p>Send a message below to start chatting with the AI.</p>",
            "description": "Raw HTML content to serve the user as the welcome message, shown when the page is first opened, before sending their first message to the AI"
        },
        "font": {
            "default": "monospace",
            "description": "The font to use for your page"
        },
        "primary_color": {
            "default": "#FFFFFF",
            "description": "Color (in hex code format) to use as the primary accent color for the page"
        },
        "secondary_color": {
            "default": "#CCCCCC",
            "description": "Color (in hex code format) to use as the secondary accent color for the page"
        }
    })
}

crate::descriptor_from_meta!("simple_webui", settings, factory);

fn factory() -> Arc<dyn ChannelBehavior> {
    Arc::new(SimpleWebui::default())
}

#[derive(Default)]
struct PageState {
    /// (role, content)
    messages: Vec<(String, String)>,
    processing: bool,
}

#[derive(Default)]
pub struct SimpleWebui {
    state: Arc<Mutex<PageState>>,
    template: Arc<Mutex<String>>,
    shutdown_tx: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    server: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

#[derive(Clone)]
struct AppState {
    ch: Arc<Channel>,
    state: Arc<Mutex<PageState>>,
    template: Arc<Mutex<String>>,
}

pub fn clean_html(s: &str) -> String {
    HTML_CLEAN_PATTERN.replace_all(s, "").into_owned()
}

/// python's `str.replace` chain applied to the template in on_ready
pub fn prepare_template(primary: &str, secondary: &str, font: &str, welcome: &str) -> String {
    HTML_TEMPLATE
        .replace("var(--primary-color)", primary)
        .replace("var(--secondary-color)", secondary)
        .replace("var(--font)", font)
        .replace("<<welcome_message>>", welcome)
}

/// markupsafe.escape
pub fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&#34;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// Flask `render_template_string` (autoescaping on; None renders as "None")
pub fn render(template: &str, channel_name: &Value, messages: &[(String, String)], processing: bool) -> Result<String, minijinja::Error> {
    let mut env = minijinja::Environment::new();
    env.set_formatter(|out, _state, value| {
        let s = if value.is_safe() {
            value.to_string()
        } else if value.is_none() {
            "None".to_string()
        } else if value.kind() == minijinja::value::ValueKind::Bool {
            if value.is_true() { "True".into() } else { "False".into() }
        } else if value.is_undefined() {
            String::new()
        } else {
            html_escape(&value.to_string())
        };
        out.write_str(&s).map_err(minijinja::Error::from)
    });
    env.add_function("url_for", |endpoint: &str| -> String {
        match endpoint {
            "clear_chat" => "/clear".into(),
            _ => "/".into(),
        }
    });
    let msgs: Vec<Value> = messages.iter().map(|(r, c)| json!({"role": r, "content": c})).collect();
    env.render_str(
        template,
        minijinja::context! {
            channel_name => minijinja::Value::from_serialize(channel_name),
            messages => minijinja::Value::from_serialize(&msgs),
            last_user_message => "",
            processing => processing,
        },
    )
}

fn html_response(status: StatusCode, body: String) -> Response {
    let mut r = Response::new(Body::from(body));
    *r.status_mut() = status;
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    r
}

/// werkzeug's redirect response
fn redirect_index() -> Response {
    let mut r = html_response(
        StatusCode::FOUND,
        "<!doctype html>\n<html lang=en>\n<title>Redirecting...</title>\n<h1>Redirecting...</h1>\n<p>You should be redirected automatically to the target URL: <a href=\"/\">/</a>. If not, click the link.\n".into(),
    );
    r.headers_mut().insert(header::LOCATION, HeaderValue::from_static("/"));
    r
}

fn werkzeug_error(status: StatusCode, title: &str, text: &str) -> Response {
    html_response(
        status,
        format!("<!doctype html>\n<html lang=en>\n<title>{} {}</title>\n<h1>{}</h1>\n<p>{}</p>\n", status.as_u16(), title, title, text),
    )
}

fn internal_error() -> Response {
    werkzeug_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "Internal Server Error",
        "The server encountered an internal error and was unable to complete your request. Either the server is overloaded or there is an error in the application.",
    )
}

fn page(app: &AppState, messages: &[(String, String)], processing: bool) -> Response {
    let template = app.template.lock().clone();
    let name = app.ch.config.get1("name").unwrap_or(Value::Null);
    match render(&template, &name, messages, processing) {
        Ok(html) => html_response(StatusCode::OK, html),
        Err(e) => {
            app.ch.log(&app.ch.name, &format!("Error while rendering page: {}", e));
            internal_error()
        }
    }
}

async fn index_get(State(app): State<AppState>) -> Response {
    let (messages, processing) = {
        let st = app.state.lock();
        (st.messages.clone(), st.processing)
    };
    page(&app, &messages, processing)
}

/// `request.form.get("message", "")` for urlencoded bodies
pub fn form_message(body: &[u8]) -> String {
    url::form_urlencoded::parse(body).find(|(k, _)| k == "message").map(|(_, v)| v.into_owned()).unwrap_or_default()
}

async fn index_post(State(app): State<AppState>, body: Bytes) -> Response {
    // snapshot before the new message is added (python renders this snapshot)
    let messages = app.state.lock().messages.clone();

    let user_input = clean_html(form_message(&body).trim());
    if user_input.is_empty() {
        return redirect_index();
    }

    {
        let mut st = app.state.lock();
        st.messages.push(("user".into(), user_input.clone()));
        if app.ch.config.get_bool("enable_logging", true) {
            app.ch.log(&app.ch.name, &format!("USER: {}", user_input));
        }
        st.processing = true;
    }

    let app2 = app.clone();
    tokio::spawn(async move {
        let args = SendArgs { commands_authorized: app2.ch.config.get_bool("allow_commands", false), ..Default::default() };
        let response = app2.ch.send(json!(user_input), args).await;
        if let Some(r) = response.filter(|r| r.as_object().map(|o| !o.is_empty()).unwrap_or(false)) {
            if let Some(content) = r.get("content").filter(|c| !c.is_null()).map(py_str) {
                let content = clean_html(&content);
                let mut st = app2.state.lock();
                st.messages.push(("assistant".into(), content.clone()));
                if app2.ch.config.get_bool("enable_logging", true) {
                    app2.ch.log(&app2.ch.name, &format!("ASSISTANT: {}", content));
                }
            }
        }
        app2.state.lock().processing = false;
    });

    page(&app, &messages, true)
}

async fn clear_chat(State(app): State<AppState>) -> Response {
    let res = app.ch.chat.clear().await;
    {
        let mut st = app.state.lock();
        st.messages.clear();
        st.processing = false;
    }
    match res {
        Ok(_) => redirect_index(),
        Err(e) => {
            app.ch.log(&app.ch.name, &format!("Error while clearing chat: {}", e));
            internal_error()
        }
    }
}

async fn not_found() -> Response {
    werkzeug_error(
        StatusCode::NOT_FOUND,
        "Not Found",
        "The requested URL was not found on the server. If you entered the URL manually please check your spelling and try again.",
    )
}

async fn method_not_allowed() -> Response {
    werkzeug_error(StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed", "The method is not allowed for the requested URL.")
}

fn router(app: AppState) -> Router {
    Router::new()
        .route("/", get(index_get).post(index_post))
        .route("/clear", get(clear_chat))
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .with_state(app)
}

#[async_trait::async_trait]
impl ChannelBehavior for SimpleWebui {
    async fn on_ready(&self, ch: &Arc<Channel>) -> anyhow::Result<()> {
        let cfg = |k: &str| ch.config.get1(k).map(|v| py_str(&v)).unwrap_or_else(|| "None".into());
        *self.template.lock() = prepare_template(&cfg("primary_color"), &cfg("secondary_color"), &cfg("font"), &cfg("welcome_message"));
        {
            let mut st = self.state.lock();
            st.messages.clear();
            st.processing = false;
        }

        let host = crate::util::resolve_host(&ch.config);
        let port = crate::util::resolve_port(&ch.config, 5000);
        let listener = tokio::net::TcpListener::bind((host.as_str(), port)).await?;
        let app = AppState { ch: ch.clone(), state: self.state.clone(), template: self.template.clone() };
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        *self.shutdown_tx.lock() = Some(tx);
        let ch2 = ch.clone();
        let task = tokio::spawn(async move {
            let res = axum::serve(listener, router(app))
                .with_graceful_shutdown(async move {
                    let _ = rx.await;
                })
                .await;
            if let Err(e) = res {
                ch2.log(&ch2.name, &format!("Simple WebUI server error: {}", e));
            }
        });
        *self.server.lock() = Some(task);
        ch.log(&ch.name, &format!("Simple WebUI is running at http://{}:{}", host, port));
        Ok(())
    }

    async fn on_push(&self, _ch: &Arc<Channel>, message: Value) -> anyhow::Result<()> {
        let content = message.get("content").map(py_str).unwrap_or_default();
        if content.is_empty() {
            return Ok(());
        }
        self.state.lock().messages.push(("assistant".into(), clean_html(&content)));
        Ok(())
    }

    async fn on_shutdown(&self, _ch: &Arc<Channel>) -> anyhow::Result<()> {
        if let Some(tx) = self.shutdown_tx.lock().take() {
            let _ = tx.send(());
        }
        let task = self.server.lock().take();
        if let Some(task) = task {
            let abort = task.abort_handle();
            if tokio::time::timeout(Duration::from_secs(2), task).await.is_err() {
                abort.abort();
            }
        }
        Ok(())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tpl() -> String {
        prepare_template("#FFFFFF", "#CCCCCC", "monospace", "<h2>Welcome!</h2>")
    }

    #[test]
    fn template_substitution() {
        let t = tpl();
        assert!(!t.contains("var(--"));
        assert!(t.contains("color: #FFFFFF;"));
        assert!(t.contains("font-family: monospace, Verdana"));
        assert!(t.contains("<h2>Welcome!</h2>"));
    }

    #[test]
    fn render_empty_state() {
        let html = render(&tpl(), &json!("OpenLumara"), &[], false).unwrap();
        assert!(html.contains("<title>OpenLumara WebUI</title>"));
        assert!(html.contains("<h2>Welcome!</h2>"));
        assert!(html.contains(r#"<a href="/clear">Clear Chat</a>"#));
        assert!(html.contains(r#"<form method="POST" action="/">"#));
        assert!(!html.contains("http-equiv=\"refresh\""));
        assert!(!html.contains("Processing your message"));
    }

    #[test]
    fn render_messages_reversed_and_escaped() {
        let msgs = vec![("user".to_string(), "a < b & \"c\" 'd'".to_string()), ("assistant".to_string(), "second".to_string())];
        let html = render(&tpl(), &json!("X"), &msgs, true).unwrap();
        assert!(html.contains("a &lt; b &amp; &#34;c&#34; &#39;d&#39;"));
        assert!(html.find("second").unwrap() < html.find("a &lt; b").unwrap());
        assert!(html.contains("<meta http-equiv=\"refresh\" content=\"1\">"));
        assert!(html.contains("Processing your message... (page will auto-refresh)"));
        assert!(!html.contains("Welcome!"));
    }

    #[test]
    fn none_name_renders_as_python_none() {
        let html = render(&tpl(), &Value::Null, &[], false).unwrap();
        assert!(html.contains("<title>None WebUI</title>"));
    }

    #[test]
    fn helpers() {
        assert_eq!(clean_html("hi <b>there</b> <script>x</script>"), "hi there x");
        assert_eq!(form_message(b"message=hello+world%21&x=1"), "hello world!");
        assert_eq!(form_message(b"x=1"), "");
    }
}
