use crate::{config, routing::store::Store, state, usage};
use anyhow::{Context, Result};
use axum::{
    body::{Body, Bytes},
    extract::{Request, State as WebState},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};

#[derive(Clone)]
struct App {
    store: Store,
    config: config::Routing,
    accounts: std::path::PathBuf,
    observation: std::path::PathBuf,
    client: reqwest::Client,
    upstream: String,
}
fn error(status: StatusCode, message: impl ToString) -> Response {
    (
        status,
        Json(json!({"type":"error","error":{"type":"api_error","message":message.to_string()}})),
    )
        .into_response()
}
fn client_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .or_else(|| headers.get("x-api-key").and_then(|h| h.to_str().ok()))
}
fn identity(headers: &HeaderMap, body: &Value) -> Option<String> {
    let from_header = headers
        .get("x-claude-code-session-id")
        .and_then(|v| v.to_str().ok());
    let metadata = body
        .get("metadata")
        .and_then(|v| v.get("user_id"))
        .and_then(Value::as_str);
    let parsed = metadata.and_then(|v| serde_json::from_str::<Value>(v).ok());
    let from_body = parsed
        .as_ref()
        .and_then(|v| v.get("session_id"))
        .and_then(Value::as_str);
    from_header
        .or(from_body)
        .and_then(|s| uuid::Uuid::parse_str(s).ok())
        .map(|id| id.to_string())
}
fn strip_headers(headers: &mut HeaderMap) {
    let connection = headers
        .get("connection")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    for name in connection
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        headers.remove(name);
    }
    for name in [
        "host",
        "authorization",
        "x-api-key",
        "connection",
        "proxy-authorization",
        "proxy-authenticate",
        "keep-alive",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
        "content-length",
    ] {
        headers.remove(name);
    }
}
async fn health(WebState(app): WebState<Arc<App>>, headers: HeaderMap) -> Response {
    match client_token(&headers).and_then(|t| app.store.authenticated(t).ok()) {
        Some(()) => (StatusCode::OK, "ok").into_response(),
        None => error(StatusCode::UNAUTHORIZED, "launch this session with cs run"),
    }
}
async fn forward(WebState(app): WebState<Arc<App>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let Some(token) = client_token(&parts.headers).map(String::from) else {
        return error(StatusCode::UNAUTHORIZED, "launch this session with cs run");
    };
    if app.store.authenticated(&token).is_err() {
        return error(
            StatusCode::UNAUTHORIZED,
            "unknown or closed managed session",
        );
    }
    let body = match axum::body::to_bytes(body, 32 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => return error(StatusCode::PAYLOAD_TOO_LARGE, "request exceeds 32 MiB"),
    };
    let value: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return error(StatusCode::BAD_REQUEST, "invalid JSON request"),
    };
    let Some(root) = identity(&parts.headers, &value) else {
        return error(
            StatusCode::BAD_REQUEST,
            "missing Claude conversation ID; update Claude Code",
        );
    };
    let copy = app.clone();
    let request_root = root.clone();
    let assigned = tokio::task::spawn_blocking(move || {
        let accounts = state::State::load_from(&copy.accounts)?;
        let observed = usage::Observation::load_from(&copy.observation)?.unwrap_or_default();
        copy.store.acquire(
            &token,
            &request_root,
            &accounts,
            &observed.accounts,
            &copy.config,
            usage::now(),
        )
    })
    .await;
    let (account, _alias, guard) = match assigned {
        Ok(Ok(a)) => a,
        Ok(Err(e)) => return error(StatusCode::SERVICE_UNAVAILABLE, e),
        Err(_) => return error(StatusCode::INTERNAL_SERVER_ERROR, "assignment failed"),
    };
    let oauth = account.data["claudeAiOauth"]["accessToken"]
        .as_str()
        .unwrap_or_default();
    let mut headers = parts.headers;
    strip_headers(&mut headers);
    let beta = headers
        .get("anthropic-beta")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let beta = if beta.split(',').any(|v| v.trim() == "oauth-2025-04-20") {
        beta.to_string()
    } else if beta.is_empty() {
        "oauth-2025-04-20".into()
    } else {
        format!("{beta},oauth-2025-04-20")
    };
    headers.insert("anthropic-beta", beta.parse().expect("valid beta header"));
    headers.insert("accept-encoding", "identity".parse().unwrap());
    let url = format!(
        "{}{}",
        app.upstream,
        parts
            .uri
            .path_and_query()
            .map(|v| v.as_str())
            .unwrap_or("/v1/messages")
    );
    let sent = app
        .client
        .post(url)
        .headers(headers)
        .bearer_auth(oauth)
        .body(body)
        .send()
        .await;
    let upstream = match sent {
        Ok(r) => r,
        Err(_) => {
            return error(
                StatusCode::BAD_GATEWAY,
                "upstream connection failed; request was not retried",
            )
        }
    };
    let status = upstream.status();
    let mut headers = upstream.headers().clone();
    strip_headers(&mut headers);
    let mut chunks = upstream.bytes_stream();
    let stream = async_stream::stream! {
        let _guard=guard;
        while let Some(chunk)=chunks.next().await {match chunk {Ok(bytes)=>yield Ok::<Bytes,std::io::Error>(bytes),Err(_)=>{yield Err(std::io::Error::other("upstream stream interrupted; not retried"));break;}}}
    };
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}
fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/messages", post(forward))
        .route("/v1/messages/count_tokens", post(forward))
        .with_state(app)
}
fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .retry(reqwest::retry::never())
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}
fn report_usage() -> Result<()> {
    let Some(observation) = usage::Observation::load()? else {
        return Ok(());
    };
    let counts = Store::default_path()?.update(|state| Ok(state.counts()))?;
    let lines = usage::format_lines_with_counts(&observation, usage::now(), false, &counts);
    crate::daemon::log_block("usage", &lines);
    Ok(())
}

pub fn run() -> Result<()> {
    let cfg = config::load()?.routing;
    let _service_lock = state::lock_at(&state::state_path()?.with_file_name("proxy.lock"), false)
        .context("session router is already running")?;
    let store = Store::default_path()?;
    store.reset_requests()?;
    crate::daemon::log("start", "polling account usage before accepting sessions");
    crate::accounts::poll()?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let listener =
            tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, cfg.port)).await?;
        crate::launch::write_endpoint(listener.local_addr()?.port())?;
        crate::daemon::log(
            "start",
            &format!("session router listening on {}", listener.local_addr()?),
        );
        let app = Arc::new(App {
            store,
            config: cfg.clone(),
            accounts: state::state_path()?,
            observation: usage::usage_path()?,
            client: client()?,
            upstream: "https://api.anthropic.com".into(),
        });
        let polling = tokio::spawn(async move {
            let mut last_report = None;
            loop {
                if cfg.report_secs > 0
                    && last_report.is_none_or(|last: std::time::Instant| {
                        last.elapsed() >= Duration::from_secs(cfg.report_secs)
                    })
                {
                    match tokio::task::spawn_blocking(report_usage).await {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => crate::daemon::log("report", &error.to_string()),
                        Err(error) => crate::daemon::log("report", &error.to_string()),
                    }
                    last_report = Some(std::time::Instant::now());
                }
                tokio::time::sleep(Duration::from_secs(cfg.poll_secs)).await;
                if let Ok(Err(e)) = tokio::task::spawn_blocking(crate::accounts::poll).await {
                    crate::daemon::log("poll", &e.to_string());
                }
            }
        });
        let result = axum::serve(listener, router(app))
            .with_graceful_shutdown(async {
                let mut term =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                        .expect("signal handler");
                tokio::select! {_ = tokio::signal::ctrl_c()=>{},_ = term.recv()=>{}}
            })
            .await;
        polling.abort();
        let _ = std::fs::remove_file(crate::launch::endpoint_path()?);
        result?;
        Ok::<_, anyhow::Error>(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        state::{Account, State},
        usage::{Entry, Observation, Reading, Usage, Window},
    };
    use std::{
        collections::BTreeMap,
        sync::atomic::{AtomicUsize, Ordering},
    };
    fn fixture(dir: &std::path::Path, upstream: String) -> (Arc<App>, String) {
        let store = Store {
            path: dir.join("routing.json"),
        };
        let token = store.register().unwrap();
        let accounts = dir.join("state.json");
        let observation = dir.join("usage.json");
        let mut state = State::default();
        state.claude_mut().accounts.insert("a".into(),Account{email:"a@x".into(),data:json!({"claudeAiOauth":{"accessToken":"upstream-secret","expiresAt":(usage::now()+7200)*1000}})});
        std::fs::write(&accounts, serde_json::to_vec(&state).unwrap()).unwrap();
        let obs = Observation {
            accounts: BTreeMap::from([(
                "a".into(),
                Entry::new(
                    Reading::Known(Usage {
                        five_hour: Window {
                            utilization: 0.,
                            resets_at: None,
                        },
                        seven_day: None,
                    }),
                    usage::now(),
                ),
            )]),
            ..Default::default()
        };
        obs.save_to(&observation).unwrap();
        (
            Arc::new(App {
                store,
                config: config::Routing::default(),
                accounts,
                observation,
                client: client().unwrap(),
                upstream,
            }),
            token,
        )
    }
    async fn serve(app: Router) -> (String, tokio::task::JoinHandle<()>) {
        let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", socket.local_addr().unwrap());
        (
            url,
            tokio::spawn(async move {
                axum::serve(socket, app).await.unwrap();
            }),
        )
    }
    #[test]
    fn explicit_header_does_not_require_metadata() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-claude-code-session-id",
            "00000000-0000-4000-8000-000000000001".parse().unwrap(),
        );
        assert_eq!(
            identity(&headers, &json!({"model":"test"})).as_deref(),
            Some("00000000-0000-4000-8000-000000000001")
        );
    }
    #[tokio::test]
    async fn dropping_downstream_releases_assignment_and_upstream_stream() {
        let ended = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let upstream_ended = ended.clone();
        struct End(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for End {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let (upstream, upstream_task) = serve(Router::new().route(
            "/v1/messages",
            post(move || {
                let ended = upstream_ended.clone();
                async move {
                    let stream = async_stream::stream! {
                        let _end=End(ended);
                        yield Ok::<_,std::io::Error>(Bytes::from_static(b"data: first\n\n"));
                        std::future::pending::<()>().await;
                    };
                    Response::new(Body::from_stream(stream))
                }
            }),
        ))
        .await;
        let dir = tempfile::tempdir().unwrap();
        let (app, token) = fixture(dir.path(), upstream);
        let store = app.store.clone();
        let (url, task) = serve(router(app)).await;
        let mut response = client()
            .unwrap()
            .post(format!("{url}/v1/messages"))
            .bearer_auth(token)
            .header(
                "x-claude-code-session-id",
                "00000000-0000-4000-8000-000000000001",
            )
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(response.chunk().await.unwrap().unwrap(), "data: first\n\n");
        drop(response);
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let active = store
                    .update(|s| Ok(s.bindings.values().map(|b| b.in_flight).sum::<usize>()))
                    .unwrap();
                if active == 0 && ended.load(Ordering::SeqCst) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        task.abort();
        upstream_task.abort();
    }

    #[tokio::test]
    async fn forwards_original_body_and_stream_without_local_credentials() {
        let expected=Bytes::from_static(br#"{"metadata":{"user_id":"{\"session_id\":\"00000000-0000-4000-8000-000000000001\"}"},"model":"test","stream":true,"messages":[]}"#);
        let upstream_expected = expected.clone();
        let (upstream, upstream_task) = serve(Router::new().route(
            "/v1/messages",
            post(move |headers: HeaderMap, body: Bytes| {
                let expected = upstream_expected.clone();
                async move {
                    assert_eq!(body, expected);
                    assert_eq!(headers["authorization"], "Bearer upstream-secret");
                    assert!(!headers.contains_key("x-api-key"));
                    assert_eq!(headers["anthropic-beta"], "existing,oauth-2025-04-20");
                    (
                        [("content-type", "text/event-stream"), ("retry-after", "7")],
                        "data: first\n\ndata: second\n\n",
                    )
                }
            }),
        ))
        .await;
        let dir = tempfile::tempdir().unwrap();
        let (app, token) = fixture(dir.path(), upstream);
        let store = app.store.clone();
        let (url, task) = serve(router(app)).await;
        let response = client()
            .unwrap()
            .post(format!("{url}/v1/messages?beta=true"))
            .header("x-api-key", token)
            .header("anthropic-beta", "existing")
            .body(expected)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["retry-after"], "7");
        assert_eq!(
            response.text().await.unwrap(),
            "data: first\n\ndata: second\n\n"
        );
        assert_eq!(
            store.update(|s| Ok(s.counts().get("a").copied())).unwrap(),
            Some(1)
        );
        task.abort();
        upstream_task.abort();
    }
    #[tokio::test]
    async fn rejects_unknown_clients_and_never_retries_upstream_errors() {
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let (upstream, upstream_task) = serve(Router::new().route(
            "/v1/messages",
            post(move || {
                count.fetch_add(1, Ordering::SeqCst);
                async { (StatusCode::TOO_MANY_REQUESTS, "quota exhausted") }
            }),
        ))
        .await;
        let dir = tempfile::tempdir().unwrap();
        let (app, token) = fixture(dir.path(), upstream);
        let (url, task) = serve(router(app)).await;
        let http = client().unwrap();
        assert_eq!(
            http.post(format!("{url}/v1/messages"))
                .body("{}")
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let response=http.post(format!("{url}/v1/messages")).bearer_auth(token).body(json!({"metadata":{"user_id":"{\"session_id\":\"00000000-0000-4000-8000-000000000001\"}"}}).to_string()).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.text().await.unwrap(), "quota exhausted");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        task.abort();
        upstream_task.abort();
    }
}
