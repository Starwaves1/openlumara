//! Port of channels/api_bridge: an OpenAI-compatible HTTP API (FastAPI/uvicorn in
//! python, axum here) that forwards the last message of a chat completion request
//! into this channel's chat.
//!
//! Routes: `GET /v1` (307 → /v1/health), `POST /v1` (307 → /v1/chat/completions),
//! `GET /v1/health`, `GET /v1/models`, `POST /v1/chat/completions` (JSON or SSE).
//! FastAPI's auto-generated `/docs`, `/redoc` and `/openapi.json` are not served.

#[path = "../../../generated/channels/api_bridge.rs"]
mod meta;

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use futures::StreamExt;
use ol_core::channel::{Channel, ChannelBehavior, SendArgs, TextFormatOpts};
use ol_core::pyjson::{self, py_str, truthy};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tower_http::cors::{AllowHeaders, Any, CorsLayer};

crate::descriptor_from_meta!("api_bridge", meta::settings, factory);

fn factory() -> Arc<dyn ChannelBehavior> {
    Arc::new(ApiBridge::default())
}

#[derive(Default)]
pub struct ApiBridge {
    host: Mutex<String>,
    port: Mutex<u16>,
    shutdown_tx: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    stopped: Arc<tokio::sync::Notify>,
    running: Arc<std::sync::atomic::AtomicBool>,
}

fn unix_now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn json_response(status: StatusCode, v: &Value) -> Response {
    // starlette JSONResponse: compact separators, utf-8 (ensure_ascii=False)
    let mut r = Response::new(Body::from(serde_json::to_string(v).unwrap_or_default()));
    *r.status_mut() = status;
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    r
}

fn redirect_307(location: &'static str) -> Response {
    let mut r = Response::new(Body::empty());
    *r.status_mut() = StatusCode::TEMPORARY_REDIRECT;
    r.headers_mut().insert(header::LOCATION, HeaderValue::from_static(location));
    r
}

/// one `chat.completion.chunk` as python's `json.dumps` renders it
pub fn openai_chunk(chat_id: &str, created: i64, model: &Value, delta: &Value) -> String {
    pyjson::dumps(&json!({
        "id": chat_id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": {"content": delta},
            "finish_reason": null
        }]
    }))
}

pub fn completion_body(id: &str, created: i64, model: &Value, content: &Value) -> Value {
    json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": content},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0}
    })
}

pub fn models_body(created: i64) -> Value {
    json!({
        "object": "list",
        "data": [{"id": "openlumara", "object": "model", "created": created, "owned_by": "openlumara"}]
    })
}

fn internal_server_error() -> Response {
    let mut r = Response::new(Body::from("Internal Server Error"));
    *r.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
    r
}

async fn auth_middleware(State(ch): State<Arc<Channel>>, req: Request, next: Next) -> Response {
    if ch.config.get_bool("api_key_required", false) {
        let expected = format!("Bearer {}", ch.config.get1("api_key").map(|v| py_str(&v)).unwrap_or_default());
        let ok = req.headers().get(header::AUTHORIZATION).and_then(|h| h.to_str().ok()).map(|h| !h.is_empty() && h == expected).unwrap_or(false);
        if !ok {
            return json_response(
                StatusCode::UNAUTHORIZED,
                &json!({"error": {"message": "Invalid API key", "type": "invalid_request_error", "param": null, "code": "invalid_api_key"}}),
            );
        }
    }
    next.run(req).await
}

async fn chat_completions(State(ch): State<Arc<Channel>>, body: Bytes) -> Response {
    let Ok(body) = serde_json::from_slice::<Value>(&body) else {
        return internal_server_error();
    };
    let Some(obj) = body.as_object() else {
        return internal_server_error();
    };
    let messages = obj.get("messages").cloned().unwrap_or(Value::Null);
    if !truthy(&messages) {
        return json_response(StatusCode::BAD_REQUEST, &json!({"detail": "No messages provided"}));
    }
    let last_msg = match &messages {
        Value::Array(a) => a.last().cloned().unwrap_or(Value::Null),
        _ => return internal_server_error(),
    };
    let Some(last_msg) = last_msg.as_object() else {
        return internal_server_error();
    };
    let content = last_msg.get("content").cloned().unwrap_or(json!(""));
    let stream = obj.get("stream").map(truthy).unwrap_or(false);

    if stream {
        stream_handler(ch, content, json!("openlumara"))
    } else {
        completion_handler(ch, content, obj.get("model").cloned().unwrap_or(json!("openlumara"))).await
    }
}

async fn completion_handler(ch: Arc<Channel>, message: Value, model: Value) -> Response {
    let response = ch.send(message, SendArgs::authorized()).await;
    let Some(response) = response else {
        // python: format_message(None) raises
        let err = "'NoneType' object is not iterable";
        ch.log(&ch.name, &format!("Error in completion: {}", err));
        return json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &json!({"error": {"message": err, "type": "server_error", "param": null, "code": "internal_error"}}),
        );
    };
    let response = ch.format_message(&response);
    let content = response.get("content").cloned().unwrap_or(json!(""));
    let body = completion_body(&format!("chatcmpl-{}", uuid::Uuid::new_v4()), unix_now(), &model, &content);
    json_response(StatusCode::OK, &body)
}

fn stream_handler(ch: Arc<Channel>, message: Value, model: Value) -> Response {
    let chat_id = format!("chatcmpl-{}", uuid::Uuid::new_v4());
    let created = unix_now();
    let body = async_stream::stream! {
        // initial empty chunk to satisfy some clients
        yield Ok::<_, std::convert::Infallible>(format!("data: {}\n\n", openai_chunk(&chat_id, created, &model, &json!(""))));
        let send = ch.send_stream(message, SendArgs::authorized());
        let mut formatted = ch.format_stream_for_text(send, TextFormatOpts::default());
        while let Some(token) = formatted.next().await {
            if token.get("type").and_then(|t| t.as_str()) == Some("formatted") {
                let content = token.get("content").cloned().unwrap_or(Value::Null);
                yield Ok(format!("data: {}\n\n", openai_chunk(&chat_id, created, &model, &content)));
            }
        }
        yield Ok("data: [DONE]\n\n".to_string());
    };
    let mut r = Response::new(Body::from_stream(body));
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/event-stream; charset=utf-8"));
    r
}

async fn not_found() -> Response {
    json_response(StatusCode::NOT_FOUND, &json!({"detail": "Not Found"}))
}

async fn method_not_allowed() -> Response {
    json_response(StatusCode::METHOD_NOT_ALLOWED, &json!({"detail": "Method Not Allowed"}))
}

pub fn router(ch: Arc<Channel>) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::DELETE, Method::GET, Method::HEAD, Method::OPTIONS, Method::PATCH, Method::POST, Method::PUT])
        .allow_headers(AllowHeaders::mirror_request())
        .max_age(Duration::from_secs(600));
    Router::new()
        .route("/v1", get(|| async { redirect_307("/v1/health") }).post(|| async { redirect_307("/v1/chat/completions") }))
        .route("/v1/health", get(|| async { json_response(StatusCode::OK, &json!({"status": "OK"})) }))
        .route("/v1/models", get(|| async { json_response(StatusCode::OK, &models_body(unix_now())) }))
        .route("/v1/chat/completions", post(chat_completions))
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .with_state(ch.clone())
        // the auth middleware was added last in python, so it is the outermost layer
        .layer(cors)
        .layer(middleware::from_fn_with_state(ch, auth_middleware))
}

#[async_trait::async_trait]
impl ChannelBehavior for ApiBridge {
    async fn on_ready(&self, ch: &Arc<Channel>) -> anyhow::Result<()> {
        *self.host.lock() = crate::util::resolve_host(&ch.config);
        *self.port.lock() = crate::util::resolve_port(&ch.config, 8000);
        Ok(())
    }

    async fn run(&self, ch: &Arc<Channel>) -> anyhow::Result<()> {
        let host = self.host.lock().clone();
        let port = *self.port.lock();
        let listener = match tokio::net::TcpListener::bind((host.as_str(), port)).await {
            Ok(l) => l,
            Err(e) => {
                ch.log("api bridge", &format!("Error while starting API bridge: {}", ol_core::detail_error(&e)));
                return Ok(());
            }
        };
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        *self.shutdown_tx.lock() = Some(tx);
        ch.log("api bridge", &format!("The API bridge is up and running on {}:{}", host, port));
        self.running.store(true, std::sync::atomic::Ordering::SeqCst);
        let res = axum::serve(listener, router(ch.clone()))
            .with_graceful_shutdown(async move {
                let _ = rx.await;
            })
            .await;
        self.running.store(false, std::sync::atomic::Ordering::SeqCst);
        self.stopped.notify_waiters();
        if let Err(e) = res {
            ch.log("api bridge", &format!("Error while starting API bridge: {}", ol_core::detail_error(&e)));
        }
        Ok(())
    }

    async fn on_shutdown(&self, ch: &Arc<Channel>) -> anyhow::Result<()> {
        let tx = self.shutdown_tx.lock().take();
        if let Some(tx) = tx {
            let stopped = self.stopped.notified();
            let _ = tx.send(());
            if self.running.load(std::sync::atomic::Ordering::SeqCst)
                && tokio::time::timeout(Duration::from_secs(5), stopped).await.is_err()
            {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
        ch.log("api bridge", "API bridge server shut down successfully.");
        Ok(())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_is_python_json() {
        let s = openai_chunk("chatcmpl-x", 5, &json!("openlumara"), &json!("héllo"));
        assert_eq!(
            s,
            r#"{"id": "chatcmpl-x", "object": "chat.completion.chunk", "created": 5, "model": "openlumara", "choices": [{"index": 0, "delta": {"content": "h\u00e9llo"}, "finish_reason": null}]}"#
        );
    }

    #[test]
    fn completion_shape() {
        let b = completion_body("id", 1, &json!("m"), &json!("hi"));
        assert_eq!(b["choices"][0]["message"], json!({"role": "assistant", "content": "hi"}));
        assert_eq!(b["usage"]["total_tokens"], 0);
        assert_eq!(serde_json::to_string(&models_body(3)).unwrap(), r#"{"object":"list","data":[{"id":"openlumara","object":"model","created":3,"owned_by":"openlumara"}]}"#);
    }
}
