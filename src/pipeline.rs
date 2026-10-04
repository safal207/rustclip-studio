//! A bounded serial conveyor. Trend rank is a discovery heuristic, not proof of
//! future reach; generated scripts remain editable and source facts unverified.
//! Live posting requires an explicit config and configured official adapters.

use crate::{
    generate::{self, GenerateRequest},
    models::{Profile, Project, Trend, Voice},
    publish::{self, PublishRequest},
    render, server,
    store::Store,
    trends,
};
use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, FixedOffset, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::HashSet, time::Duration};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PipelineConfig {
    pub source: String,
    pub region: String,
    pub angle: String,
    pub profile: Profile,
    pub voice: Voice,
    pub language: String,
    pub use_ollama: bool,
    pub platform: String,
    pub privacy: String,
    pub dry_run: bool,
    pub limit: usize,
    pub daily_budget: usize,
    pub interval_minutes: u64,
    /// Calendar-day budget at a fixed UTC offset, e.g. +3 for Moscow/Turkey.
    pub utc_offset_hours: i32,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            source: "google".into(),
            region: "RU".into(),
            angle: "Объяснить тему через собственный проверяемый пример".into(),
            profile: Profile::Shorts,
            voice: Voice::Espeak,
            language: "ru".into(),
            use_ollama: false,
            platform: "export".into(),
            privacy: "private".into(),
            dry_run: true,
            limit: 1,
            daily_budget: 2,
            interval_minutes: 60,
            utc_offset_hours: 3,
        }
    }
}

impl PipelineConfig {
    pub fn validate(&self) -> Result<()> {
        if !["google", "youtube"].contains(&self.source.as_str()) {
            bail!("Источник конвейера: google или youtube");
        }
        if self.region.len() != 2 || !self.region.bytes().all(|c| c.is_ascii_alphabetic()) {
            bail!("Регион конвейера: две латинские буквы, например RU, US или TR");
        }
        if self.angle.trim().is_empty() || self.angle.chars().count() > 240 {
            bail!("Собственный ракурс: от 1 до 240 символов");
        }
        if !["ru", "en"].contains(&self.language.as_str()) {
            bail!("Язык конвейера: ru или en");
        }
        if !(1..=3).contains(&self.limit) {
            bail!("Лимит одного цикла: от 1 до 3 роликов");
        }
        if !(1..=10).contains(&self.daily_budget) {
            bail!("Дневной бюджет: от 1 до 10 новых проектов");
        }
        if !(15..=1440).contains(&self.interval_minutes) {
            bail!("Интервал конвейера: от 15 до 1440 минут");
        }
        if !(-12..=14).contains(&self.utc_offset_hours) {
            bail!("Часовой пояс дневного бюджета: от UTC-12 до UTC+14");
        }
        self.publish_request().validate()
    }

    fn publish_request(&self) -> PublishRequest {
        PublishRequest {
            platform: self.platform.clone(),
            privacy: self.privacy.clone(),
            dry_run: self.dry_run,
            scheduled_at: None,
            made_for_kids: false,
            contains_synthetic_media: false,
        }
    }

    fn budget_offset(&self) -> Result<FixedOffset> {
        FixedOffset::east_opt(self.utc_offset_hours * 3600)
            .ok_or_else(|| anyhow!("Некорректный часовой пояс дневного бюджета"))
    }
}

struct TopicSelection {
    selected: Vec<Trend>,
    skipped_existing: usize,
    skipped_duplicate: usize,
}

/// Exact topic text after trimming. Failed/draft projects also prevent repeats;
/// case folding or semantic similarity never silently merges unrelated topics.
fn select_topics(trends: &[Trend], projects: &[Project], limit: usize) -> TopicSelection {
    let existing: HashSet<&str> = projects.iter().map(|p| p.spec.topic.trim()).collect();
    let mut ranked = trends.to_vec();
    ranked.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
    let mut seen = HashSet::new();
    let mut selected = Vec::new();
    let mut skipped_existing = 0;
    let mut skipped_duplicate = 0;
    for trend in ranked {
        if existing.contains(trend.title.trim()) {
            skipped_existing += 1;
        } else if !seen.insert(trend.title.trim().to_owned()) {
            skipped_duplicate += 1;
        } else if selected.len() < limit {
            selected.push(trend);
        }
    }
    TopicSelection {
        selected,
        skipped_existing,
        skipped_duplicate,
    }
}

fn budget_day(at: DateTime<Utc>, offset: FixedOffset) -> NaiveDate {
    at.with_timezone(&offset).date_naive()
}

/// All projects count, including drafts, failed renders and manually created
/// projects. Invalid timestamps stop the cycle rather than bypassing the cap.
fn remaining_daily_budget(
    projects: &[Project],
    budget: usize,
    at: DateTime<Utc>,
    offset: FixedOffset,
) -> Result<usize> {
    let today = budget_day(at, offset);
    let mut used = 0usize;
    for project in projects {
        let created = DateTime::parse_from_rfc3339(&project.created_at)
            .context("У проекта некорректная дата создания; дневной бюджет не проверен")?;
        if created.with_timezone(&offset).date_naive() == today {
            used = used.saturating_add(1);
        }
    }
    Ok(budget.saturating_sub(used))
}

fn sanitized_error(error: &anyhow::Error) -> String {
    let mut text = format!("{error:#}");
    for name in [
        "YOUTUBE_ACCESS_TOKEN",
        "YOUTUBE_ANALYTICS_ACCESS_TOKEN",
        "YOUTUBE_API_KEY",
        "TELEGRAM_BOT_TOKEN",
    ] {
        if let Ok(secret) = std::env::var(name) {
            if !secret.is_empty() {
                text = text.replace(&secret, "[redacted]");
            }
        }
    }
    text.chars().take(2000).collect()
}

fn item_error(trend: &Trend, stage: &str, message: String, project_id: Option<&str>) -> Value {
    json!({"trend_id": trend.id, "topic": trend.title, "stage": stage, "message": message, "project_id": project_id})
}

/// Caller owns execution exclusivity: CLI owns the Store directory lock; the
/// server acquires its shared renderer semaphore for the whole cycle.
pub async fn cycle(store: &Store, config: &PipelineConfig) -> Result<Value> {
    config.validate()?;
    if !config.dry_run
        && config.platform != "export"
        && publish::capabilities()[&config.platform]["configured"] != true
    {
        bail!("Для живой публикации сначала настройте официальный адаптер выбранной площадки");
    }
    let offset = config.budget_offset()?;
    let cycle_id = crate::models::id();
    // record_intent verifies the immutable graph and every projection before
    // source requests or external actions. Store failures always escape `?`.
    let intent = store.record_intent(
        &format!("pipeline:{cycle_id}"),
        "Оператор запустил ограниченный конвейер: новый тренд → оригинальный редактируемый черновик → рендер → выбранный адаптер",
        serde_json::to_value(config)?,
    )?;
    let fresh = trends::fetch(&config.source, &config.region)
        .await
        .map_err(|error| anyhow!(sanitized_error(&error)))?;
    store.save_trends(&fresh)?;
    let projects = store.projects()?;
    let initial_budget =
        remaining_daily_budget(&projects, config.daily_budget, Utc::now(), offset)?;
    let selection = select_topics(&fresh, &projects, config.limit.min(initial_budget));
    let selected_count = selection.selected.len();
    let mut created = Vec::new();
    let mut errors = Vec::new();
    let mut skipped_during_cycle = 0usize;
    let mut skipped_admission = Vec::new();

    for trend in &selection.selected {
        // Recheck the calendar day between items, including midnight and any
        // manual projects created while an earlier render was running.
        if remaining_daily_budget(&store.projects()?, config.daily_budget, Utc::now(), offset)? == 0
        {
            break;
        }
        let request = GenerateRequest {
            topic: trend.title.clone(),
            angle: config.angle.clone(),
            profile: config.profile,
            voice: config.voice,
            language: config.language.clone(),
            source_urls: vec![trend.url.clone()],
            use_ollama: config.use_ollama,
        };
        let (mut spec, generator) = match generate::draft(request).await {
            Ok(value) => value,
            Err(error) => {
                errors.push(item_error(trend, "draft", sanitized_error(&error), None));
                continue;
            }
        };
        // The selected signal is the exact topic identity used for dedup and
        // source lineage, even if a model suggested another topic label.
        spec.topic = trend.title.clone();
        // A model prompt alone does not establish facts or performance. Keep a
        // visible disclosure even if a model omitted the requested disclaimer.
        if generator == "ollama" {
            let label = if config.language == "ru" {
                "Редактируемый черновик ИИ. Содержание источников не проверено; факты требуют подтверждения. Популярность темы не гарантирует просмотры или доход."
            } else {
                "Editable AI draft. Source contents have not been checked; facts need verification. Topic popularity does not guarantee views or revenue."
            };
            spec.description = format!("{label}\n\n{}", spec.description);
        }
        if let Err(error) = spec.validate() {
            errors.push(item_error(trend, "draft", sanitized_error(&error), None));
            continue;
        }
        // Generation may span midnight or race with manual creation. Admission
        // and insertion use one IMMEDIATE transaction, never a stale precheck.
        let project = match store.create_pipeline_project(
            spec,
            &generator,
            Some(&intent.id),
            config.daily_budget,
            config.utc_offset_hours,
        )? {
            Some(project) => project,
            None => {
                skipped_during_cycle += 1;
                skipped_admission.push(json!({"trend_id":trend.id,"topic":trend.title,"reason":"budget_or_topic_changed_at_admission","project_created":false}));
                continue;
            }
        };
        let (render_job, start) = store.start_render(&project.id)?;
        let mut item = json!({
            "trend_id": trend.id, "topic": trend.title, "project_id": project.id,
            "generator": generator, "editable_draft": true, "facts_verified": false,
            "render_job_id": render_job.id, "publish_job_id": null,
            "state": render_job.state, "stage": "render"
        });
        let render_job = if start {
            store.begin_job(&render_job.id)?;
            let rendering_project = store.project(&project.id)?;
            let result = render::render(&rendering_project, &store.root).await;
            store.finish_render(&render_job.id, result)?
        } else {
            render_job
        };
        item["render_state"] = json!(render_job.state);
        if render_job.state != "succeeded" {
            item["state"] = json!(render_job.state);
            if let Some(error) = &render_job.error {
                errors.push(item_error(
                    trend,
                    "render",
                    sanitized_error(&anyhow!(error.clone())),
                    Some(&project.id),
                ));
            }
            created.push(item);
            continue;
        }
        let publish_request = config.publish_request();
        let job = store.queue_publish(&project.id, serde_json::to_value(publish_request)?, None)?;
        item["publish_job_id"] = json!(job.id);
        item["stage"] = json!("publish");
        // execute_publish admits the job once, checks graph/revision/digest and
        // persists actual receipts or UNKNOWN_OUTCOME without automatic retry.
        server::execute_publish(store, &job.id).await?;
        let finished = store.job(&job.id)?;
        let pending = ["queued", "scheduled", "running"].contains(&finished.state.as_str());
        item["publish_state"] = json!(finished.state);
        item["publishing_pending"] = json!(pending);
        item["state"] = if pending {
            json!("publishing_pending")
        } else {
            json!(finished.state)
        };
        item["result"] = json!(finished.result);
        if let Some(error) = &finished.error {
            errors.push(item_error(
                trend,
                "publish",
                sanitized_error(&anyhow!(error.clone())),
                Some(&project.id),
            ));
        }
        created.push(item);
    }
    let finished_at = Utc::now();
    let remaining =
        remaining_daily_budget(&store.projects()?, config.daily_budget, finished_at, offset)?;
    Ok(json!({
        "cycle_id": cycle_id, "intent_event_id": intent.id, "intent_hash": intent.hash,
        "created": created, "errors": errors,
        "skipped_existing": selection.skipped_existing,
        "skipped_during_admission": skipped_during_cycle,
        "skipped_admission": skipped_admission,
        "skipped_duplicate": selection.skipped_duplicate,
        "fetched": fresh.len(), "selected": selected_count,
        "daily_budget_remaining": remaining, "daily_budget": config.daily_budget,
        "budget_day": budget_day(finished_at, offset).to_string(),
        "utc_offset_hours": config.utc_offset_hours,
        "dry_run": config.dry_run, "platform": config.platform,
        "performance": "unproven; trend ranking does not predict reach or revenue",
        "finished_at": finished_at.to_rfc3339()
    }))
}

/// Ctrl-C requests a graceful stop after the current bounded cycle, allowing an
/// in-flight upload to finish and its real outcome to be recorded.
pub async fn watch(store: Store, config: PipelineConfig) -> Result<()> {
    config.validate()?;
    let mut shutdown = tokio::spawn(tokio::signal::ctrl_c());
    loop {
        match cycle(&store, &config).await {
            Ok(report) => println!("{}", serde_json::to_string(&report)?),
            // An integrity/projection error must stop automation. Other top-level
            // failures are also deliberately propagated; restarting is explicit.
            Err(error) => {
                shutdown.abort();
                return Err(anyhow!(sanitized_error(&error)));
            }
        }
        if shutdown.is_finished() {
            shutdown
                .await
                .context("Не удалось завершить обработчик Ctrl-C")??;
            return Ok(());
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(config.interval_minutes * 60)) => {},
            signal = &mut shutdown => {
                signal.context("Не удалось завершить обработчик Ctrl-C")??;
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(topic: &str, created_at: &str, status: &str) -> Project {
        let mut project: Project = serde_json::from_value(json!({
            "id":topic, "revision":1, "created_at":created_at, "updated_at":created_at,
            "status":status, "artifact":null, "error":null,
            "spec":{"title":"Test", "topic":topic, "scenes":[{"title":"One","text":"Text","narration":"","duration_s":1.0}],"voice":"none"}
        })).unwrap();
        project.spec.topic = topic.into();
        project
    }

    fn trend(topic: &str, score: f64, id: &str) -> Trend {
        Trend {
            id: id.into(),
            title: topic.into(),
            url: "https://example.com/source".into(),
            source: "google".into(),
            published_at: "".into(),
            fetched_at: "2026-10-03T22:00:00Z".into(),
            volume: None,
            score,
            evidence_kind: "popular_search".into(),
        }
    }

    #[test]
    fn safe_defaults_and_config_bounds() {
        let config: PipelineConfig = serde_json::from_value(json!({})).unwrap();
        config.validate().unwrap();
        assert!(config.dry_run);
        assert_eq!(config.platform, "export");
        assert_eq!(config.privacy, "private");
        assert_eq!(config.utc_offset_hours, 3);
        let sample: PipelineConfig =
            serde_json::from_str(include_str!("../examples/autopilot.json")).unwrap();
        sample.validate().unwrap();
        assert!(sample.dry_run);
        for patch in [
            json!({"limit":0}),
            json!({"limit":4}),
            json!({"daily_budget":0}),
            json!({"daily_budget":11}),
            json!({"interval_minutes":14}),
            json!({"interval_minutes":1441}),
            json!({"utc_offset_hours":-13}),
            json!({"utc_offset_hours":15}),
            json!({"source":"reddit"}),
            json!({"platform":"tiktok"}),
            json!({"privacy":"friends"}),
            json!({"region":"RUS"}),
            json!({"angle":""}),
            json!({"language":"de"}),
        ] {
            let invalid: PipelineConfig = serde_json::from_value(patch).unwrap();
            assert!(invalid.validate().is_err(), "{invalid:?}");
        }
        assert!(serde_json::from_value::<PipelineConfig>(json!({"dry_rnu":false})).is_err());
    }

    #[test]
    fn selection_skips_existing_failed_topics_and_ranks_new_exact_topics() {
        let projects = vec![project("Existing", "2026-10-03T22:00:00Z", "failed")];
        let signals = vec![
            trend("Fresh", 20.0, "low"),
            trend("Existing", 100.0, "used"),
            trend("Fresh", 80.0, "high"),
            trend("fresh", 60.0, "different-case"),
            trend("Other", 40.0, "other"),
        ];
        let selected = select_topics(&signals, &projects, 2);
        assert_eq!(
            selected
                .selected
                .iter()
                .map(|t| t.id.as_str())
                .collect::<Vec<_>>(),
            ["high", "different-case"]
        );
        assert_eq!(selected.skipped_existing, 1);
        assert_eq!(selected.skipped_duplicate, 1);
    }

    #[test]
    fn daily_cap_counts_all_projects_at_offset_midnight() {
        let offset = FixedOffset::east_opt(3 * 3600).unwrap();
        let at = DateTime::parse_from_rfc3339("2026-10-03T21:30:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let projects = vec![
            project("Yesterday", "2026-10-03T20:59:59Z", "ready"),
            project("Today failed", "2026-10-03T21:00:00Z", "failed"),
            project("Today manual draft", "2026-10-04T00:20:00+03:00", "draft"),
        ];
        assert_eq!(budget_day(at, offset).to_string(), "2026-10-04");
        assert_eq!(remaining_daily_budget(&projects, 2, at, offset).unwrap(), 0);
        assert_eq!(remaining_daily_budget(&projects, 3, at, offset).unwrap(), 1);
        assert_eq!(remaining_daily_budget(&projects, 1, at, offset).unwrap(), 0);
        let bad = vec![project("Invalid", "unknown", "draft")];
        assert!(remaining_daily_budget(&bad, 2, at, offset).is_err());
    }

    #[tokio::test]
    async fn projection_corruption_stops_cycle_before_fetch_or_generation() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let spec = project("Original", "2026-10-03T22:00:00Z", "draft").spec;
        let created = store.create_project(spec, "manual").unwrap();
        let connection = rusqlite::Connection::open(store.root.join("studio.sqlite3")).unwrap();
        connection
            .execute(
                "UPDATE projects SET json=json_set(json,'$.spec.topic','Tampered') WHERE id=?1",
                [created.id],
            )
            .unwrap();
        let error = cycle(&store, &PipelineConfig::default())
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("не соответствует") || error.contains("изменена"),
            "{error}"
        );
        assert_eq!(store.events().unwrap().len(), 1);
    }
}
