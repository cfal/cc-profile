use super::auth::{self, Auth};
use axum::{
    Json, Router,
    extract::{
        ConnectInfo, DefaultBodyLimit, Path, Query, Request, State, connect_info::Connected,
    },
    http::{Method, StatusCode, Uri},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, patch, post, put},
    serve::IncomingStream,
};
use ccpd::{
    native::{Build, Installed},
    paths,
    protocol::{
        ACP_CHECK, ACP_LAUNCH, ACP_LIST, ACP_LISTED, CODEX_BROWSE, CODEX_CHECK, CODEX_LAUNCH,
        CODEX_LISTED, CODEX_OWNER, CodexListed, RoutingMode,
    },
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
    pub claude: Installed,
    pub codex: Installed,
    pub agy: Installed,
    pub state_dir: PathBuf,
    pub auth: Auth,
    pub quotas: super::quotas::Jobs,
    pub logins: super::device_login::Logins,
    pub identity_slots: tokio::sync::Semaphore,
    pub acp_listing: Arc<tokio::sync::Semaphore>,
    pub reset_write: tokio::sync::Mutex<()>,
}
impl App {
    pub async fn build(&self, t: &str) -> Build {
        let installed = if t == "claude" {
            &self.claude
        } else if t == "agy" {
            &self.agy
        } else {
            &self.codex
        };
        installed.current(&self.state_dir).await
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
        .route(ACP_CHECK, post(super::acp_list::check))
        .route(CODEX_CHECK, post(|State(s): State<Shared>, Json(bin): Json<PathBuf>| async move {
            ccpd::native::validate_binary(&s.build("codex").await, Some(&bin))?;
            Ok::<_, Error>(Json(()))
        }))
        .route(CODEX_OWNER, post(|State(s): State<Shared>, Json(r): Json<routing::Request>| async move {
            let build = s.build("codex").await;
            blocking(move || routing::codex_owner(&s.store, &build, &r)).await.map(Json)
        }))
        .route(CODEX_BROWSE, post(|State(s): State<Shared>, Json((r,all)): Json<(routing::Request,bool)>| async move {
            let build = s.build("codex").await;
            blocking(move || routing::codex_browse_plans(&s.store, &build, &r, all)).await.map(Json)
        }))
        .route(CODEX_LISTED, post(|State(s): State<Shared>, Json(list): Json<CodexListed>| async move {
            let build = s.build("codex").await;
            blocking(move || ccpd::listed::codex(&s.store, &build, list)).await.map(Json)
        }))
        .route(ACP_LIST, post(super::acp_list::list))
        .route(ACP_LISTED, post(super::acp_list::listed))
        .route("/v3/agy-acp/check", post(|| async { acp_upgrade() }))
        .route("/v3/agy-acp/list", post(|| async { acp_upgrade() }))
        .route(
            "/v2/profile-tool",
            post(|State(s): State<Shared>, Json(id): Json<i64>| async move {
                blocking(move || Ok(Json(config(&*s.store.db()?, id)?.tool))).await
            }),
        )
        .route(
            "/v2/launch",
            post(|| async {
                (StatusCode::UPGRADE_REQUIRED,
                 Json(json!({"message":"This daemon requires a v3 launcher. Update ccp and ccpd together."})))
            }),
        )
        .route("/v3/launch", post(launch))
        .route(ACP_LAUNCH, post(launch))
        .route(CODEX_LAUNCH, post(launch))
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
fn acp_upgrade() -> Error {
    Error(
        StatusCode::UPGRADE_REQUIRED,
        "acp_version",
        "Managed AgY ACP requires matching ccp and ccpd. Update both and restart ccpd.".into(),
    )
}
async fn launch(
    State(s): State<Shared>,
    uri: Uri,
    Json(r): Json<routing::Request>,
) -> Result<Json<ccpd::protocol::Plan>> {
    if r.managed_agy_acp() != (uri.path() == ACP_LAUNCH) {
        return Err(acp_upgrade());
    }
    if r.managed_codex_stdio() != (uri.path() == CODEX_LAUNCH) {
        return Err(Error(
            StatusCode::UPGRADE_REQUIRED,
            "codex_stdio_version",
            "Managed Codex stdio requires matching ccp and ccpd. Update both and restart ccpd."
                .into(),
        ));
    }
    let app = s.clone();
    let tool = r.tool.clone();
    let build = s.build(&tool).await;
    let admission = blocking(move || routing::admit(&app.store, &build, &r)).await?;
    if admission.refresh_quota {
        tokio::spawn(super::quotas::refresh_provider(s, tool));
    }
    admission.plan.map(Json)
}
#[derive(Deserialize)]
pub struct Filter {
    tool: String,
}
async fn versions(State(s): State<Shared>, method: Method) -> Json<Value> {
    let force = method == Method::POST;
    let (claude, codex, agy) = tokio::join!(
        s.claude.resolve(&s.state_dir, force),
        s.codex.resolve(&s.state_dir, force),
        s.agy.resolve(&s.state_dir, force),
    );
    let report = |b: Build| json!({"version":b.version,"supported":b.supported});
    Json(json!({"claude":report(claude),"codex":report(codex),"agy":report(agy)}))
}
pub async fn accounts(State(s): State<Shared>, Query(q): Query<Filter>) -> Result<Json<Value>> {
    let build = s.build(tool(&q.tool)?).await;
    blocking(move || {
        let db = s.store.db()?;
        let mut items = Vec::new();
        for c in configs(&db, &q.tool)? {
            let mut v = c.public();
            v["readiness"] = build.ready(&c).into();
            v["quota"] = super::quotas::view(&db, &c, &s.quotas)?;
            items.push(v);
        }
        Ok(Json(
            json!({"items":items,"version":build.version,"supported":build.supported}),
        ))
    })
    .await
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
    blocking(move || Ok(Json(s.store.edit(id, &v)?.public()))).await
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
struct DefaultAccountSelection {
    config_id: Option<i64>,
}
async fn set_default_account(
    State(s): State<Shared>,
    Path(tool): Path<String>,
    Json(v): Json<DefaultAccountSelection>,
) -> Result<Json<Value>> {
    blocking(move || {
        s.store.set_default_account(&tool, v.config_id)?;
        Ok(Json(json!({})))
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
        .route("/api/v1/versions", get(versions).post(versions))
        .route("/api/v1/accounts", get(accounts).post(add))
        .route("/api/v1/accounts/{id}", patch(edit))
        .route(
            "/api/v1/accounts/{id}/identity",
            get(super::identity::read).post(super::identity::check_catalog),
        )
        .route(
            "/api/v1/accounts/{id}/resets/list",
            post(super::resets::list),
        )
        .route(
            "/api/v1/accounts/{id}/resets/redeem",
            post(super::resets::redeem),
        )
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
        .route("/api/v1/quota/refresh", post(super::quotas::bulk))
        .route("/api/v1/current/{tool}", put(set_default_account))
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
