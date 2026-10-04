//! Publication adapters use official APIs and pre-existing OAuth/bot credentials.
//! This module never obtains credentials, refreshes tokens, or retries an upload.
//! `scheduled_at` belongs to the studio queue, not YouTube's `publishAt`.

use crate::models::Project;
use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, NaiveDate, Utc};
use reqwest::{header, multipart, Client, Response, StatusCode, Url};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::io::AsyncReadExt;
use tokio_util::io::ReaderStream;

const YOUTUBE_UPLOAD: &str = "https://www.googleapis.com/upload/youtube/v3/videos";
const YOUTUBE_ANALYTICS: &str = "https://youtubeanalytics.googleapis.com/v2/reports";
const TELEGRAM_BASE: &str = "https://api.telegram.org";
const TELEGRAM_LIMIT: u64 = 50_000_000;
const MAX_RESPONSE: usize = 2 * 1024 * 1024;

fn default_private() -> String {
    "private".into()
}
fn default_dry_run() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishRequest {
    pub platform: String,
    #[serde(default = "default_private")]
    pub privacy: String,
    #[serde(default = "default_dry_run")]
    pub dry_run: bool,
    #[serde(default)]
    pub scheduled_at: Option<String>,
    #[serde(default)]
    pub made_for_kids: bool,
    #[serde(default)]
    pub contains_synthetic_media: bool,
}

impl PublishRequest {
    pub fn validate(&self) -> Result<()> {
        if !["youtube", "telegram", "export"].contains(&self.platform.as_str()) {
            bail!("Платформа: youtube, telegram или export");
        }
        if !["private", "unlisted", "public"].contains(&self.privacy.as_str()) {
            bail!("Видимость: private, unlisted или public");
        }
        if let Some(value) = &self.scheduled_at {
            DateTime::parse_from_rfc3339(value)
                .map_err(|_| anyhow!("Дата публикации должна быть RFC3339 с часовым поясом"))?;
        }
        Ok(())
    }
}

/// Presence of a token means configured, not authenticated or channel-verified.
pub fn capabilities() -> Value {
    json!({
        "export": {"configured": true, "network": false},
        "youtube": {"configured": has_env("YOUTUBE_ACCESS_TOKEN"), "default_privacy": "private", "requires": "OAuth access token with youtube.upload scope; OAuth setup is manual", "automatic_retries": false},
        "telegram": {"configured": has_env("TELEGRAM_BOT_TOKEN") && has_env("TELEGRAM_CHAT_ID"), "max_bytes": TELEGRAM_LIMIT, "privacy": "chat", "recipient": "TELEGRAM_CHAT_ID environment variable", "automatic_retries": false},
        "youtube_analytics": {"configured": has_env("YOUTUBE_ANALYTICS_ACCESS_TOKEN") || has_env("YOUTUBE_ACCESS_TOKEN"), "requires": "yt-analytics.readonly; revenue additionally needs yt-analytics-monetary.readonly", "revenue_kind": "API estimate, not confirmed payout"}
    })
}

fn has_env(name: &str) -> bool {
    std::env::var(name).is_ok_and(|value| !value.trim().is_empty())
}

fn credential(name: &str) -> Result<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("Не настроена переменная {name}"))
}

fn client() -> Result<Client> {
    Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(300))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| anyhow!("Не удалось создать API-клиент"))
}

fn safe_segment(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        bail!("Некорректный идентификатор проекта или артефакта");
    }
    Ok(())
}

async fn digest(path: &Path) -> Result<String> {
    let mut file = tokio::fs::File::open(path)
        .await
        .context("Не удалось прочитать ролик")?;
    let mut buffer = [0u8; 64 * 1024];
    let mut hasher = Sha256::new();
    loop {
        let len = file
            .read(&mut buffer)
            .await
            .context("Ошибка чтения ролика")?;
        if len == 0 {
            break;
        }
        hasher.update(&buffer[..len]);
    }
    Ok(hex::encode(hasher.finalize()))
}

struct Bundle {
    path: PathBuf,
    video: PathBuf,
    bytes: u64,
    manifest: Value,
}

const CAPTION_LIMIT: u64 = 1024 * 1024;

/// Use the renderer's real timeline: TTS may have lengthened scenes after the
/// nominal spec was written. Only a genuinely missing file uses the fallback.
async fn export_captions(source: &Path, root: &Path, project: &Project) -> Result<(Vec<u8>, bool)> {
    let sibling = source
        .parent()
        .ok_or_else(|| anyhow!("У ролика нет папки субтитров"))?
        .join("captions.srt");
    match tokio::fs::symlink_metadata(&sibling).await {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((scene_srt(project).into_bytes(), false));
        }
        Err(_) => bail!("Не удалось проверить субтитры рендера"),
        Ok(_) => {}
    }
    let canonical = tokio::fs::canonicalize(&sibling)
        .await
        .context("Не удалось проверить путь субтитров рендера")?;
    if !canonical.starts_with(root) {
        bail!("Субтитры должны находиться в папке данных студии");
    }
    let metadata = tokio::fs::metadata(&canonical).await?;
    if !metadata.is_file() || metadata.len() > CAPTION_LIMIT {
        bail!("Субтитры рендера должны быть обычным файлом не более 1 MiB");
    }
    let file = tokio::fs::File::open(&canonical)
        .await
        .context("Не удалось открыть субтитры рендера")?;
    let metadata = file.metadata().await?;
    if !metadata.is_file() || metadata.len() > CAPTION_LIMIT {
        bail!("Субтитры рендера должны быть обычным файлом не более 1 MiB");
    }
    let mut bytes = Vec::new();
    file.take(CAPTION_LIMIT + 1)
        .read_to_end(&mut bytes)
        .await
        .context("Не удалось прочитать субтитры рендера")?;
    if bytes.len() as u64 > CAPTION_LIMIT {
        bail!("Субтитры рендера больше 1 MiB");
    }
    std::str::from_utf8(&bytes).context("Субтитры рендера должны иметь кодировку UTF-8")?;
    Ok((bytes, true))
}

/// Copy into a unique bundle and verify that exact copy before sending bytes.
async fn export_bundle(
    project: &Project,
    data_dir: &Path,
    request: &PublishRequest,
) -> Result<Bundle> {
    let artifact = project
        .artifact
        .as_ref()
        .ok_or_else(|| anyhow!("Сначала отрендерите проект"))?;
    if artifact.revision != project.revision {
        bail!("Ролик устарел: отрендерите текущую ревизию проекта");
    }
    safe_segment(&project.id)?;
    safe_segment(&artifact.id)?;
    if artifact.sha256.len() != 64 || !artifact.sha256.bytes().all(|c| c.is_ascii_hexdigit()) {
        bail!("У артефакта отсутствует корректный SHA256");
    }
    let root = tokio::fs::canonicalize(data_dir)
        .await
        .context("Папка данных не существует")?;
    let source = tokio::fs::canonicalize(root.join(&artifact.path))
        .await
        .context("Ролик не найден")?;
    if !source.starts_with(&root) {
        bail!("Ролик должен находиться в папке данных студии");
    }
    let info = tokio::fs::metadata(&source)
        .await
        .context("Нет доступа к ролику")?;
    if !info.is_file() || info.len() == 0 {
        bail!("Артефакт должен быть непустым файлом");
    }
    if digest(&source).await? != artifact.sha256.to_ascii_lowercase() {
        bail!("SHA256 ролика не совпадает с артефактом");
    }
    let (captions, rendered_captions) = export_captions(&source, &root, project).await?;

    let relative = format!(
        "exports/{}/{}/{}",
        project.id,
        artifact.id,
        uuid::Uuid::new_v4()
    );
    // Check every existing ancestor before creation so an exports symlink cannot
    // create output outside the data root.
    let mut ancestor = root.clone();
    for part in relative.split('/') {
        ancestor.push(part);
        match tokio::fs::symlink_metadata(&ancestor).await {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    bail!("Небезопасная папка экспорта");
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                tokio::fs::create_dir(&ancestor)
                    .await
                    .context("Не удалось создать папку экспорта")?;
            }
            Err(_) => bail!("Не удалось проверить папку экспорта"),
        }
    }
    let path = tokio::fs::canonicalize(&ancestor)
        .await
        .context("Не удалось проверить экспорт")?;
    if !path.starts_with(&root) {
        bail!("Небезопасная папка экспорта");
    }
    let video = path.join("video.mp4");
    tokio::fs::copy(&source, &video)
        .await
        .context("Не удалось скопировать ролик")?;
    if digest(&video).await? != artifact.sha256.to_ascii_lowercase() {
        bail!("Копия ролика изменилась при экспорте");
    }
    let bytes = tokio::fs::metadata(&video).await?.len();
    let caption = format!("{}\n\n{}", project.spec.title, project.spec.description)
        .trim_end()
        .to_string();
    let manifest = json!({
        "bundle": relative,
        "project_id": project.id, "artifact_id": artifact.id,
        "revision": project.revision, "sha256": artifact.sha256.to_ascii_lowercase(),
        "bytes": bytes, "duration_s": artifact.duration_s,
        "title": project.spec.title, "description": project.spec.description,
        "tags": project.spec.tags, "language": project.spec.language,
        "profile": project.spec.profile, "source_urls": project.spec.source_urls,
        "request": request,
        "schedule_semantics": "studio queue due time; no YouTube publishAt",
        "files": {
            "video": format!("/{relative}/video.mp4"),
            "caption": format!("/{relative}/caption.txt"),
            "metadata": format!("/{relative}/metadata.json"),
            "captions": format!("/{relative}/captions.srt")
        },
        "caption_source": if rendered_captions { "renderer" } else { "nominal_fallback" },
        "caption_timing": if rendered_captions {
            "renderer timeline with proportional text chunks, not word-aligned transcription"
        } else {
            "fallback: nominal scene boundaries; renderer captions.srt missing; may not match extended TTS"
        }
    });
    tokio::fs::write(path.join("caption.txt"), caption).await?;
    tokio::fs::write(path.join("captions.srt"), captions).await?;
    tokio::fs::write(
        path.join("metadata.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )
    .await?;
    Ok(Bundle {
        path,
        video,
        bytes,
        manifest,
    })
}

fn scene_srt(project: &Project) -> String {
    let mut result = String::new();
    let mut seconds = 0.0;
    for (index, scene) in project.spec.scenes.iter().enumerate() {
        let end = seconds + scene.duration_s;
        let text = if scene.narration.trim().is_empty() {
            &scene.text
        } else {
            &scene.narration
        };
        result.push_str(&format!(
            "{}\n{} --> {}\n{}\n\n",
            index + 1,
            srt_time(seconds),
            srt_time(end),
            text.replace('\r', "")
        ));
        seconds = end;
    }
    result
}

fn srt_time(seconds: f64) -> String {
    let ms = (seconds.max(0.0) * 1000.0).round() as u64;
    format!(
        "{:02}:{:02}:{:02},{:03}",
        ms / 3_600_000,
        (ms / 60_000) % 60,
        (ms / 1000) % 60,
        ms % 1000
    )
}

pub async fn publish(
    project: &Project,
    data_dir: &Path,
    request: &PublishRequest,
) -> Result<Value> {
    request.validate()?;
    project.spec.validate()?;
    let bundle = export_bundle(project, data_dir, request).await?;
    if request.dry_run || request.platform == "export" {
        return Ok(
            json!({"kind": if request.dry_run {"simulated"} else {"export"}, "platform": request.platform, "external_request_sent": false, "manifest": bundle.manifest}),
        );
    }
    if let Some(date) = &request.scheduled_at {
        let due = DateTime::parse_from_rfc3339(date)?;
        if due > Utc::now() {
            bail!("Время публикации ещё не наступило; используйте очередь студии");
        }
    }
    let api = client()?;
    let receipt = match request.platform.as_str() {
        "youtube" => {
            let token = credential("YOUTUBE_ACCESS_TOKEN")?;
            youtube_upload(
                &api,
                YOUTUBE_UPLOAD,
                &token,
                project,
                &bundle,
                request,
                false,
            )
            .await?
        }
        "telegram" => {
            let token = credential("TELEGRAM_BOT_TOKEN")?;
            let chat = credential("TELEGRAM_CHAT_ID")?;
            telegram_upload(&api, TELEGRAM_BASE, &token, &chat, project, &bundle).await?
        }
        _ => unreachable!("validated platform"),
    };
    // A remote success already occurred. A disk failure must not become a
    // retryable upload failure, so include it as a receipt warning.
    let saved = tokio::fs::write(
        bundle.path.join("receipt.json"),
        serde_json::to_vec_pretty(&receipt)?,
    )
    .await
    .is_ok();
    Ok(
        json!({"kind": "published", "platform": request.platform, "external_request_sent": true, "receipt": receipt, "receipt_saved": saved, "manifest": bundle.manifest}),
    )
}

/// Error text deliberately omits reqwest errors, bodies and session URLs.
async fn response_json(mut response: Response) -> Result<Value> {
    let mut data = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| anyhow!("Не удалось прочитать ответ API"))?
    {
        if data.len() + chunk.len() > MAX_RESPONSE {
            bail!("Ответ API слишком большой");
        }
        data.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&data).map_err(|_| anyhow!("API вернул некорректный JSON"))
}

fn rejected(service: &str, status: StatusCode) -> anyhow::Error {
    anyhow!(
        "{service} отклонил запрос (HTTP {}); проверьте доступы и параметры",
        status.as_u16()
    )
}

fn uncertain(service: &str) -> anyhow::Error {
    anyhow!("UNKNOWN_OUTCOME: {service}: результат отправки неизвестен; проверьте канал перед повторной публикацией")
}

async fn youtube_upload(
    api: &Client,
    upload_url: &str,
    token: &str,
    project: &Project,
    bundle: &Bundle,
    request: &PublishRequest,
    allow_local: bool,
) -> Result<Value> {
    // YouTube's limits are stricter than the generic studio project limits.
    if project.spec.title.contains('<')
        || project.spec.title.contains('>')
        || project.spec.description.contains('<')
        || project.spec.description.contains('>')
    {
        bail!("YouTube: заголовок и описание не могут содержать < или >");
    }
    if project.spec.description.len() > 5000 {
        bail!("YouTube: описание ограничено 5000 байтами");
    }
    let tag_size: usize = project
        .spec
        .tags
        .iter()
        .map(|tag| tag.chars().count() + if tag.contains(' ') { 2 } else { 0 })
        .sum::<usize>()
        + project.spec.tags.len().saturating_sub(1);
    if tag_size > 500 {
        bail!("YouTube: суммарная длина тегов ограничена 500 символами");
    }
    let metadata = json!({
        "snippet": {"title": project.spec.title, "description": project.spec.description, "tags": project.spec.tags, "defaultLanguage": project.spec.language},
        "status": {"privacyStatus": request.privacy, "selfDeclaredMadeForKids": request.made_for_kids, "containsSyntheticMedia": request.contains_synthetic_media}
    });
    let response = api
        .post(upload_url)
        .query(&[("uploadType", "resumable"), ("part", "snippet,status")])
        .bearer_auth(token)
        .header("X-Upload-Content-Type", "video/mp4")
        .header("X-Upload-Content-Length", bundle.bytes)
        .json(&metadata)
        .send()
        .await
        .map_err(|_| {
            anyhow!("YouTube: не удалось начать загрузку; проверьте соединение и OAuth-токен")
        })?;
    if !response.status().is_success() {
        return Err(rejected("YouTube", response.status()));
    }
    let location = response
        .headers()
        .get(header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| anyhow!("YouTube не вернул сессию загрузки"))?;
    let session =
        Url::parse(location).map_err(|_| anyhow!("YouTube вернул некорректную сессию загрузки"))?;
    let trusted = session.scheme() == "https"
        && session
            .host_str()
            .is_some_and(|host| host == "googleapis.com" || host.ends_with(".googleapis.com"))
        && session.username().is_empty()
        && session.password().is_none()
        && session.port_or_known_default() == Some(443);
    let local_test = cfg!(test)
        && allow_local
        && session.scheme() == "http"
        && session.host_str() == Some("127.0.0.1");
    if !trusted && !local_test {
        bail!("YouTube вернул недоверенный адрес сессии загрузки");
    }
    let file = tokio::fs::File::open(&bundle.video)
        .await
        .context("Не удалось открыть экспортированный ролик")?;
    let response = api
        .put(session)
        .bearer_auth(token)
        .header(header::CONTENT_TYPE, "video/mp4")
        .header(header::CONTENT_LENGTH, bundle.bytes)
        .body(reqwest::Body::wrap_stream(ReaderStream::new(file)))
        .send()
        .await
        .map_err(|_| uncertain("YouTube"))?;
    let status = response.status();
    if !status.is_success() {
        if status.is_server_error() || status.as_u16() == 308 {
            return Err(uncertain("YouTube"));
        }
        return Err(rejected("YouTube", status));
    }
    let result = response_json(response)
        .await
        .map_err(|_| uncertain("YouTube"))?;
    let id = result
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| valid_video_id(id))
        .ok_or_else(|| uncertain("YouTube"))?;
    let privacy = result
        .pointer("/status/privacyStatus")
        .and_then(Value::as_str)
        .filter(|value| ["private", "unlisted", "public"].contains(value))
        .ok_or_else(|| uncertain("YouTube"))?;
    let upload_status = result
        .pointer("/status/uploadStatus")
        .and_then(Value::as_str)
        .filter(|value| ["uploaded", "processed", "failed", "rejected", "deleted"].contains(value));
    if matches!(upload_status, Some("failed" | "rejected" | "deleted")) {
        bail!("UNKNOWN_OUTCOME: YouTube создал запись {id}, но сообщил ошибку обработки; проверьте видео перед повторной отправкой");
    }
    Ok(
        json!({"platform": "youtube", "id": id, "url": format!("https://www.youtube.com/watch?v={id}"), "privacy": privacy, "requested_privacy": request.privacy, "status": upload_status.unwrap_or("accepted"), "received_at": crate::models::now(), "processing_complete": upload_status == Some("processed")}),
    )
}

async fn telegram_upload(
    api: &Client,
    base: &str,
    token: &str,
    chat: &str,
    project: &Project,
    bundle: &Bundle,
) -> Result<Value> {
    if bundle.bytes > TELEGRAM_LIMIT {
        bail!("Telegram Bot API: ролик больше 50 MB; уменьшите размер или используйте экспорт");
    }
    if token
        .bytes()
        .any(|c| !c.is_ascii_alphanumeric() && c != b':' && c != b'_' && c != b'-')
    {
        bail!("Некорректный формат TELEGRAM_BOT_TOKEN");
    }
    let file = tokio::fs::File::open(&bundle.video)
        .await
        .context("Не удалось открыть экспортированный ролик")?;
    let video = multipart::Part::stream_with_length(
        reqwest::Body::wrap_stream(ReaderStream::new(file)),
        bundle.bytes,
    )
    .file_name("video.mp4")
    .mime_str("video/mp4")?;
    let caption = telegram_caption(&format!(
        "{}\n\n{}",
        project.spec.title, project.spec.description
    ));
    let form = multipart::Form::new()
        .text("chat_id", chat.to_owned())
        .text("caption", caption)
        .text("supports_streaming", "true")
        .part("video", video);
    let response = api
        .post(format!("{base}/bot{token}/sendVideo"))
        .multipart(form)
        .send()
        .await
        .map_err(|_| uncertain("Telegram"))?;
    if !response.status().is_success() {
        if response.status().is_server_error() {
            return Err(uncertain("Telegram"));
        }
        return Err(rejected("Telegram", response.status()));
    }
    let result = response_json(response)
        .await
        .map_err(|_| uncertain("Telegram"))?;
    if result.get("ok").and_then(Value::as_bool) == Some(false) {
        bail!("Telegram отклонил отправку; проверьте чат, права бота и параметры");
    }
    if result.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(uncertain("Telegram"));
    }
    let id = result
        .pointer("/result/message_id")
        .and_then(Value::as_i64)
        .filter(|id| *id > 0)
        .ok_or_else(|| uncertain("Telegram"))?;
    let chat_id = result
        .pointer("/result/chat/id")
        .and_then(Value::as_i64)
        .ok_or_else(|| uncertain("Telegram"))?;
    let chat_type = result
        .pointer("/result/chat/type")
        .and_then(Value::as_str)
        .filter(|value| ["private", "group", "supergroup", "channel"].contains(value));
    Ok(
        json!({"platform": "telegram", "id": id.to_string(), "message_id": id, "chat_id": chat_id, "privacy": "chat", "chat_type": chat_type, "status": "sent", "received_at": crate::models::now()}),
    )
}

fn telegram_caption(value: &str) -> String {
    // Telegram entity offsets use UTF-16. Counting those units avoids splitting
    // a surrogate pair or exceeding 1024 with emoji-heavy descriptions.
    let mut units = 0;
    value
        .chars()
        .take_while(|character| {
            units += character.len_utf16();
            units <= 1024
        })
        .collect()
}

fn valid_video_id(value: &str) -> bool {
    value.len() == 11
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

fn analytics_dates(start: &str, end: &str) -> Result<(NaiveDate, NaiveDate)> {
    let parse = |value: &str| -> Result<NaiveDate> {
        let date = NaiveDate::parse_from_str(value, "%Y-%m-%d")
            .map_err(|_| anyhow!("Дата аналитики: YYYY-MM-DD"))?;
        if date.format("%Y-%m-%d").to_string() != value {
            bail!("Дата аналитики: YYYY-MM-DD");
        }
        Ok(date)
    };
    let a = parse(start)?;
    let b = parse(end)?;
    if b < a || (b - a).num_days() >= 366 {
        bail!("Период аналитики: от 1 до 366 дней");
    }
    Ok((a, b))
}

pub async fn youtube_analytics(
    video_id: &str,
    start: &str,
    end: &str,
    currency: &str,
    include_revenue: bool,
) -> Result<Vec<Value>> {
    if !valid_video_id(video_id) {
        bail!("Некорректный YouTube video_id");
    }
    analytics_dates(start, end)?;
    if !["RUB", "USD", "EUR"].contains(&currency) {
        bail!("Валюта аналитики: RUB, USD или EUR");
    }
    let token = credential("YOUTUBE_ANALYTICS_ACCESS_TOKEN")
        .or_else(|_| credential("YOUTUBE_ACCESS_TOKEN"))?;
    analytics_request(
        &client()?,
        YOUTUBE_ANALYTICS,
        &token,
        video_id,
        (start, end),
        currency,
        include_revenue,
    )
    .await
}

async fn analytics_request(
    api: &Client,
    url: &str,
    token: &str,
    video_id: &str,
    dates: (&str, &str),
    currency: &str,
    include_revenue: bool,
) -> Result<Vec<Value>> {
    let (start, end) = dates;
    let response = api
        .get(url)
        .bearer_auth(token)
        .query(&[
            ("ids", "channel==MINE"),
            ("startDate", start),
            ("endDate", end),
            ("dimensions", "day"),
            (
                "metrics",
                if include_revenue {
                    "views,estimatedRevenue"
                } else {
                    "views"
                },
            ),
            ("filters", &format!("video=={video_id}")),
            ("sort", "day"),
            ("currency", currency),
        ])
        .send()
        .await
        .map_err(|_| anyhow!("YouTube Analytics: ошибка соединения"))?;
    if !response.status().is_success() {
        return Err(rejected("YouTube Analytics", response.status()));
    }
    parse_analytics(
        response_json(response).await?,
        start,
        end,
        currency,
        include_revenue,
    )
}

fn parse_analytics(
    result: Value,
    start: &str,
    end: &str,
    currency: &str,
    include_revenue: bool,
) -> Result<Vec<Value>> {
    let (start, end) = analytics_dates(start, end)?;
    let columns = result
        .get("columnHeaders")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("В аналитике нет заголовков колонок"))?;
    let locate = |name: &str| {
        columns
            .iter()
            .position(|column| column.get("name").and_then(Value::as_str) == Some(name))
    };
    let day_index = locate("day").ok_or_else(|| anyhow!("В аналитике нет колонки day"))?;
    let views_index = locate("views").ok_or_else(|| anyhow!("В аналитике нет колонки views"))?;
    let revenue_index = if include_revenue {
        Some(
            locate("estimatedRevenue")
                .ok_or_else(|| anyhow!("В аналитике нет запрошенного estimatedRevenue"))?,
        )
    } else {
        None
    };
    let rows = match result.get("rows") {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Array(rows)) => rows,
        _ => bail!("Некорректные строки аналитики"),
    };
    let mut output = Vec::with_capacity(rows.len());
    let mut seen = std::collections::HashSet::new();
    for row in rows {
        let row = row
            .as_array()
            .ok_or_else(|| anyhow!("Некорректная строка аналитики"))?;
        let day = row
            .get(day_index)
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("Некорректный день аналитики"))?;
        let (date, _) = analytics_dates(day, day)?;
        if date < start || date > end || !seen.insert(day.to_string()) {
            bail!("День аналитики повторён или вне периода");
        }
        let views = row
            .get(views_index)
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow!("Некорректное число просмотров"))?;
        let revenue = if let Some(index) = revenue_index {
            let amount = row
                .get(index)
                .and_then(Value::as_f64)
                .ok_or_else(|| anyhow!("Некорректная оценка дохода"))?;
            let minor = (amount * 100.0).round();
            if !minor.is_finite() || minor < i64::MIN as f64 || minor >= i64::MAX as f64 {
                bail!("Оценка дохода вне диапазона");
            }
            Some(minor as i64)
        } else {
            None
        };
        output.push(json!({"date": day, "views": views, "api_estimated_revenue_minor": revenue, "currency": currency, "source": "youtube_analytics"}));
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Artifact, Profile, ProjectSpec, Scene, Voice};
    use axum::{
        extract::State,
        http::{HeaderMap, HeaderValue},
        response::IntoResponse,
        routing::{get, post, put},
        Router,
    };
    use std::sync::{Arc, Mutex};

    fn sample() -> Project {
        Project {
            id: "project1".into(),
            revision: 2,
            created_at: "".into(),
            updated_at: "".into(),
            status: "rendered".into(),
            error: None,
            spec: ProjectSpec {
                title: "Title".into(),
                description: "Description".into(),
                topic: "".into(),
                profile: Profile::Shorts,
                language: "ru".into(),
                voice: Voice::None,
                scenes: vec![Scene {
                    title: "Scene".into(),
                    text: "Text".into(),
                    narration: "Voice".into(),
                    duration_s: 2.0,
                    asset: None,
                }],
                music_asset: None,
                source_urls: vec![],
                tags: vec![],
            },
            artifact: Some(Artifact {
                id: "artifact1".into(),
                path: "render.mp4".into(),
                width: 1080,
                height: 1920,
                duration_s: 2.0,
                revision: 2,
                sha256: "".into(),
                created_at: "".into(),
            }),
        }
    }

    async fn fixture() -> (tempfile::TempDir, Project, Bundle) {
        let dir = tempfile::tempdir().unwrap();
        let mut project = sample();
        tokio::fs::write(dir.path().join("render.mp4"), b"fixture-video")
            .await
            .unwrap();
        project.artifact.as_mut().unwrap().sha256 =
            digest(&dir.path().join("render.mp4")).await.unwrap();
        let request: PublishRequest =
            serde_json::from_value(json!({"platform":"youtube"})).unwrap();
        let bundle = export_bundle(&project, dir.path(), &request).await.unwrap();
        (dir, project, bundle)
    }

    #[test]
    fn defaults_and_dates_are_safe() {
        let request: PublishRequest =
            serde_json::from_value(json!({"platform":"youtube"})).unwrap();
        assert_eq!(request.privacy, "private");
        assert!(request.dry_run);
        assert!(analytics_dates("2024-01-01", "2024-12-31").is_ok());
        assert!(analytics_dates("2024-01-01", "2025-01-01").is_err());
        assert!(analytics_dates("2024-1-01", "2024-01-01").is_err());
    }

    #[tokio::test]
    async fn export_rejects_stale_and_changed_artifact() {
        let (dir, mut project, _) = fixture().await;
        let request: PublishRequest = serde_json::from_value(json!({"platform":"export"})).unwrap();
        project.revision += 1;
        assert!(publish(&project, dir.path(), &request)
            .await
            .unwrap_err()
            .to_string()
            .contains("устарел"));
        project.revision -= 1;
        tokio::fs::write(dir.path().join("render.mp4"), b"modified-video")
            .await
            .unwrap();
        assert!(publish(&project, dir.path(), &request)
            .await
            .unwrap_err()
            .to_string()
            .contains("SHA256"));
    }

    #[tokio::test]
    async fn artifact_cannot_escape_data_directory() {
        let (dir, mut project, _) = fixture().await;
        let outside = tempfile::tempdir().unwrap();
        let file = outside.path().join("outside.mp4");
        tokio::fs::write(&file, b"fixture-video").await.unwrap();
        project.artifact.as_mut().unwrap().path = file.to_string_lossy().into_owned();
        let request: PublishRequest = serde_json::from_value(json!({"platform":"export"})).unwrap();
        assert!(publish(&project, dir.path(), &request)
            .await
            .unwrap_err()
            .to_string()
            .contains("папке данных"));
    }

    #[test]
    fn telegram_emoji_caption_stays_within_limit() {
        let caption = telegram_caption(&"😀".repeat(700));
        assert_eq!(caption.chars().count(), 512);
        assert_eq!(caption.encode_utf16().count(), 1024);
    }

    #[tokio::test]
    async fn dry_run_exports_without_credentials() {
        let (dir, project, _) = fixture().await;
        let request: PublishRequest =
            serde_json::from_value(json!({"platform":"telegram"})).unwrap();
        let result = publish(&project, dir.path(), &request).await.unwrap();
        assert_eq!(result["kind"], "simulated");
        assert_eq!(result["external_request_sent"], false);
        assert_eq!(result["manifest"]["caption_source"], "nominal_fallback");
        let path = result["manifest"]["bundle"].as_str().unwrap();
        for filename in ["video.mp4", "caption.txt", "metadata.json", "captions.srt"] {
            assert!(dir.path().join(path).join(filename).exists());
        }
    }

    #[tokio::test]
    async fn export_preserves_renderer_captions_for_extended_tts_timeline() {
        let (dir, mut project, _) = fixture().await;
        // The spec requests two seconds, but speech extended the rendered scene.
        project.artifact.as_mut().unwrap().duration_s = 3.8;
        let actual = "1\n00:00:00,000 --> 00:00:01,900\nVoice first chunk\n\n2\n00:00:01,900 --> 00:00:03,800\nVoice second chunk\n\n";
        tokio::fs::write(dir.path().join("captions.srt"), actual)
            .await
            .unwrap();
        let request: PublishRequest =
            serde_json::from_value(json!({"platform":"export", "dry_run":false})).unwrap();
        let result = publish(&project, dir.path(), &request).await.unwrap();
        let bundle = result["manifest"]["bundle"].as_str().unwrap();
        let exported = tokio::fs::read_to_string(dir.path().join(bundle).join("captions.srt"))
            .await
            .unwrap();
        assert_eq!(exported, actual);
        assert_eq!(result["manifest"]["caption_source"], "renderer");
        assert!(result["manifest"]["caption_timing"]
            .as_str()
            .unwrap()
            .contains("renderer timeline"));
    }

    #[tokio::test]
    async fn invalid_renderer_captions_do_not_silently_fall_back() {
        let (dir, project, _) = fixture().await;
        tokio::fs::write(
            dir.path().join("captions.srt"),
            vec![b'a'; CAPTION_LIMIT as usize + 1],
        )
        .await
        .unwrap();
        let request: PublishRequest = serde_json::from_value(json!({"platform":"export"})).unwrap();
        assert!(publish(&project, dir.path(), &request)
            .await
            .unwrap_err()
            .to_string()
            .contains("1 MiB"));
    }

    #[test]
    fn analytics_uses_column_names_and_distinguishes_unrequested_revenue() {
        let data = json!({"columnHeaders":[{"name":"estimatedRevenue"},{"name":"day"},{"name":"views"}],"rows":[[1.235,"2026-10-01",42]]});
        let rows = parse_analytics(data.clone(), "2026-10-01", "2026-10-01", "USD", true).unwrap();
        assert_eq!(rows[0]["api_estimated_revenue_minor"], 124);
        let rows = parse_analytics(data, "2026-10-01", "2026-10-01", "USD", false).unwrap();
        assert!(rows[0]["api_estimated_revenue_minor"].is_null());
    }

    #[tokio::test]
    async fn official_upload_protocol_streams_and_reads_actual_privacy() {
        #[derive(Clone)]
        struct StateData {
            base: String,
            seen: Arc<Mutex<Vec<String>>>,
        }
        async fn start(
            State(state): State<StateData>,
            headers: HeaderMap,
            axum::Json(body): axum::Json<Value>,
        ) -> impl IntoResponse {
            assert_eq!(headers.get("x-upload-content-length").unwrap(), "13");
            assert_eq!(body["status"]["privacyStatus"], "private");
            state.seen.lock().unwrap().push("session".into());
            let mut response = StatusCode::OK.into_response();
            response.headers_mut().insert(
                header::LOCATION,
                HeaderValue::from_str(&format!("{}/upload", state.base)).unwrap(),
            );
            response
        }
        async fn upload(
            State(state): State<StateData>,
            bytes: axum::body::Bytes,
        ) -> impl IntoResponse {
            assert_eq!(&bytes[..], b"fixture-video");
            state.seen.lock().unwrap().push("upload".into());
            axum::Json(
                json!({"id":"abcdefghijk","status":{"privacyStatus":"private","uploadStatus":"uploaded"}}),
            )
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(vec![]));
        let router = Router::new()
            .route("/session", post(start))
            .route("/upload", put(upload))
            .with_state(StateData {
                base: base.clone(),
                seen: seen.clone(),
            });
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let (_dir, project, bundle) = fixture().await;
        let request: PublishRequest =
            serde_json::from_value(json!({"platform":"youtube"})).unwrap();
        let receipt = youtube_upload(
            &client().unwrap(),
            &format!("{base}/session"),
            "test-token",
            &project,
            &bundle,
            &request,
            true,
        )
        .await
        .unwrap();
        assert_eq!(receipt["privacy"], "private");
        assert_eq!(receipt["status"], "uploaded");
        assert_eq!(seen.lock().unwrap().as_slice(), ["session", "upload"]);
        server.abort();
    }

    #[tokio::test]
    async fn ambiguous_response_never_claims_success_or_exposes_token() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = Router::new().route(
            "/botsecret-token/sendVideo",
            post(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "secret-token") }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let (_dir, project, bundle) = fixture().await;
        let error = telegram_upload(
            &client().unwrap(),
            &base,
            "secret-token",
            "123",
            &project,
            &bundle,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.starts_with("UNKNOWN_OUTCOME:"));
        assert!(!error.contains("secret-token"));
        server.abort();
    }

    #[tokio::test]
    async fn analytics_query_maps_reordered_columns() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = Router::new().route("/reports", get(|axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String,String>>| async move {
            assert_eq!(query["metrics"], "views,estimatedRevenue"); assert_eq!(query["filters"], "video==abcdefghijk");
            axum::Json(json!({"columnHeaders":[{"name":"views"},{"name":"estimatedRevenue"},{"name":"day"}],"rows":[[123,2.0,"2026-10-01"]]}))
        }));
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let rows = analytics_request(
            &client().unwrap(),
            &format!("{base}/reports"),
            "test-token",
            "abcdefghijk",
            ("2026-10-01", "2026-10-01"),
            "USD",
            true,
        )
        .await
        .unwrap();
        assert_eq!(rows[0]["views"], 123);
        assert_eq!(rows[0]["api_estimated_revenue_minor"], 200);
        server.abort();
    }
}
