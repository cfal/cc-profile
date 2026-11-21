use super::pairing::{self, Pairing};
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
    observer, paths,
    routing::{self, Failure, Observation},
    store::{Page, Store, config, configs, revision},
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
    pub state_dir: PathBuf,
    pub port: u16,
    pub pairing: Pairing,
    pub quotas: super::quotas::Jobs,
    pub workspace: super::workspace::Checks,
}
impl App {
    pub fn build(&self, t: &str) -> &Build {
        if t == "claude" {
            &self.claude
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
            "/v1/launch",
            post(
                |State(s): State<Shared>, Json(r): Json<routing::Request>| async move {
                    let checks = super::workspace::check(&s, &r).await?;
                    blocking(move || {
                        routing::launch_in(&s.store, s.build(&r.tool), &r, &|c| {
                            checks.as_ref().is_none_or(|v| v.ready(c, &s.codex))
                        })
                    })
                    .await
                    .map(Json)
                },
            ),
        )
        .route(
            "/v1/observations",
            post(
                |State(s): State<Shared>, Json(o): Json<Observation>| async move {
                    blocking(move || routing::observe(&s.store, &o))
                        .await
                        .map(|_| Json(json!({})))
                },
            ),
        )
        .route(
            "/v1/exec-failure",
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
        Ok(Json(json!({"items":items,"version":build.version,"supported":build.supported,"observer":observer::spec()?,"compatibility":links::status(&db,&q.tool)?})))
    }).await
}
async fn add(State(s): State<Shared>, Json(v): Json<Value>) -> Result<(StatusCode, Json<Value>)> {
    blocking(move || {
        s.store
            .register(&v)
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
async fn hooks(
    State(s): State<Shared>,
    Path((id, op)): Path<(i64, String)>,
    Json(body): Json<Value>,
) -> Result<Json<Value>> {
    let c = config(&*s.store.db()?, id)?;
    revision(&body, &c)?;
    match op.as_str() {
        "install" => blocking(move || observer::install(&c)).await?,
        "check" => observer::check(&s.store, &s.codex, &c, &s.state_dir).await?,
        _ => return Err(Error::missing("route", "Unknown observer action.")),
    }
    Ok(Json(json!({})))
}
pub fn web(s: Shared) -> Router {
    Router::new()
        .route(
            "/",
            get(|| async { axum::response::Html(include_str!("../web/index.html")) }),
        )
        .route(
            "/style.css",
            get(|| async {
                (
                    [("content-type", "text/css; charset=utf-8")],
                    include_str!("../web/style.css"),
                )
            }),
        )
        .route(
            "/app.js",
            get(|| async {
                (
                    [("content-type", "text/javascript; charset=utf-8")],
                    include_str!("../web/app.js"),
                )
            }),
        )
        .route(
            "/icons.svg",
            get(|| async {
                (
                    [("content-type", "image/svg+xml")],
                    include_str!("../web/icons.svg"),
                )
            }),
        )
        .route(
            "/LUCIDE-LICENSE.txt",
            get(|| async { include_str!("../web/LUCIDE-LICENSE.txt") }),
        )
        .route("/api/v1/login", post(pairing::login))
        .route("/api/v1/session", get(pairing::session))
        .route("/api/v1/logout", post(pairing::logout))
        .route("/api/v1/accounts", get(accounts).post(add))
        .route("/api/v1/accounts/{id}", patch(edit))
        .route("/api/v1/accounts/{id}/observer/{op}", post(hooks))
        .route(
            "/api/v1/accounts/{id}/quota/refresh",
            post(super::quotas::one),
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
        .layer(middleware::from_fn_with_state(s.clone(), pairing::guard))
        .with_state(s)
}
