use super::auth::{self, Auth};
use axum::{
    Json, Router,
    extract::{
        ConnectInfo, DefaultBodyLimit, Path, Query, Request, State, connect_info::Connected,
    },
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, patch, post, put},
    serve::IncomingStream,
};
use ccpd::{
    links,
    native::Build,
    paths,
    protocol::RoutingMode,
    routing::{self, Failure, Observation},
    store::{Page, Store, config, configs, routing_mode},
    types::{Error, Result, tool},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc};
use tokio::net::UnixListener;

pub type Shared = Arc<App>;
pub struct App {
    pub store: Store,
    pub claude: Build,
    pub codex: Build,
    pub agy: Build,
    pub state_dir: PathBuf,
    pub auth: Auth,
    pub quotas: super::quotas::Jobs,
    pub logins: super::device_login::Logins,
}
impl App {
    pub fn build(&self, t: &str) -> &Build {
        if t == "claude" {
            &self.claude
        } else if t == "agy" {
            &self.agy
        } else {
            &self.codex
        }
    }
}
pub async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| Error::unavailable("worker", "Router worker unavailable."))?
}
#[derive(Clone)]
pub struct Peer(Option<u32>);
impl Connected<IncomingStream<'_, UnixListener>> for Peer {
    fn connect_info(s: IncomingStream<'_, UnixListener>) -> Self {
        Self(s.io().peer_cred().ok().map(|p| p.uid()))
    }
}
async fn same_uid(req: Request, next: Next) -> Response {
    if req
        .extensions()
        .get::<ConnectInfo<Peer>>()
        .and_then(|p| p.0.0)
        != Some(paths::uid())
    {
        return Error::denied().into_response();
    }
    next.run(req).await
}
pub fn private(s: Shared) -> Router {
    Router::new()
        .route(
            "/v2/profile-tool",
            post(|State(s): State<Shared>, Json(id): Json<i64>| async move {
                blocking(move || Ok(Json(config(&*s.store.db()?, id)?.tool))).await
            }),
        )
        .route(
            "/v2/launch",
            post(
                |State(s): State<Shared>, Json(r): Json<routing::Request>| async move {
                    let app = s.clone();
                    let tool = r.tool.clone();
                    let admission =
                        blocking(move || routing::admit(&app.store, app.build(&r.tool), &r))
                            .await?;
                    if admission.refresh_quota {
                        tokio::spawn(super::quotas::refresh_provider(s, tool));
                    }
                    admission.plan.map(Json)
                },
            ),
        )
        .route(
            "/v2/observations",
            post(
                |State(s): State<Shared>, Json(o): Json<Observation>| async move {
                    blocking(move || routing::observe(&s.store, &o))
                        .await
                        .map(|_| Json(json!({})))
                },
            ),
        )
        .route(
            "/v2/exec-failure",
            post(
                |State(s): State<Shared>, Json(f): Json<Failure>| async move {
                    blocking(move || routing::exec_failure(&s.store, &f))
                        .await
                        .map(|_| Json(json!({})))
                },
            ),
        )
        .layer(DefaultBodyLimit::max(256 * 1024))
        .layer(middleware::from_fn(same_uid))
        .with_state(s)
}
#[derive(Deserialize)]
pub struct Filter {
    tool: String,
}
pub async fn accounts(State(s): State<Shared>, Query(q): Query<Filter>) -> Result<Json<Value>> {
    blocking(move||{
        let db=s.store.db()?;let build=s.build(tool(&q.tool)?);
        let mut items=Vec::new();
        for c in configs(&db,&q.tool)?{let mut v=c.public();v["readiness"]=build.ready(&c).into();v["quota"]=super::quotas::view(&db,&c,&s.quotas)?;items.push(v);}
        Ok(Json(json!({"items":items,"version":build.version,"supported":build.supported,"compatibility":links::status(&db,&q.tool)?})))
    }).await
}
async fn add(State(s): State<Shared>, Json(v): Json<Value>) -> Result<(StatusCode, Json<Value>)> {
    blocking(move || {
        (if v.get("create") == Some(&Value::Bool(true)) {
            s.store.create_profile(v)
        } else {
            s.store.register(&v)
        })
        .map(|c| (StatusCode::CREATED, Json(c.public())))
    })
    .await
}
async fn edit(
    State(s): State<Shared>,
    Path(id): Path<i64>,
    Json(v): Json<Value>,
) -> Result<Json<Value>> {
    blocking(move || {
        let c = s.store.edit(id, &v)?;
        let mut out = c.public();
        if !c.enabled && links::repair(&s.store, &c.tool).is_err() {
            out["link_warning"] = "Compatibility link needs repair.".into();
        }
        Ok(Json(out))
    })
    .await
}
async fn settings(State(s): State<Shared>) -> Result<Json<Value>> {
    blocking(move || {
        Ok(Json(
            json!({"routing_mode":routing_mode(&*s.store.db()?)?.name()}),
        ))
    })
    .await
}
async fn set_settings(State(s): State<Shared>, Json(v): Json<Value>) -> Result<Json<Value>> {
    let mode = v["routing_mode"]
        .as_str()
        .and_then(RoutingMode::parse)
        .ok_or_else(|| Error::bad("bad_routing_mode", "Expected a supported routing_mode."))?;
    blocking(move || {
        s.store.set_routing_mode(mode)?;
        Ok(Json(json!({"routing_mode": mode.name()})))
    })
    .await
}
#[derive(Deserialize)]
struct Selection {
    config_id: Option<i64>,
}
async fn current(
    State(s): State<Shared>,
    Path(tool): Path<String>,
    Json(v): Json<Selection>,
) -> Result<Json<Value>> {
    blocking(move || {
        s.store.current(&tool, v.config_id)?;
        Ok(Json(
            json!({"link_needs_repair":links::repair(&s.store,&tool).is_err()}),
        ))
    })
    .await
}
#[derive(Deserialize)]
struct LinkPaths {
    root: Option<String>,
    outer: Option<String>,
}
async fn compatibility(
    State(s): State<Shared>,
    Path(t): Path<String>,
    Json(v): Json<LinkPaths>,
) -> Result<Json<Value>> {
    blocking(move || {
        links::configure(
            &s.store,
            &t,
            v.root.as_deref(),
            v.outer.as_deref(),
            &s.state_dir,
            &paths::runtime_dir()?,
        )?;
        Ok(Json(links::status(&*s.store.db()?, &t)?))
    })
    .await
}
pub fn web(s: Shared) -> Router {
    Router::new()
        .route(
            "/",
            get(|| async { axum::response::Html(include_str!("../../../web/index.html")) }),
        )
        .route(
            "/style.css",
            get(|| async {
                (
                    [("content-type", "text/css; charset=utf-8")],
                    include_str!("../../../web/style.css"),
                )
            }),
        )
        .route(
            "/app.js",
            get(|| async {
                (
                    [("content-type", "text/javascript; charset=utf-8")],
                    include_str!("../../../web/app.js"),
                )
            }),
        )
        .route(
            "/icons.svg",
            get(|| async {
                (
                    [("content-type", "image/svg+xml")],
                    include_str!("../../../web/icons.svg"),
                )
            }),
        )
        .route(
            "/LUCIDE-LICENSE.txt",
            get(|| async { include_str!("../../../web/LUCIDE-LICENSE.txt") }),
        )
        .route("/api/v1/setup", post(auth::setup))
        .route("/api/v1/login", post(auth::login))
        .route("/api/v1/password", post(auth::change_password))
        .route("/api/v1/session", get(auth::session))
        .route("/api/v1/logout", post(auth::logout))
        .route("/api/v1/settings", get(settings).put(set_settings))
        .route("/api/v1/accounts", get(accounts).post(add))
        .route("/api/v1/accounts/{id}", patch(edit))
        .route(
            "/api/v1/accounts/{id}/device-login",
            get(super::device_login::status).post(super::device_login::start),
        )
        .route(
            "/api/v1/accounts/{id}/device-login/cancel",
            post(super::device_login::cancel),
        )
        .route(
            "/api/v1/accounts/{id}/quota/refresh",
            post(super::quotas::one),
        )
        .route(
            "/api/v1/accounts/{id}/quota/report",
            post(super::quotas::report),
        )
        .route("/api/v1/quota/refresh", post(super::quotas::bulk))
        .route("/api/v1/current/{tool}", put(current))
        .route("/api/v1/compatibility/{tool}", put(compatibility))
        .route(
            "/api/v1/current/{tool}/repair",
            post(
                |State(s): State<Shared>, Path(t): Path<String>| async move {
                    blocking(move || links::repair(&s.store, &t))
                        .await
                        .map(|_| Json(json!({})))
                },
            ),
        )
        .route(
            "/api/v1/sessions",
            get(
                |State(s): State<Shared>, Query(q): Query<Page>| async move {
                    blocking(move || s.store.page(true, &q)).await.map(Json)
                },
            ),
        )
        .route(
            "/api/v1/startups",
            get(
                |State(s): State<Shared>, Query(q): Query<Page>| async move {
                    blocking(move || s.store.page(false, &q)).await.map(Json)
                },
            ),
        )
        .layer(DefaultBodyLimit::max(64 * 1024))
        .layer(middleware::from_fn_with_state(s.clone(), auth::guard))
        .with_state(s)
}
