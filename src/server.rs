use crate::{
    generate::{self, GenerateRequest},
    graph,
    models::*,
    money::{self, LedgerRow},
    pipeline::{self, PipelineConfig},
    publish::{self, PublishRequest},
    render,
    store::Store,
    trends,
};
use anyhow::{bail, Context, Result};
use axum::{
    extract::{DefaultBodyLimit, Multipart, Path, Query, State},
    http::{HeaderMap, Method, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post, put},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::AsyncWriteExt,
    sync::{RwLock, Semaphore},
};
use tower_http::services::ServeDir;

#[derive(Clone)]
pub struct AppState {
    pub store: Store,
    render_slots: Arc<Semaphore>,
    pipeline_slots: Arc<Semaphore>,
    pipeline: Arc<RwLock<PipelineState>>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PipelineSettings {
    enabled: bool,
    config: PipelineConfig,
}
#[derive(Clone, Serialize)]
struct PipelineState {
    enabled: bool,
    status: String,
    config: Option<PipelineConfig>,
    active_config: Option<PipelineConfig>,
    last_report: Option<Value>,
    last_error: Option<String>,
    last_started_at: Option<String>,
    last_finished_at: Option<String>,
    next_run_at: Option<String>,
}
impl AppState {
    fn new(store: Store) -> Self {
        Self {
            store,
            render_slots: Arc::new(Semaphore::new(1)),
            pipeline_slots: Arc::new(Semaphore::new(1)),
            pipeline: Arc::new(RwLock::new(PipelineState {
                enabled: false,
                status: "off".into(),
                config: None,
                active_config: None,
                last_report: None,
                last_error: None,
                last_started_at: None,
                last_finished_at: None,
                next_run_at: None,
            })),
        }
    }
}
pub struct ApiError(anyhow::Error);
impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        Self(e.into())
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":format!("{:#}",self.0)})),
        )
            .into_response()
    }
}
type ApiResult<T> = std::result::Result<T, ApiError>;
macro_rules! api_bail {($($arg:tt)*)=>{return Err(ApiError(anyhow::anyhow!($($arg)*)))}}

pub fn router(store: Store) -> Router {
    router_with_state(AppState::new(store))
}
fn router_with_state(state: AppState) -> Router {
    let store = state.store.clone();
    Router::new()
        .route(
            "/",
            get(|| async { Html(include_str!("../web/index.html")) }),
        )
        .route(
            "/app.css",
            get(|| async {
                (
                    [(axum::http::header::CONTENT_TYPE, "text/css; charset=utf-8")],
                    include_str!("../web/app.css"),
                )
            }),
        )
        .route(
            "/app.js",
            get(|| async {
                (
                    [(
                        axum::http::header::CONTENT_TYPE,
                        "text/javascript; charset=utf-8",
                    )],
                    include_str!("../web/app.js"),
                )
            }),
        )
        .route("/api/state", get(state_handler))
        .route(
            "/api/example",
            get(|| async { Json(crate::preferred_example()) }),
        )
        .route("/api/trends/refresh", post(refresh_trends))
        .route("/api/draft", post(draft))
        .route("/api/pipeline/run", post(run_pipeline))
        .route("/api/pipeline/config", post(configure_pipeline))
        .route("/api/projects", post(create_project))
        .route("/api/projects/{id}", put(update_project))
        .route("/api/projects/{id}/render", post(start_render))
        .route("/api/projects/{id}/publish", post(queue_publish))
        .route("/api/projects/{id}/analytics", post(analytics))
        .route("/api/projects/{id}/hypothesis", post(hypothesis))
        .route("/api/projects/{id}/snapshot", get(snapshot))
        .route("/api/jobs/{id}/cancel", post(cancel_job))
        .route("/api/assets", post(upload_asset))
        .route("/api/ledger", post(save_ledger))
        .route("/api/ledger/import", post(import_ledger))
        .route("/api/graph", get(graph_handler))
        .nest_service("/media", ServeDir::new(store.root.join("renders")))
        .nest_service("/exports", ServeDir::new(store.root.join("exports")))
        .layer(DefaultBodyLimit::max(100 * 1024 * 1024))
        .layer(middleware::from_fn(local_origin))
        .with_state(state)
}

/// The app has no public listener. Reject drive-by writes from unrelated browser origins.
async fn local_origin(headers: HeaderMap, req: axum::extract::Request, next: Next) -> Response {
    if !matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS) {
        if let Some(origin) = headers.get("origin") {
            let origin = origin.to_str().unwrap_or("");
            let host = headers
                .get("host")
                .and_then(|h| h.to_str().ok())
                .unwrap_or("");
            let allowed = format!("http://{host}");
            if origin != allowed
                || !(host.starts_with("127.0.0.1:") || host.starts_with("localhost:"))
            {
                return (
                    StatusCode::FORBIDDEN,
                    Json(json!({"error":"Запрос должен идти из локального интерфейса студии"})),
                )
                    .into_response();
            }
        }
    }
    next.run(req).await
}
pub async fn serve(store: Store, port: u16) -> Result<()> {
    let state = AppState::new(store.clone());
    let settings_path = store.root.join("autopilot.json");
    if settings_path.is_file() {
        let settings: PipelineSettings = serde_json::from_slice(&std::fs::read(settings_path)?)
            .context("Некорректные настройки автоконвейера")?;
        settings.config.validate()?;
        let mut p = state.pipeline.write().await;
        p.enabled = settings.enabled;
        p.config = Some(settings.config);
        p.status = if p.enabled { "idle" } else { "off" }.into();
    }
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?;
    println!("RustClip Studio: http://127.0.0.1:{port}");
    let worker_store = store.clone();
    let worker = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        loop {
            interval.tick().await;
            let jobs = match worker_store.jobs() {
                Ok(j) => j,
                Err(e) => {
                    tracing::error!("Чтение очереди: {e}");
                    continue;
                }
            };
            for j in jobs.iter().rev() {
                if j.kind != "publish" || !["queued", "scheduled"].contains(&j.state.as_str()) {
                    continue;
                }
                if j.due_at.as_ref().is_some_and(|t| {
                    chrono::DateTime::parse_from_rfc3339(t)
                        .map(|d| d > chrono::Utc::now())
                        .unwrap_or(true)
                }) {
                    continue;
                }
                if let Err(e) = execute_publish(&worker_store, &j.id).await {
                    tracing::error!("Сохранение результата очереди: {e}");
                }
            }
        }
    });
    let pipeline_state = state.clone();
    let conveyor = tokio::spawn(async move {
        let mut timer = tokio::time::interval(Duration::from_secs(5));
        loop {
            timer.tick().await;
            if let Err(e) = launch_cycle(&pipeline_state, None).await {
                tracing::error!("Автоконвейер не запущен: {e}");
            }
        }
    });
    axum::serve(listener, router_with_state(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    worker.abort();
    conveyor.abort();
    Ok(())
}
async fn state_handler(State(s): State<AppState>) -> ApiResult<Json<Value>> {
    let rows = s.store.ledger()?;
    Ok(Json(
        json!({"projects":s.store.projects()?,"trends":s.store.trends()?,"jobs":s.store.jobs()?,"ledger":rows,"totals":money::totals(&rows)?,"graph_integrity":graph::verify(&s.store.events()?),"pipeline":s.pipeline.read().await.clone(),"capabilities":{"renderer":render::capabilities(),"publish":publish::capabilities(),"ollama_configured":std::env::var("OLLAMA_MODEL").is_ok(),"version":env!("CARGO_PKG_VERSION")}}),
    ))
}
async fn run_pipeline(
    State(s): State<AppState>,
    Json(config): Json<PipelineConfig>,
) -> ApiResult<Json<Value>> {
    Ok(Json(launch_cycle(&s, Some(config)).await?))
}
async fn configure_pipeline(
    State(s): State<AppState>,
    Json(settings): Json<PipelineSettings>,
) -> ApiResult<Json<PipelineState>> {
    settings.config.validate()?;
    let mut p = s.pipeline.write().await;
    s.store.record_intent(
        "autopilot:policy",
        "Оператор изменил период, площадку и бюджет автоконвейера",
        serde_json::to_value(&settings)?,
    )?;
    let temp = s.store.root.join("autopilot.json.tmp");
    tokio::fs::write(&temp, serde_json::to_vec_pretty(&settings)?).await?;
    tokio::fs::rename(temp, s.store.root.join("autopilot.json")).await?;
    p.enabled = settings.enabled;
    p.config = Some(settings.config);
    p.next_run_at = None;
    if p.status != "running" {
        p.status = if p.enabled { "idle" } else { "off" }.into();
    }
    Ok(Json(p.clone()))
}
async fn launch_cycle(s: &AppState, requested: Option<PipelineConfig>) -> Result<Value> {
    // Automatic admission and policy changes share the same lock. Never launch a
    // stale enabled/config snapshot after the operator has disabled or changed it.
    let mut p = s.pipeline.write().await;
    let config = match requested {
        Some(config) => config,
        None => {
            let due = p
                .next_run_at
                .as_ref()
                .map(|d| {
                    chrono::DateTime::parse_from_rfc3339(d).is_ok_and(|d| d <= chrono::Utc::now())
                })
                .unwrap_or(true);
            if !p.enabled || p.status == "running" || !due {
                return Ok(json!({"accepted":false}));
            }
            p.config
                .clone()
                .context("Отсутствует политика автоконвейера")?
        }
    };
    config.validate()?;
    let slot = s
        .pipeline_slots
        .clone()
        .try_acquire_owned()
        .context("Автоконвейер уже выполняет цикл")?;
    let run_id = id();
    let state = s.clone();
    p.status = "running".into();
    p.last_error = None;
    p.last_started_at = Some(now());
    p.active_config = Some(config.clone());
    if p.config.is_none() {
        p.config = Some(config.clone());
    }
    drop(p);
    tokio::spawn(async move {
        let _slot = slot;
        let report = async {
            let _cpu = state.render_slots.acquire().await?;
            pipeline::cycle(&state.store, &config).await
        }
        .await;
        let mut p = state.pipeline.write().await;
        match report {
            Ok(report) => {
                p.last_report = Some(report);
                p.last_error = None;
            }
            Err(e) => {
                p.last_error = Some(format!("{e:#}"));
                p.last_report = None;
                p.enabled = false;
                // Top-level failures stop the automatic schedule, including on
                // restart. Per-video failures are a successful structured report.
                if let Some(config) = p.config.clone() {
                    let stopped = PipelineSettings {
                        enabled: false,
                        config,
                    };
                    let saved = async {
                        let temp = state.store.root.join("autopilot.json.tmp");
                        tokio::fs::write(&temp, serde_json::to_vec_pretty(&stopped)?).await?;
                        tokio::fs::rename(temp, state.store.root.join("autopilot.json")).await?;
                        Ok::<_, anyhow::Error>(())
                    }
                    .await;
                    if let Err(e) = saved {
                        p.last_error = Some(format!(
                            "{}; не удалось сохранить остановку: {e}",
                            p.last_error.as_deref().unwrap_or("")
                        ));
                    }
                }
            }
        }
        p.last_finished_at = Some(now());
        p.status = if p.enabled { "idle" } else { "off" }.into();
        p.next_run_at = if p.enabled {
            let minutes = p.config.as_ref().map(|c| c.interval_minutes).unwrap_or(60);
            Some(
                (chrono::Utc::now() + chrono::Duration::minutes(minutes as i64))
                    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            )
        } else {
            None
        };
    });
    Ok(json!({"accepted":true,"run_id":run_id}))
}
#[derive(Deserialize)]
struct TrendRequest {
    source: String,
    region: String,
}
async fn refresh_trends(
    State(s): State<AppState>,
    Json(r): Json<TrendRequest>,
) -> ApiResult<Json<Value>> {
    let data = trends::fetch(&r.source, &r.region).await?;
    s.store.save_trends(&data)?;
    Ok(Json(json!({"trends":data})))
}
async fn draft(
    State(s): State<AppState>,
    Json(r): Json<GenerateRequest>,
) -> ApiResult<Json<Value>> {
    let (spec, generator) = generate::draft(r).await?;
    let p = s.store.create_project(spec, &generator)?;
    Ok(Json(json!({"project":p,"generator":generator})))
}
async fn create_project(
    State(s): State<AppState>,
    Json(spec): Json<ProjectSpec>,
) -> ApiResult<Json<Project>> {
    Ok(Json(s.store.create_project(spec, "manual")?))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Update {
    expected_revision: u32,
    spec: ProjectSpec,
}
async fn update_project(
    State(s): State<AppState>,
    Path(pid): Path<String>,
    Json(r): Json<Update>,
) -> ApiResult<Json<Project>> {
    Ok(Json(s.store.update_project(
        &pid,
        r.expected_revision,
        r.spec,
    )?))
}
async fn start_render(State(s): State<AppState>, Path(pid): Path<String>) -> ApiResult<Json<Job>> {
    let (j, start) = s.store.start_render(&pid)?;
    if start {
        let job = j.clone();
        tokio::spawn(async move {
            let permit = s.render_slots.acquire().await;
            if permit.is_err() {
                return;
            }
            let result = async {
                s.store.begin_job(&job.id)?;
                render::render(&s.store.project(&job.project_id)?, &s.store.root).await
            }
            .await;
            if let Err(e) = s.store.finish_render(&job.id, result) {
                tracing::error!("Сохранение рендера: {e}");
            }
        });
    }
    Ok(Json(j))
}
async fn queue_publish(
    State(s): State<AppState>,
    Path(pid): Path<String>,
    Json(r): Json<PublishRequest>,
) -> ApiResult<Json<Job>> {
    r.validate()?;
    let due = match &r.scheduled_at {
        Some(d) if !r.dry_run && chrono::DateTime::parse_from_rfc3339(d)? > chrono::Utc::now() => {
            Some(
                chrono::DateTime::parse_from_rfc3339(d)?
                    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            )
        }
        _ => None,
    };
    Ok(Json(s.store.queue_publish(
        &pid,
        serde_json::to_value(r)?,
        due,
    )?))
}
pub async fn execute_publish(store: &Store, jid: &str) -> Result<()> {
    let current = store.job(jid)?;
    let integrity = graph::verify(&store.events()?);
    if !integrity.valid {
        store.finish_job(
            jid,
            "failed",
            None,
            Some("Причинный журнал повреждён; внешнее действие заблокировано".into()),
        )?;
        return Ok(());
    }
    let j = match store.begin_job(jid) {
        Ok(j) => j,
        Err(e) => {
            // An independent process can win the transaction. Do not rewrite its running job.
            let actual = store.job(jid)?;
            if actual.state == current.state
                && ["queued", "scheduled"].contains(&actual.state.as_str())
            {
                store.finish_job(jid, "failed", None, Some(format!("{e:#}")))?;
            }
            return Ok(());
        }
    };
    let result = async {
        let req: PublishRequest = serde_json::from_value(j.request["publish"].clone())?;
        let p = store.project(&j.project_id)?;
        publish::publish(&p, &store.root, &req).await
    }
    .await;
    match result {
        Ok(value) => {
            let state = if value["kind"] == "simulated" {
                "dry_run"
            } else {
                "succeeded"
            };
            store.finish_job(jid, state, Some(value), None)?;
        }
        Err(e) => {
            let error = format!("{e:#}");
            let state = if error.starts_with("UNKNOWN_OUTCOME:") {
                "unknown"
            } else {
                "failed"
            };
            store.finish_job(jid, state, None, Some(error))?;
        }
    }
    Ok(())
}
async fn cancel_job(State(s): State<AppState>, Path(jid): Path<String>) -> ApiResult<Json<Job>> {
    let j = s.store.job(&jid)?;
    if j.kind != "publish" {
        api_bail!("Остановка рендера в первой версии не поддерживается");
    }
    if !["queued", "scheduled"].contains(&j.state.as_str()) {
        api_bail!("Можно отменить только ещё не запущенное задание");
    }
    Ok(Json(s.store.finish_job(&jid, "cancelled", None, None)?))
}

async fn upload_asset(State(s): State<AppState>, mut data: Multipart) -> ApiResult<Json<Value>> {
    while let Some(mut field) = data.next_field().await? {
        if field.name() != Some("file") {
            continue;
        }
        let name = field.file_name().unwrap_or("asset").to_string();
        let ext = std::path::Path::new(&name)
            .extension()
            .and_then(|x| x.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if ![
            "png", "jpg", "jpeg", "webp", "mp4", "mov", "mkv", "webm", "wav", "mp3", "ogg", "flac",
            "m4a",
        ]
        .contains(&ext.as_str())
        {
            api_bail!("Загрузите изображение, видео или аудио");
        }
        let filename = format!("{}.{}", id(), ext);
        let tmp = s.store.root.join("assets").join(format!("{filename}.part"));
        let mut file = tokio::fs::File::create(&tmp).await?;
        let mut size = 0usize;
        let result: Result<()> = async {
            while let Some(bytes) = field.chunk().await? {
                size += bytes.len();
                if size > 95 * 1024 * 1024 {
                    bail!("Материал ограничен 95 MB");
                }
                file.write_all(&bytes).await?;
            }
            file.flush().await?;
            if size == 0 {
                bail!("Файл пуст");
            }
            Ok(())
        }
        .await;
        drop(file);
        if let Err(e) = result {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(e.into());
        }
        tokio::fs::rename(&tmp, s.store.root.join("assets").join(&filename)).await?;
        return Ok(Json(
            json!({"asset":format!("assets/{filename}"),"name":std::path::Path::new(&name).file_name().and_then(|p|p.to_str()).unwrap_or("asset"),"size":size}),
        ));
    }
    api_bail!("Нужно поле file");
}
async fn save_ledger(
    State(s): State<AppState>,
    Json(row): Json<LedgerRow>,
) -> ApiResult<Json<Value>> {
    let count = s.store.save_ledger(&[row])?;
    Ok(Json(json!({"rows_imported":count})))
}
async fn import_ledger(State(s): State<AppState>, mut data: Multipart) -> ApiResult<Json<Value>> {
    while let Some(mut field) = data.next_field().await? {
        if field.name() != Some("file") {
            continue;
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = field.chunk().await? {
            if bytes.len() + chunk.len() > 2 * 1024 * 1024 {
                api_bail!("CSV ограничен 2 MB");
            }
            bytes.extend_from_slice(&chunk);
        }
        let rows = money::parse_csv(&bytes)?;
        let count = s.store.save_ledger(&rows)?;
        return Ok(Json(json!({"rows_imported":count})));
    }
    api_bail!("Нужно поле file");
}
#[derive(Deserialize)]
struct AnalyticsRequest {
    start: String,
    end: String,
    currency: String,
    #[serde(default)]
    include_revenue: bool,
}
async fn analytics(
    State(s): State<AppState>,
    Path(pid): Path<String>,
    Json(r): Json<AnalyticsRequest>,
) -> ApiResult<Json<Value>> {
    s.store.project(&pid)?;
    let jobs = s.store.jobs()?;
    let video_ids: BTreeSet<String> = jobs
        .iter()
        .filter(|j| {
            j.project_id == pid
                && j.kind == "publish"
                && j.state == "succeeded"
                && j.result
                    .as_ref()
                    .is_some_and(|r| r["platform"] == "youtube")
        })
        .filter_map(|j| {
            j.result
                .as_ref()
                .and_then(|v| v["receipt"]["id"].as_str())
                .map(str::to_string)
        })
        .collect();
    if video_ids.is_empty() {
        api_bail!("Нужен подтверждённый ID ролика после публикации в YouTube");
    }
    if video_ids.len() > 50 {
        api_bail!("В одном проекте аналитика ограничена 50 опубликованными роликами");
    }
    // All published revisions share a project ledger: aggregate every unique remote video first.
    // A failed API call leaves the existing ledger untouched.
    let mut batches = Vec::new();
    for video_id in &video_ids {
        batches.push(
            publish::youtube_analytics(video_id, &r.start, &r.end, &r.currency, r.include_revenue)
                .await?,
        );
    }
    let raw = aggregate_analytics(&batches)?;
    let existing = s.store.ledger()?;
    let mut rows = Vec::new();
    for v in raw {
        let date = v["date"]
            .as_str()
            .context("API не вернул дату")?
            .to_string();
        let old = existing.iter().find(|row| {
            row.project_id == pid
                && row.platform == "youtube"
                && row.date == date
                && row.currency == r.currency
        });
        rows.push(LedgerRow {
            project_id: pid.clone(),
            platform: "youtube".into(),
            date,
            views: v["views"].as_u64().context("API не вернул просмотры")?,
            currency: r.currency.clone(),
            rpm_minor: old.and_then(|r| r.rpm_minor),
            monetized: old.map(|r| r.monetized).unwrap_or(r.include_revenue),
            actual_revenue_minor: old.and_then(|r| r.actual_revenue_minor),
            api_estimated_revenue_minor: if r.include_revenue {
                v["api_estimated_revenue_minor"].as_i64()
            } else {
                old.and_then(|r| r.api_estimated_revenue_minor)
            },
            cost_minor: old.map(|r| r.cost_minor).unwrap_or(0),
            eligible_bps: old.map(|r| r.eligible_bps).unwrap_or(10000),
            source: "youtube_analytics".into(),
        });
    }
    let count = s.store.save_ledger(&rows)?;
    Ok(Json(
        json!({"rows_imported":count,"videos_aggregated":video_ids.len()}),
    ))
}

fn aggregate_analytics(batches: &[Vec<Value>]) -> Result<Vec<Value>> {
    let mut days: BTreeMap<String, (u64, i64, bool)> = BTreeMap::new();
    for batch in batches {
        for row in batch {
            let date = row["date"]
                .as_str()
                .context("API не вернул дату")?
                .to_owned();
            let views = row["views"].as_u64().context("API не вернул просмотры")?;
            let total = days.entry(date).or_insert((0, 0, true));
            total.0 = total
                .0
                .checked_add(views)
                .context("Переполнение просмотров")?;
            if let Some(revenue) = row["api_estimated_revenue_minor"].as_i64() {
                total.1 = total
                    .1
                    .checked_add(revenue)
                    .context("Переполнение дохода")?;
            } else {
                total.2 = false;
            }
        }
    }
    Ok(days.into_iter().map(|(date,(views,income,complete))|json!({"date":date,"views":views,"api_estimated_revenue_minor":if complete{Some(income)}else{None}})).collect())
}
#[derive(Deserialize)]
struct GraphQuery {
    project_id: Option<String>,
    as_of: Option<String>,
}
async fn graph_handler(
    State(s): State<AppState>,
    Query(r): Query<GraphQuery>,
) -> ApiResult<Json<Value>> {
    Ok(Json(graph::view(
        &s.store.events()?,
        r.project_id.as_deref(),
        r.as_of.as_deref(),
    )?))
}
async fn snapshot(
    State(s): State<AppState>,
    Path(pid): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    Ok(Json(
        json!({"project":graph::snapshot(&s.store.events()?,&pid,q.get("as_of").map(String::as_str))?,"replay":"read_only_no_side_effects"}),
    ))
}
#[derive(Deserialize)]
struct Hypothesis {
    reason: String,
}
async fn hypothesis(
    State(s): State<AppState>,
    Path(pid): Path<String>,
    Json(r): Json<Hypothesis>,
) -> ApiResult<Json<CausalEvent>> {
    Ok(Json(s.store.hypothesis(&pid, &r.reason)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::example;
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    #[tokio::test]
    async fn automatic_admission_obeys_current_disabled_and_future_policy() {
        let tmp = tempfile::tempdir().unwrap();
        let state = AppState::new(Store::open(tmp.path()).unwrap());
        {
            let mut p = state.pipeline.write().await;
            p.config = Some(PipelineConfig::default());
        }
        assert_eq!(launch_cycle(&state, None).await.unwrap()["accepted"], false);
        {
            let mut p = state.pipeline.write().await;
            p.enabled = true;
            p.next_run_at = Some((chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339());
        }
        assert_eq!(launch_cycle(&state, None).await.unwrap()["accepted"], false);
        assert!(state.store.events().unwrap().is_empty());
        assert!(state.pipeline.read().await.active_config.is_none());
    }
    #[tokio::test]
    async fn corrupt_projection_stops_schedule_and_keeps_actual_cycle_policy() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let project = store.create_project(example(), "manual").unwrap();
        let db = rusqlite::Connection::open(tmp.path().join("studio.sqlite3")).unwrap();
        db.execute(
            "UPDATE projects SET json=json_set(json,'$.revision',42) WHERE id=?1",
            [project.id],
        )
        .unwrap();
        let state = AppState::new(store);
        let scheduled = PipelineConfig::default();
        {
            let mut p = state.pipeline.write().await;
            p.enabled = true;
            p.config = Some(scheduled.clone());
        }
        let mut actual = scheduled;
        actual.angle = "Actual one-cycle intent".into();
        assert_eq!(
            launch_cycle(&state, Some(actual)).await.unwrap()["accepted"],
            true
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if state.pipeline.read().await.status != "running" {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let p = state.pipeline.read().await;
        assert!(!p.enabled);
        assert!(p.next_run_at.is_none());
        assert!(p.last_error.is_some());
        assert_eq!(
            p.active_config.as_ref().unwrap().angle,
            "Actual one-cycle intent"
        );
        assert_ne!(p.config.as_ref().unwrap().angle, "Actual one-cycle intent");
        let saved: PipelineSettings =
            serde_json::from_slice(&std::fs::read(tmp.path().join("autopilot.json")).unwrap())
                .unwrap();
        assert!(!saved.enabled);
    }
    #[test]
    fn analytics_aggregates_all_published_revisions_before_upsert() {
        let a = vec![json!({"date":"2026-10-03","views":1000,"api_estimated_revenue_minor":1200})];
        let b = vec![json!({"date":"2026-10-03","views":700,"api_estimated_revenue_minor":400})];
        let rows = aggregate_analytics(&[a, b]).unwrap();
        assert_eq!(rows[0]["views"], 1700);
        assert_eq!(rows[0]["api_estimated_revenue_minor"], 1600);
        let rows = aggregate_analytics(&[
            vec![json!({"date":"2026-10-03","views":1,"api_estimated_revenue_minor":null})],
            vec![json!({"date":"2026-10-03","views":1,"api_estimated_revenue_minor":50})],
        ])
        .unwrap();
        assert!(rows[0]["api_estimated_revenue_minor"].is_null());
    }
    #[tokio::test]
    async fn cross_origin_write_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let app = router(Store::open(tmp.path()).unwrap());
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/projects")
                    .header("origin", "https://unrelated.example")
                    .header("host", "127.0.0.1:3939")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
    #[tokio::test]
    async fn api_project_revision_and_graph_are_connected() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let app = router(store.clone());
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/projects")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_string(&example()).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let p: Project = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(p.revision, 1);
        assert_eq!(store.events().unwrap().len(), 1);
        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/graph?project_id={}", p.id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}
