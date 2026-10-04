use crate::{graph, models::*, money::LedgerRow};
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs::File,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

#[derive(Clone)]
pub struct Store {
    pub root: PathBuf,
    db: PathBuf,
    // The final Store clone dropping closes this descriptor and releases the
    // operating-system lock. Never delete the lock file while it is in use.
    _root_lock: Arc<File>,
}

struct EventDraft {
    subject: String,
    operation: String,
    execution: String,
    from: String,
    to: String,
    reason: String,
    claim: String,
    scope: String,
    confidence: Option<f64>,
    parents: Vec<(String, String)>,
    supersedes: Option<String>,
    valid_at: String,
    expected: Value,
    observed: Value,
    evidence: Value,
}
impl EventDraft {
    fn new(
        subject: &str,
        operation: &str,
        execution: &str,
        from: &str,
        to: &str,
        reason: &str,
    ) -> Self {
        Self {
            subject: subject.into(),
            operation: operation.into(),
            execution: execution.into(),
            from: from.into(),
            to: to.into(),
            reason: reason.into(),
            claim: "OBSERVATION".into(),
            scope: "local".into(),
            confidence: None,
            parents: vec![],
            supersedes: None,
            valid_at: now(),
            expected: json!({}),
            observed: json!({}),
            evidence: json!({}),
        }
    }
}
pub fn digest(value: &impl serde::Serialize) -> Result<String> {
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(value)?)))
}

impl Store {
    pub fn open(root: &Path) -> Result<Self> {
        std::fs::create_dir_all(root)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))?;
        }
        let root = std::fs::canonicalize(root)?;
        let root_lock = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join("studio.lock"))?;
        root_lock.try_lock().map_err(|error| anyhow::anyhow!(
            "Папка студии уже используется другим процессом. Остановите сервер перед запуском CLI с этой папкой: {error}"
        ))?;
        for name in ["assets", "renders", "exports"] {
            std::fs::create_dir_all(root.join(name))?;
        }
        let store = Self {
            db: root.join("studio.sqlite3"),
            root,
            _root_lock: Arc::new(root_lock),
        };
        let c = store.conn()?;
        c.execute_batch("PRAGMA journal_mode=WAL;
            CREATE TABLE IF NOT EXISTS projects(id TEXT PRIMARY KEY, json TEXT NOT NULL, updated_at TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS trends(id TEXT PRIMARY KEY, json TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS jobs(id TEXT PRIMARY KEY, operation_key TEXT NOT NULL, json TEXT NOT NULL, created_at TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS jobs_operation ON jobs(operation_key);
            CREATE TABLE IF NOT EXISTS ledger(row_key TEXT PRIMARY KEY, json TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS events(seq INTEGER PRIMARY KEY, id TEXT UNIQUE NOT NULL, subject_id TEXT NOT NULL, hash TEXT UNIQUE NOT NULL, json TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS events_subject ON events(subject_id,seq);
            CREATE TRIGGER IF NOT EXISTS events_no_update BEFORE UPDATE ON events BEGIN SELECT RAISE(ABORT, 'causal history is append-only'); END;
            CREATE TRIGGER IF NOT EXISTS events_no_delete BEFORE DELETE ON events BEGIN SELECT RAISE(ABORT, 'causal history is append-only'); END;")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&store.db, std::fs::Permissions::from_mode(0o600))?;
        }
        drop(c);
        let mut c = store.conn()?;
        let tx = c.transaction()?;
        Self::validate_projections_tx(&tx)?;
        drop(tx);
        Ok(store)
    }
    fn conn(&self) -> Result<Connection> {
        let c = Connection::open(&self.db)?;
        c.busy_timeout(Duration::from_secs(5))?;
        Ok(c)
    }
    fn project_tx(tx: &Transaction<'_>, id: &str) -> Result<Project> {
        let raw: Option<(String, String)> = tx
            .query_row(
                "SELECT json,updated_at FROM projects WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let (raw, updated_at) = raw.context("Проект не найден")?;
        let value: Value = serde_json::from_str(&raw).context("Проект повреждён")?;
        let events = Self::checked_events_tx(tx)?;
        Self::check_projection(&events, "project", id, &value)?;
        let project: Project = serde_json::from_value(value)?;
        if project.id != id || project.updated_at != updated_at {
            bail!("Индекс проекта не соответствует причинному журналу");
        }
        Ok(project)
    }
    fn put_project(tx: &Transaction<'_>, p: &Project) -> Result<()> {
        tx.execute("INSERT INTO projects VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET json=excluded.json,updated_at=excluded.updated_at",params![p.id,serde_json::to_string(p)?,p.updated_at])?;
        Ok(())
    }
    fn put_job(tx: &Transaction<'_>, j: &Job) -> Result<()> {
        tx.execute(
            "INSERT INTO jobs VALUES(?1,?2,?3,?4) ON CONFLICT(id) DO UPDATE SET json=excluded.json",
            params![
                j.id,
                j.operation_key,
                serde_json::to_string(j)?,
                j.created_at
            ],
        )?;
        Ok(())
    }
    fn job_tx(tx: &Transaction<'_>, id: &str) -> Result<Job> {
        let raw: Option<(String, String, String)> = tx
            .query_row(
                "SELECT json,operation_key,created_at FROM jobs WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let (raw, operation_key, created_at) = raw.context("Задача не найдена")?;
        let value: Value = serde_json::from_str(&raw)?;
        let events = Self::checked_events_tx(tx)?;
        Self::check_projection(&events, "job", id, &value)?;
        let job: Job = serde_json::from_value(value)?;
        if job.id != id || job.operation_key != operation_key || job.created_at != created_at {
            bail!("Индекс задачи не соответствует причинному журналу");
        }
        Ok(job)
    }

    fn checked_events_tx(tx: &Transaction<'_>) -> Result<Vec<CausalEvent>> {
        let mut query =
            tx.prepare("SELECT seq,id,subject_id,hash,json FROM events ORDER BY seq")?;
        let raw = query
            .query_map([], |row| {
                Ok((
                    row.get::<_, u64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut events = Vec::with_capacity(raw.len());
        for (seq, id, subject, hash, json) in raw {
            let event: CausalEvent = serde_json::from_str(&json)?;
            if event.seq != seq
                || event.id != id
                || event.subject_id != subject
                || event.hash != hash
            {
                bail!("Индексы причинного события не соответствуют его содержимому");
            }
            events.push(event);
        }
        let integrity = graph::verify(&events);
        if !integrity.valid {
            bail!(
                "Причинный журнал повреждён: {}",
                integrity.error.unwrap_or_default()
            );
        }
        Ok(events)
    }

    fn projection_key(field: &str, value: &Value) -> Result<String> {
        if field == "ledger" {
            let row: LedgerRow = serde_json::from_value(value.clone())?;
            row.validate()?;
            Ok(row.key())
        } else {
            Ok(value["id"]
                .as_str()
                .context("В снимке проекции отсутствует ID")?
                .to_string())
        }
    }

    fn normalized_projection(field: &str, value: &Value) -> Result<Value> {
        let mut normalized = value.clone();
        if field == "job" {
            // Job.progress is f32. serde_json::to_string(Job) writes its short
            // decimal while json!({"job": Job}) first widens it to f64. Compare
            // the actual model value consistently, preserving every other key.
            let progress: f32 = serde_json::from_value(value["progress"].clone())?;
            normalized["progress"] = json!(progress);
        }
        Ok(normalized)
    }

    fn check_projection(
        events: &[CausalEvent],
        field: &str,
        key: &str,
        value: &Value,
    ) -> Result<()> {
        if Self::projection_key(field, value)? != key {
            bail!("ID проекции {field} не соответствует индексу");
        }
        let expected = events
            .iter()
            .rev()
            .find_map(|event| {
                let snapshot = event.observed.get(field)?;
                (Self::projection_key(field, snapshot).ok().as_deref() == Some(key))
                    .then_some(snapshot)
            })
            .with_context(|| format!("У проекции {field} отсутствует неизменяемый снимок"))?;
        if Self::normalized_projection(field, expected)?
            != Self::normalized_projection(field, value)?
        {
            bail!("Проекция {field} изменена вне причинного журнала");
        }
        Ok(())
    }

    fn validate_projections_tx(tx: &Transaction<'_>) -> Result<()> {
        let events = Self::checked_events_tx(tx)?;
        for (table, field, key_column) in [
            ("projects", "project", "id"),
            ("jobs", "job", "id"),
            ("ledger", "ledger", "row_key"),
            ("trends", "trend", "id"),
        ] {
            let mut expected = HashMap::new();
            for event in &events {
                if let Some(value) = event.observed.get(field) {
                    expected.insert(
                        Self::projection_key(field, value)?,
                        Self::normalized_projection(field, value)?,
                    );
                }
            }
            let mut query = tx.prepare(&format!("SELECT {key_column},json FROM {table}"))?;
            let rows = query
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for (key, raw) in rows {
                let value: Value = serde_json::from_str(&raw)?;
                if Self::projection_key(field, &value)? != key
                    || expected.remove(&key).as_ref()
                        != Some(&Self::normalized_projection(field, &value)?)
                {
                    bail!("Проекция {field} не соответствует неизменяемому журналу");
                }
                match field {
                    "project" => {
                        let p: Project = serde_json::from_value(value)?;
                        let indexed: String = tx.query_row(
                            "SELECT updated_at FROM projects WHERE id=?1",
                            [key],
                            |row| row.get(0),
                        )?;
                        if indexed != p.updated_at {
                            bail!("Индекс даты проекта повреждён");
                        }
                    }
                    "job" => {
                        let j: Job = serde_json::from_value(value)?;
                        let indexed: (String, String) = tx.query_row(
                            "SELECT operation_key,created_at FROM jobs WHERE id=?1",
                            [key],
                            |row| Ok((row.get(0)?, row.get(1)?)),
                        )?;
                        if indexed != (j.operation_key, j.created_at) {
                            bail!("Индекс задачи повреждён");
                        }
                    }
                    _ => {}
                }
            }
            // Trend rows are a deliberately bounded cache; other projections
            // must cover every durable entity in the immutable journal.
            if field != "trend" && !expected.is_empty() {
                bail!("В проекции {field} отсутствуют записи из причинного журнала");
            }
        }
        Ok(())
    }

    fn projection_event_tx(
        tx: &Transaction<'_>,
        field: &str,
        key: &str,
        first: bool,
    ) -> Result<CausalEvent> {
        if !["project", "job", "trend"].contains(&field) {
            bail!("Неизвестный тип снимка");
        }
        let order = if first { "ASC" } else { "DESC" };
        let raw: String = tx.query_row(&format!("SELECT json FROM events WHERE json_extract(json,?1)=?2 ORDER BY seq {order} LIMIT 1"),params![format!("$.observed.{field}.id"),key],|row|row.get(0))
            .context("Не найден точный причинный снимок")?;
        let event: CausalEvent = serde_json::from_str(&raw)?;
        let mut sealed = event.clone();
        graph::seal(&mut sealed)?;
        if sealed.hash != event.hash {
            bail!("Причинный снимок повреждён");
        }
        Ok(event)
    }

    fn parent(d: &mut EventDraft, event: &CausalEvent, relation: &str) {
        if !d.parents.iter().any(|parent| parent.0 == event.id) {
            d.parents.push((event.id.clone(), relation.into()));
        }
    }

    fn publishing_running_tx(tx: &Transaction<'_>, pid: &str) -> Result<bool> {
        // Validate the whole job projection before using it as a lock predicate.
        Self::validate_projections_tx(tx)?;
        let count:u64=tx.query_row("SELECT count(*) FROM jobs WHERE json_extract(json,'$.project_id')=?1 AND json_extract(json,'$.kind')='publish' AND json_extract(json,'$.state')='running'",[pid],|row|row.get(0))?;
        Ok(count != 0)
    }
    fn event_space(tx: &Transaction<'_>, d: &EventDraft) -> Result<Value> {
        let mut space = json!({"locus":match d.scope.as_str(){"source"=>"source","external"=>"platform",_=>"studio"},"scope":d.scope,"subject":d.subject});
        let coordinates = space
            .as_object_mut()
            .context("Координаты должны быть объектом")?;
        let job = d.observed.get("job");
        let ledger = d.observed.get("ledger");
        let request = job.and_then(|job| job.get("request"));
        let revision = request
            .and_then(|r| r["revision"].as_u64())
            .or_else(|| d.evidence["revision"].as_u64());
        let project_value = if let Some(project) = d.observed.get("project") {
            Some(project.clone())
        } else if ledger.is_none() && (job.is_some() || revision.is_some()) {
            let raw:Option<String>=tx.query_row("SELECT json FROM events WHERE json_extract(json,'$.observed.project.id')=?1 AND (?2 IS NULL OR json_extract(json,'$.observed.project.revision')=?2) ORDER BY seq DESC LIMIT 1",params![d.subject,revision],|row|row.get(0)).optional()?;
            raw.map(|raw| {
                serde_json::from_str::<CausalEvent>(&raw)
                    .map(|event| event.observed["project"].clone())
            })
            .transpose()?
        } else {
            None
        };
        if let Some(project) = project_value {
            let p: Project = serde_json::from_value(project)?;
            coordinates.insert("project_revision".into(), json!(p.revision));
            coordinates.insert("profile".into(), serde_json::to_value(p.spec.profile)?);
            let (width, height) = p.spec.profile.dimensions();
            coordinates.insert("geometry".into(), json!({"width":width,"height":height}));
            if let Some(artifact) = p.artifact {
                coordinates.insert("artifact_id".into(), json!(artifact.id));
                coordinates.insert("artifact_sha256".into(), json!(artifact.sha256));
            }
        }
        if let Some(request) = request {
            for name in ["artifact_id", "artifact_sha256"] {
                if let Some(value) = request.get(name) {
                    coordinates.insert(name.into(), value.clone());
                }
            }
            if let Some(platform) = request["publish"].get("platform") {
                coordinates.insert("platform".into(), platform.clone());
            }
        }
        if let Some(ledger) = ledger {
            coordinates.insert("platform".into(), ledger["platform"].clone());
            coordinates.insert("day".into(), ledger["date"].clone());
            // Metrics may aggregate several published revisions. Preserve
            // their exact receipt coordinates rather than inventing one.
            let mut artifacts = Vec::new();
            for (parent, relation) in &d.parents {
                if relation != "dependency" {
                    continue;
                }
                let raw: String =
                    tx.query_row("SELECT json FROM events WHERE id=?1", [parent], |row| {
                        row.get(0)
                    })?;
                let event: CausalEvent = serde_json::from_str(&raw)?;
                let request = &event.observed["job"]["request"];
                if request["artifact_sha256"].is_string() {
                    artifacts.push(json!({"event_id":event.id,"project_revision":request["revision"],"artifact_id":request["artifact_id"],"artifact_sha256":request["artifact_sha256"]}));
                }
            }
            if !artifacts.is_empty() {
                coordinates.insert("artifacts".into(), json!(artifacts));
            }
        }
        if let Some(trend) = d.observed.get("trend") {
            coordinates.insert("source".into(), trend["source"].clone());
            coordinates.insert("source_url".into(), trend["url"].clone());
        }
        Ok(space)
    }
    fn append(tx: &Transaction<'_>, d: EventDraft) -> Result<CausalEvent> {
        chrono::DateTime::parse_from_rfc3339(&d.valid_at)
            .context("Время факта должно иметь формат RFC3339")?;
        let space = Self::event_space(tx, &d)?;
        let last: Option<(u64, String)> = tx
            .query_row(
                "SELECT seq,hash FROM events ORDER BY seq DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let (seq, previous_hash) = match last {
            Some((s, h)) => (s + 1, h),
            None => (1, "0".repeat(64)),
        };
        let mut parents = Vec::new();
        for (pid, relation) in d.parents {
            let ph: String = tx
                .query_row("SELECT hash FROM events WHERE id=?1", [&pid], |r| r.get(0))
                .context("Причинный родитель не найден")?;
            parents.push(ParentRef {
                id: pid,
                hash: ph,
                relation,
            });
        }
        let mut e = CausalEvent {
            seq,
            id: id(),
            subject_id: d.subject,
            operation_id: d.operation,
            execution_id: d.execution,
            valid_at: d.valid_at,
            recorded_at: now(),
            from_state: d.from,
            to_state: d.to,
            reason: d.reason,
            claim_level: d.claim,
            spatial_scope: d.scope,
            space,
            confidence: d.confidence,
            parents,
            supersedes: d.supersedes,
            expected: d.expected,
            observed: d.observed,
            evidence: d.evidence,
            previous_hash,
            hash: String::new(),
        };
        graph::seal(&mut e)?;
        tx.execute(
            "INSERT INTO events VALUES(?1,?2,?3,?4,?5)",
            params![
                e.seq,
                e.id,
                e.subject_id,
                e.hash,
                serde_json::to_string(&e)?
            ],
        )?;
        Ok(e)
    }
    pub fn projects(&self) -> Result<Vec<Project>> {
        let mut c = self.conn()?;
        let tx = c.transaction()?;
        Self::validate_projections_tx(&tx)?;
        let mut query = tx.prepare("SELECT json FROM projects ORDER BY updated_at DESC")?;
        let rows = query
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|raw| Ok(serde_json::from_str(&raw)?))
            .collect()
    }
    pub fn jobs(&self) -> Result<Vec<Job>> {
        let mut c = self.conn()?;
        let tx = c.transaction()?;
        Self::validate_projections_tx(&tx)?;
        let mut query = tx.prepare("SELECT json FROM jobs ORDER BY created_at DESC,rowid DESC")?;
        let rows = query
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|raw| Ok(serde_json::from_str(&raw)?))
            .collect()
    }
    pub fn trends(&self) -> Result<Vec<Trend>> {
        let mut c = self.conn()?;
        let tx = c.transaction()?;
        Self::validate_projections_tx(&tx)?;
        let mut query = tx.prepare("SELECT json FROM trends")?;
        let raw = query
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut t: Vec<Trend> = raw
            .into_iter()
            .map(|raw| serde_json::from_str(&raw))
            .collect::<serde_json::Result<_>>()?;
        t.sort_by(|a, b| b.score.total_cmp(&a.score));
        Ok(t)
    }
    pub fn ledger(&self) -> Result<Vec<LedgerRow>> {
        let mut c = self.conn()?;
        let tx = c.transaction()?;
        Self::validate_projections_tx(&tx)?;
        let mut query = tx.prepare("SELECT json FROM ledger ORDER BY row_key DESC")?;
        let raw = query
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        raw.into_iter()
            .map(|raw| Ok(serde_json::from_str(&raw)?))
            .collect()
    }
    pub fn events(&self) -> Result<Vec<CausalEvent>> {
        let mut c = self.conn()?;
        let tx = c.transaction()?;
        Self::checked_events_tx(&tx)
    }
    pub fn project(&self, id: &str) -> Result<Project> {
        let mut c = self.conn()?;
        let tx = c.transaction()?;
        Self::project_tx(&tx, id)
    }
    pub fn job(&self, id: &str) -> Result<Job> {
        let mut c = self.conn()?;
        let tx = c.transaction()?;
        Self::job_tx(&tx, id)
    }

    pub fn create_project(&self, spec: ProjectSpec, generator: &str) -> Result<Project> {
        self.create_project_with_parent(spec, generator, None)
    }
    pub fn create_project_with_parent(
        &self,
        spec: ProjectSpec,
        generator: &str,
        context_event_id: Option<&str>,
    ) -> Result<Project> {
        self.create_project_admitted(spec, generator, context_event_id, None)?
            .context("Обычное создание проекта неожиданно пропущено")
    }

    /// The final budget and exact-topic checks share the same write lock as
    /// the new projection and its causal event. Manual projects also consume
    /// the automation allowance; ordinary manual creation remains explicit.
    pub fn create_pipeline_project(
        &self,
        spec: ProjectSpec,
        generator: &str,
        context_event_id: Option<&str>,
        daily_budget: usize,
        utc_offset_hours: i32,
    ) -> Result<Option<Project>> {
        if daily_budget > 10 || !(-12..=14).contains(&utc_offset_hours) {
            bail!("Некорректный дневной бюджет или часовой пояс конвейера");
        }
        let offset = chrono::FixedOffset::east_opt(utc_offset_hours * 3600)
            .context("Некорректный часовой пояс конвейера")?;
        self.create_project_admitted(
            spec,
            generator,
            context_event_id,
            Some((daily_budget, offset)),
        )
    }

    fn pipeline_admission(
        projects: &[Project],
        topic: &str,
        daily_budget: usize,
        at: chrono::DateTime<chrono::Utc>,
        offset: chrono::FixedOffset,
    ) -> Result<bool> {
        let today = at.with_timezone(&offset).date_naive();
        let mut used = 0usize;
        let mut existing = false;
        for project in projects {
            let created = chrono::DateTime::parse_from_rfc3339(&project.created_at)
                .context("У проекта некорректная дата создания; дневной бюджет не проверен")?;
            if created.with_timezone(&offset).date_naive() == today {
                used = used.saturating_add(1);
            }
            existing |= project.spec.topic.trim() == topic.trim();
        }
        Ok(!existing && used < daily_budget)
    }

    fn create_project_admitted(
        &self,
        spec: ProjectSpec,
        generator: &str,
        context_event_id: Option<&str>,
        admission: Option<(usize, chrono::FixedOffset)>,
    ) -> Result<Option<Project>> {
        spec.validate()?;
        let mut c = self.conn()?;
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        Self::validate_projections_tx(&tx)?;
        // Acquire the transaction before sampling the calendar day. The same
        // instant labels the created project even when a cycle spans midnight.
        let at = chrono::Utc::now();
        if let Some((budget, offset)) = admission {
            let projects: Vec<Project> = {
                let mut query = tx.prepare("SELECT json FROM projects")?;
                let rows = query
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                rows.into_iter()
                    .map(|raw| serde_json::from_str(&raw))
                    .collect::<serde_json::Result<_>>()?
            };
            if !Self::pipeline_admission(&projects, &spec.topic, budget, at, offset)? {
                return Ok(None);
            }
        }
        let timestamp = at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let p = Project {
            id: id(),
            revision: 1,
            created_at: timestamp.clone(),
            updated_at: timestamp,
            status: "draft".into(),
            spec,
            artifact: None,
            error: None,
        };
        let op = id();
        let mut d = EventDraft::new(
            &p.id,
            &op,
            &id(),
            "absent",
            "draft",
            "Создана редактируемая идея ролика",
        );
        d.claim = if generator == "ollama" {
            "MODEL_OUTPUT"
        } else {
            "DERIVED"
        }
        .into();
        d.expected = json!({"goal":"original_video","performance":"unproven"});
        d.observed = json!({"project":p});
        d.evidence = json!({"generator":generator,"source_urls":p.spec.source_urls});
        // Link the exact observed trend batch if the chosen source URL is known.
        let trend_rows: Vec<String> = {
            let mut s = tx.prepare("SELECT json FROM trends")?;
            let result = s
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            result
        };
        for raw in trend_rows {
            let trend: Trend = serde_json::from_str(&raw)?;
            if p.spec.source_urls.contains(&trend.url) && p.spec.topic.trim() == trend.title.trim()
            {
                let e = Self::projection_event_tx(&tx, "trend", &trend.id, false)?;
                Self::parent(&mut d, &e, "dependency");
            }
        }
        if let Some(context_id) = context_event_id {
            let raw: String = tx
                .query_row("SELECT json FROM events WHERE id=?1", [context_id], |row| {
                    row.get(0)
                })
                .context("Событие контекста не найдено")?;
            let context: CausalEvent = serde_json::from_str(&raw)?;
            Self::parent(&mut d, &context, "dependency");
        }
        Self::put_project(&tx, &p)?;
        Self::append(&tx, d)?;
        tx.commit()?;
        Ok(Some(p))
    }
    pub fn update_project(
        &self,
        pid: &str,
        expected_revision: u32,
        spec: ProjectSpec,
    ) -> Result<Project> {
        spec.validate()?;
        let mut c = self.conn()?;
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut p = Self::project_tx(&tx, pid)?;
        if p.revision != expected_revision {
            bail!("Версия устарела. Обновите проект перед сохранением");
        }
        if p.status == "rendering" {
            bail!("Дождитесь завершения рендера перед изменением сценария");
        }
        if Self::publishing_running_tx(&tx, pid)? {
            bail!("Дождитесь результата выполняющейся публикации перед изменением сценария");
        }
        let previous_project = Self::projection_event_tx(&tx, "project", pid, false)?;
        let old = p.revision;
        p.revision = p.revision.checked_add(1).context("Переполнение версии")?;
        p.spec = spec;
        p.artifact = None;
        p.status = "draft".into();
        p.error = None;
        p.updated_at = now();
        let mut d = EventDraft::new(
            pid,
            &id(),
            &id(),
            "script",
            "draft",
            "Изменён сценарий; предыдущий рендер теперь исторический",
        );
        Self::parent(&mut d, &previous_project, "dependency");
        d.claim = "CORRECTION".into();
        d.supersedes = Some(previous_project.id);
        d.expected = json!({"base_revision":old});
        d.observed = json!({"project":p});
        d.evidence = json!({"spec_sha256":digest(&p.spec)?});
        Self::put_project(&tx, &p)?;
        Self::append(&tx, d)?;
        tx.commit()?;
        Ok(p)
    }
    pub fn save_trends(&self, trends: &[Trend]) -> Result<()> {
        let mut c = self.conn()?;
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        Self::validate_projections_tx(&tx)?;
        let operation = id();
        for t in trends {
            tx.execute(
                "INSERT INTO trends VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET json=excluded.json",
                params![t.id, serde_json::to_string(t)?],
            )?;
            let subject = format!("trend:{}", t.id);
            let mut d = EventDraft::new(
                &subject,
                &operation,
                &id(),
                "source",
                "observed",
                "Источник сообщил популярный запрос или ролик",
            );
            d.scope = "source".into();
            d.valid_at = t.fetched_at.clone();
            d.observed = json!({"trend":t});
            d.evidence = json!({"url":t.url,"source":t.source,"kind":t.evidence_kind,"source_published_at":t.published_at,"valid_time_authority":"fetch_observation","ranking":"heuristic_not_causal_proof"});
            Self::append(&tx, d)?;
        }
        // Keep a bounded cache while preserving all past observations in the immutable graph.
        tx.execute("DELETE FROM trends WHERE id NOT IN (SELECT id FROM trends ORDER BY json_extract(json,'$.fetched_at') DESC LIMIT 200)",[])?;
        tx.commit()?;
        Ok(())
    }
    pub fn start_render(&self, pid: &str) -> Result<(Job, bool)> {
        let mut c = self.conn()?;
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut p = Self::project_tx(&tx, pid)?;
        if Self::publishing_running_tx(&tx, pid)? {
            bail!("Дождитесь результата выполняющейся публикации перед новым рендером");
        }
        let project_parent = Self::projection_event_tx(&tx, "project", pid, false)?;
        let key =
            digest(&json!({"kind":"render","project":pid,"revision":p.revision,"spec":p.spec}))?;
        let old: Option<String> = tx
            .query_row(
                "SELECT id FROM jobs WHERE operation_key=?1 ORDER BY rowid DESC LIMIT 1",
                [&key],
                |r| r.get(0),
            )
            .optional()?;
        let mut retry_parent = None;
        if let Some(jid) = old {
            let j = Self::job_tx(&tx, &jid)?;
            if ["queued", "running", "succeeded"].contains(&j.state.as_str())
                && (j.state != "succeeded" || p.artifact.is_some())
            {
                return Ok((j, false));
            }
            retry_parent = Some(Self::projection_event_tx(&tx, "job", &j.id, false)?);
        }
        if p.status == "rendering" {
            bail!("Рендер уже запущен");
        }
        let timestamp = now();
        let j = Job {
            id: id(),
            project_id: pid.into(),
            kind: "render".into(),
            state: "queued".into(),
            progress: 0.0,
            created_at: timestamp.clone(),
            updated_at: timestamp,
            due_at: None,
            request: json!({"revision":p.revision}),
            result: None,
            error: None,
            operation_key: key,
        };
        let from = p.status.clone();
        p.status = "rendering".into();
        p.error = None;
        p.updated_at = now();
        let mut d = EventDraft::new(
            pid,
            &j.operation_key,
            &j.id,
            &from,
            "render_queued",
            "Запрошен рендер конкретной версии сценария",
        );
        Self::parent(&mut d, &project_parent, "dependency");
        if let Some(previous) = retry_parent {
            Self::parent(&mut d, &previous, "recovery");
        }
        d.expected = json!({"revision":p.revision,"dimensions":p.spec.profile.dimensions()});
        d.observed = json!({"project":p,"job":j});
        Self::put_project(&tx, &p)?;
        Self::put_job(&tx, &j)?;
        Self::append(&tx, d)?;
        tx.commit()?;
        Ok((j, true))
    }
    pub fn queue_publish(&self, pid: &str, request: Value, due: Option<String>) -> Result<Job> {
        let mut c = self.conn()?;
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        Self::validate_projections_tx(&tx)?;
        let p = Self::project_tx(&tx, pid)?;
        let a = p.artifact.as_ref().context("Сначала соберите ролик")?;
        if a.revision != p.revision || p.status == "rendering" {
            bail!("Нужен рендер текущей версии");
        }
        let request = json!({"publish":request,"revision":p.revision,"artifact_id":a.id,"artifact_sha256":a.sha256});
        let key = digest(&json!({"kind":"publish","project":pid,"request":request}))?;
        let mut retry_parent = None;
        if let Some(jid) = tx
            .query_row(
                "SELECT id FROM jobs WHERE operation_key=?1 ORDER BY rowid DESC LIMIT 1",
                [&key],
                |r| r.get::<_, String>(0),
            )
            .optional()?
        {
            let prior = Self::job_tx(&tx, &jid)?;
            if [
                "queued",
                "scheduled",
                "running",
                "succeeded",
                "unknown",
                "dry_run",
            ]
            .contains(&prior.state.as_str())
            {
                return Ok(prior);
            }
            if !["failed", "cancelled"].contains(&prior.state.as_str()) {
                bail!("Неизвестное состояние предыдущей публикации");
            }
            retry_parent = Some(Self::projection_event_tx(&tx, "job", &prior.id, false)?);
        }
        let timestamp = now();
        let j = Job {
            id: id(),
            project_id: pid.into(),
            kind: "publish".into(),
            state: if due.is_some() { "scheduled" } else { "queued" }.into(),
            progress: 0.0,
            created_at: timestamp.clone(),
            updated_at: timestamp,
            due_at: due,
            request,
            result: None,
            error: None,
            operation_key: key,
        };
        let mut d = EventDraft::new(
            pid,
            &j.operation_key,
            &j.id,
            "ready",
            &j.state,
            "Публикация привязана к версии, файлу и выбранной площадке",
        );
        Self::parent(
            &mut d,
            &Self::projection_event_tx(&tx, "project", pid, false)?,
            "dependency",
        );
        if let Some(prior) = retry_parent {
            Self::parent(&mut d, &prior, "recovery");
        }
        d.scope = if j.request["publish"]["dry_run"] == true
            || j.request["publish"]["platform"] == "export"
        {
            "local"
        } else {
            "external"
        }
        .into();
        d.claim = "INTENT".into();
        d.expected = j.request.clone();
        d.observed = json!({"job":j});
        d.evidence = json!({"authority":"local_user_request","default":"dry_run"});
        Self::put_job(&tx, &j)?;
        Self::append(&tx, d)?;
        tx.commit()?;
        Ok(j)
    }
    pub fn begin_job(&self, jid: &str) -> Result<Job> {
        let mut c = self.conn()?;
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        Self::validate_projections_tx(&tx)?;
        let mut j = Self::job_tx(&tx, jid)?;
        let previous_job = Self::projection_event_tx(&tx, "job", jid, false)?;
        if !["queued", "scheduled"].contains(&j.state.as_str()) {
            bail!("Задача уже исполняется или завершена");
        }
        if let Some(due) = &j.due_at {
            if chrono::DateTime::parse_from_rfc3339(due)? > chrono::Utc::now() {
                bail!("Время публикации ещё не наступило");
            }
        }
        let p = Self::project_tx(&tx, &j.project_id)?;
        if j.request["revision"].as_u64() != Some(u64::from(p.revision)) {
            bail!("Запрос относится к устаревшей версии сценария");
        }
        if j.kind == "publish"
            && p.artifact.as_ref().map(|a| a.sha256.as_str())
                != j.request["artifact_sha256"].as_str()
        {
            bail!("Рендер изменился после постановки в очередь");
        }
        let from = j.state.clone();
        j.state = "running".into();
        j.progress = 0.05;
        j.updated_at = now();
        let mut d = EventDraft::new(
            &j.project_id,
            &j.operation_key,
            &j.id,
            &from,
            "running",
            "Проверены версия, причинные входы и время запуска",
        );
        Self::parent(&mut d, &previous_job, "dependency");
        d.expected = j.request.clone();
        d.observed = json!({"job":j});
        d.evidence = json!({"admission":"revision_and_schedule_checked"});
        Self::put_job(&tx, &j)?;
        Self::append(&tx, d)?;
        tx.commit()?;
        Ok(j)
    }
    pub fn finish_render(&self, jid: &str, result: Result<Artifact>) -> Result<Job> {
        let mut c = self.conn()?;
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut j = Self::job_tx(&tx, jid)?;
        if j.kind != "render" {
            bail!("Задача не является рендером");
        }
        let previous_job = Self::projection_event_tx(&tx, "job", jid, false)?;
        let mut p = Self::project_tx(&tx, &j.project_id)?;
        if !["running", "queued"].contains(&j.state.as_str()) {
            bail!("Рендер уже завершён");
        }
        match result {
            Ok(a) if a.revision == p.revision => {
                p.artifact = Some(a.clone());
                p.status = "ready".into();
                p.error = None;
                j.state = "succeeded".into();
                j.result = Some(json!({"artifact":a}));
                j.progress = 1.0;
            }
            Ok(_) => {
                j.state = "failed".into();
                j.error = Some("Рендер устарел относительно сценария".into());
                p.status = "draft".into();
            }
            Err(e) => {
                let err = format!("{e:#}");
                j.state = "failed".into();
                j.error = Some(err.clone());
                p.status = "failed".into();
                p.error = Some(err);
            }
        }
        j.updated_at = now();
        p.updated_at = now();
        let mut d = EventDraft::new(
            &p.id,
            &j.operation_key,
            &j.id,
            "rendering",
            &j.state,
            "Результат рендера проверен отдельно от запуска инструмента",
        );
        Self::parent(&mut d, &previous_job, "dependency");
        d.expected = json!({"revision":p.revision,"playable_video":true});
        d.observed = json!({"project":p,"job":j});
        d.evidence = j.result.clone().unwrap_or_else(|| json!({"error":j.error}));
        Self::put_project(&tx, &p)?;
        Self::put_job(&tx, &j)?;
        Self::append(&tx, d)?;
        tx.commit()?;
        Ok(j)
    }
    pub fn finish_job(
        &self,
        jid: &str,
        state: &str,
        result: Option<Value>,
        error: Option<String>,
    ) -> Result<Job> {
        if !["succeeded", "dry_run", "failed", "unknown", "cancelled"].contains(&state) {
            bail!("Недопустимое конечное состояние");
        }
        let mut c = self.conn()?;
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut j = Self::job_tx(&tx, jid)?;
        let previous_job = Self::projection_event_tx(&tx, "job", jid, false)?;
        if ["succeeded", "dry_run", "failed", "unknown", "cancelled"].contains(&j.state.as_str()) {
            return Ok(j);
        }
        if state == "cancelled" && j.state == "running" {
            bail!("Выполняющаяся публикация уже могла быть принята площадкой");
        }
        let from = j.state.clone();
        j.state = state.into();
        j.result = result;
        j.error = error;
        j.progress = if ["succeeded", "dry_run"].contains(&state) {
            1.0
        } else {
            j.progress
        };
        j.updated_at = now();
        let mut d = EventDraft::new(
            &j.project_id,
            &j.operation_key,
            &j.id,
            &from,
            state,
            "Зафиксирован ответ площадки или отсутствие подтверждённого результата",
        );
        Self::parent(&mut d, &previous_job, "dependency");
        d.scope = if j.kind == "publish"
            && j.request["publish"]["dry_run"] == false
            && j.request["publish"]["platform"] != "export"
        {
            "external"
        } else {
            "local"
        }
        .into();
        d.expected = j.request.clone();
        d.observed = json!({"job":j});
        d.evidence = json!({"receipt":j.result,"error":j.error,"automatic_retry":false});
        Self::put_job(&tx, &j)?;
        Self::append(&tx, d)?;
        tx.commit()?;
        Ok(j)
    }
    pub fn recover_interrupted(&self) -> Result<()> {
        for j in self.jobs()? {
            if j.kind == "render" && ["queued", "running"].contains(&j.state.as_str()) {
                self.finish_render(
                    &j.id,
                    Err(anyhow::anyhow!(
                        "Рендер прерван перезапуском студии. Можно запустить заново"
                    )),
                )?;
            }
            if j.kind == "publish" && j.state == "running" {
                self.finish_job(&j.id,"unknown",None,Some("Студия перезапущена во время публикации. Проверьте площадку перед повторной отправкой".into()))?;
            }
        }
        Ok(())
    }
    /// Imports atomically and supersedes prior interpretations, preserving old rows in events.
    pub fn save_ledger(&self, rows: &[LedgerRow]) -> Result<usize> {
        for r in rows {
            r.validate()?;
        }
        let mut c = self.conn()?;
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        Self::validate_projections_tx(&tx)?;
        let op = id();
        let mut publication_parents: HashMap<(String, String), Vec<CausalEvent>> = HashMap::new();
        for r in rows {
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM projects WHERE id=?1)",
                [&r.project_id],
                |row| row.get(0),
            )?;
            if !exists {
                bail!("Проект не найден");
            }
            let old: Option<String> = tx
                .query_row("SELECT json FROM ledger WHERE row_key=?1", [r.key()], |x| {
                    x.get(0)
                })
                .optional()?;
            if old.as_deref() == Some(&serde_json::to_string(r)?) {
                continue;
            }
            let prior:Option<String>=tx.query_row("SELECT id FROM events WHERE subject_id=?1 AND json_extract(json,'$.evidence.ledger_key')=?2 ORDER BY seq DESC LIMIT 1",params![r.project_id,r.key()],|x|x.get(0)).optional()?;
            let mut d = EventDraft::new(
                &r.project_id,
                &op,
                &id(),
                "published_or_experiment",
                "metrics",
                "Получены дневные просмотры, расходы и сведения о доходе",
            );
            let group = (r.project_id.clone(), r.platform.clone());
            if !publication_parents.contains_key(&group) {
                let mut query=tx.prepare("SELECT json FROM events WHERE json_extract(json,'$.observed.job.project_id')=?1 AND json_extract(json,'$.observed.job.kind')='publish' AND json_extract(json,'$.observed.job.state')='succeeded' AND json_extract(json,'$.observed.job.result.platform')=?2 ORDER BY seq")?;
                let raw = query
                    .query_map(params![r.project_id, r.platform], |row| {
                        row.get::<_, String>(0)
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let mut parents = raw
                    .into_iter()
                    .map(|raw| serde_json::from_str::<CausalEvent>(&raw))
                    .collect::<serde_json::Result<Vec<_>>>()?;
                if parents.is_empty() {
                    parents.push(Self::projection_event_tx(
                        &tx,
                        "project",
                        &r.project_id,
                        true,
                    )?);
                }
                publication_parents.insert(group.clone(), parents);
            }
            for event in &publication_parents[&group] {
                Self::parent(&mut d, event, "dependency");
            }
            d.supersedes = prior.clone();
            if let Some(prior) = prior {
                d.parents.push((prior, "supersedes".into()));
            }
            d.claim = if old.is_some() {
                "CORRECTION"
            } else {
                "OBSERVATION"
            }
            .into();
            d.valid_at = format!("{}T23:59:59Z", r.date);
            d.expected = json!({"forecast_is_assumption":true});
            d.observed = json!({"ledger":r,"forecast_minor":r.forecast()?});
            d.evidence = json!({"ledger_key":r.key(),"source":r.source,"actual_payout":r.actual_revenue_minor.is_some(),"api_income_is_estimate":r.api_estimated_revenue_minor.is_some()});
            tx.execute("INSERT INTO ledger VALUES(?1,?2) ON CONFLICT(row_key) DO UPDATE SET json=excluded.json",params![r.key(),serde_json::to_string(r)?])?;
            Self::append(&tx, d)?;
        }
        // Validate aggregate overflow before making any import visible.
        let all: Vec<LedgerRow> = {
            let mut s = tx.prepare("SELECT json FROM ledger")?;
            let raw = s
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            raw.into_iter()
                .map(|r| serde_json::from_str(&r))
                .collect::<serde_json::Result<_>>()?
        };
        crate::money::totals(&all)?;
        tx.commit()?;
        Ok(rows.len())
    }
    pub fn hypothesis(&self, pid: &str, reason: &str) -> Result<CausalEvent> {
        if reason.trim().is_empty() || reason.chars().count() > 1000 {
            bail!("Гипотеза должна содержать 1–1000 символов");
        }
        let mut c = self.conn()?;
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let p = Self::project_tx(&tx, pid)?;
        let mut d = EventDraft::new(pid, &id(), &id(), &p.status, "experiment_proposed", reason);
        Self::parent(
            &mut d,
            &Self::projection_event_tx(&tx, "project", pid, false)?,
            "hypothesis",
        );
        d.claim = "HYPOTHESIS".into();
        d.expected =
            json!({"evaluate":"views_and_retention","review_after":"after_first_publication"});
        d.observed = json!({"interpretation_only":true});
        d.evidence = json!({"independent_causal_proof":false,"revision":p.revision});
        let e = Self::append(&tx, d)?;
        tx.commit()?;
        Ok(e)
    }

    /// Current local operator configuration is an intent, never a historical
    /// memory authorization. Recording it cannot trigger a side effect.
    pub fn record_intent(
        &self,
        subject: &str,
        reason: &str,
        expected: Value,
    ) -> Result<CausalEvent> {
        if subject.trim().is_empty() || reason.trim().is_empty() {
            bail!("Нужны субъект и причина намерения");
        }
        let mut c = self.conn()?;
        let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        Self::validate_projections_tx(&tx)?;
        let mut d = EventDraft::new(
            subject,
            &id(),
            &id(),
            "configuration",
            "intent_recorded",
            reason,
        );
        d.claim = "INTENT".into();
        d.expected = expected;
        d.evidence = json!({"authority":"local_operator_config","memory_is_authority":false});
        let event = Self::append(&tx, d)?;
        tx.commit()?;
        Ok(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn spec() -> ProjectSpec {
        serde_json::from_value(json!({"title":"Тест","scenes":[{"title":"Шаг","text":"текст","narration":"","duration_s":1}],"voice":"none"})).unwrap()
    }
    fn ready(store: &Store) -> Project {
        let project = store.create_project(spec(), "manual").unwrap();
        let (job, _) = store.start_render(&project.id).unwrap();
        store.begin_job(&job.id).unwrap();
        store
            .finish_render(
                &job.id,
                Ok(Artifact {
                    id: id(),
                    path: "renders/test.mp4".into(),
                    width: 1080,
                    height: 1920,
                    duration_s: 1.0,
                    revision: project.revision,
                    sha256: "1".repeat(64),
                    created_at: now(),
                }),
            )
            .unwrap();
        store.project(&project.id).unwrap()
    }
    fn ledger_row(pid: &str) -> LedgerRow {
        serde_json::from_value(json!({"project_id":pid,"platform":"youtube","date":"2026-10-03","views":1000,"currency":"RUB","rpm_minor":5000,"monetized":true})).unwrap()
    }
    fn export_request() -> Value {
        json!({"platform":"export","dry_run":true})
    }
    #[test]
    fn revision_compare_and_swap_preserves_history() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let p = s.create_project(spec(), "manual").unwrap();
        let q = s.update_project(&p.id, 1, spec()).unwrap();
        assert_eq!(q.revision, 2);
        assert!(s.update_project(&p.id, 1, spec()).is_err());
        assert_eq!(s.events().unwrap().len(), 2);
        assert!(graph::verify(&s.events().unwrap()).valid);
    }
    #[test]
    fn events_are_immutable_in_sqlite() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        s.create_project(spec(), "manual").unwrap();
        assert!(s
            .conn()
            .unwrap()
            .execute("UPDATE events SET hash='x'", [])
            .is_err());
        assert!(s.conn().unwrap().execute("DELETE FROM events", []).is_err());
    }
    #[test]
    fn render_retry_is_new_execution_same_logical_operation() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let p = s.create_project(spec(), "manual").unwrap();
        let (a, spawn) = s.start_render(&p.id).unwrap();
        assert!(spawn);
        assert!(!s.start_render(&p.id).unwrap().1);
        s.finish_render(&a.id, Err(anyhow::anyhow!("failure")))
            .unwrap();
        let (b, spawn) = s.start_render(&p.id).unwrap();
        assert!(spawn);
        assert_ne!(a.id, b.id);
        assert_eq!(a.operation_key, b.operation_key);
    }
    #[test]
    fn csv_import_is_atomic_and_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let p = s.create_project(spec(), "manual").unwrap();
        let mut row:LedgerRow=serde_json::from_value(json!({"project_id":p.id,"platform":"youtube","date":"2026-10-03","views":1000,"currency":"RUB","rpm_minor":5000,"monetized":true})).unwrap();
        s.save_ledger(&[row.clone()]).unwrap();
        let count = s.events().unwrap().len();
        s.save_ledger(&[row.clone()]).unwrap();
        assert_eq!(s.events().unwrap().len(), count);
        row.project_id = id();
        assert!(s.save_ledger(&[row]).is_err());
        assert_eq!(s.ledger().unwrap().len(), 1);
    }

    #[test]
    fn project_projection_tamper_blocks_reads_mutations_and_reopening() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let project = store.create_project(spec(), "manual").unwrap();
        let mut forged = serde_json::to_value(&project).unwrap();
        forged["spec"]["title"] = json!("forged title");
        store
            .conn()
            .unwrap()
            .execute(
                "UPDATE projects SET json=?1 WHERE id=?2",
                params![forged.to_string(), project.id],
            )
            .unwrap();
        assert!(graph::verify(&store.events().unwrap()).valid);
        assert!(store.project(&project.id).is_err());
        assert!(store.projects().is_err());
        assert!(store.start_render(&project.id).is_err());
        drop(store);
        assert!(Store::open(dir.path()).is_err());
    }

    #[test]
    fn forged_job_cannot_launder_a_different_publication_intent() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let project = ready(&store);
        let job = store
            .queue_publish(&project.id, export_request(), None)
            .unwrap();
        let mut forged = serde_json::to_value(&job).unwrap();
        forged["request"]["publish"]["platform"] = json!("youtube");
        forged["request"]["publish"]["dry_run"] = json!(false);
        store
            .conn()
            .unwrap()
            .execute(
                "UPDATE jobs SET json=?1 WHERE id=?2",
                params![forged.to_string(), job.id],
            )
            .unwrap();
        let count = store.events().unwrap().len();
        assert!(store.job(&job.id).is_err());
        assert!(store.jobs().is_err());
        assert!(store.begin_job(&job.id).is_err());
        assert!(store
            .queue_publish(&project.id, export_request(), None)
            .is_err());
        assert_eq!(store.events().unwrap().len(), count);
        drop(store);
        assert!(Store::open(dir.path()).is_err());
    }

    #[test]
    fn ledger_projection_tamper_cannot_be_hidden_by_an_import() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let project = store.create_project(spec(), "manual").unwrap();
        let row = ledger_row(&project.id);
        store.save_ledger(std::slice::from_ref(&row)).unwrap();
        let mut forged = serde_json::to_value(&row).unwrap();
        forged["actual_revenue_minor"] = json!(999999);
        store
            .conn()
            .unwrap()
            .execute(
                "UPDATE ledger SET json=?1 WHERE row_key=?2",
                params![forged.to_string(), row.key()],
            )
            .unwrap();
        assert!(store.ledger().is_err());
        assert!(store.save_ledger(&[row]).is_err());
    }

    #[test]
    fn project_dependencies_ignore_unrelated_hypotheses_and_metrics() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let project = store.create_project(spec(), "manual").unwrap();
        let identity = store.events().unwrap()[0].clone();
        let hypothesis = store
            .hypothesis(&project.id, "Short hook might improve retention")
            .unwrap();
        assert_eq!(hypothesis.parents[0].id, identity.id);
        assert_eq!(hypothesis.parents[0].relation, "hypothesis");
        store.save_ledger(&[ledger_row(&project.id)]).unwrap();
        let metrics = store.events().unwrap().last().unwrap().clone();
        assert_eq!(metrics.parents[0].id, identity.id);
        let (job, _) = store.start_render(&project.id).unwrap();
        let queued = store.events().unwrap().last().unwrap().clone();
        assert_eq!(queued.parents.len(), 1);
        assert_eq!(queued.parents[0].id, identity.id);
        assert!(!queued
            .parents
            .iter()
            .any(|p| p.id == hypothesis.id || p.id == metrics.id));
        store.begin_job(&job.id).unwrap();
        let running = store.events().unwrap().last().unwrap().clone();
        assert_eq!(running.parents[0].id, queued.id);
    }

    #[test]
    fn script_correction_supersedes_a_project_snapshot_after_metrics() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let project = store.create_project(spec(), "manual").unwrap();
        let identity = store.events().unwrap()[0].id.clone();
        store.save_ledger(&[ledger_row(&project.id)]).unwrap();
        let metrics = store.events().unwrap().last().unwrap().id.clone();
        store.update_project(&project.id, 1, spec()).unwrap();
        let correction = store.events().unwrap().last().unwrap().clone();
        assert_eq!(correction.supersedes.as_deref(), Some(identity.as_str()));
        assert_ne!(correction.supersedes.as_deref(), Some(metrics.as_str()));
        assert_eq!(store.ledger().unwrap().len(), 1);
    }

    #[test]
    fn publication_retries_get_new_execution_and_explicit_recovery_lineage() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let project = ready(&store);
        let first = store
            .queue_publish(&project.id, export_request(), None)
            .unwrap();
        store
            .finish_job(&first.id, "cancelled", None, None)
            .unwrap();
        let cancelled = store.events().unwrap().last().unwrap().id.clone();
        let retry = store
            .queue_publish(&project.id, export_request(), None)
            .unwrap();
        assert_ne!(retry.id, first.id);
        assert_eq!(retry.operation_key, first.operation_key);
        assert!(store
            .events()
            .unwrap()
            .last()
            .unwrap()
            .parents
            .iter()
            .any(|parent| parent.id == cancelled && parent.relation == "recovery"));
        store.begin_job(&retry.id).unwrap();
        store
            .finish_job(&retry.id, "failed", None, Some("known rejection".into()))
            .unwrap();
        let third = store
            .queue_publish(&project.id, export_request(), None)
            .unwrap();
        assert_ne!(third.id, retry.id);
        store.begin_job(&third.id).unwrap();
        store
            .finish_job(&third.id, "unknown", None, Some("no receipt".into()))
            .unwrap();
        assert_eq!(
            store
                .queue_publish(&project.id, export_request(), None)
                .unwrap()
                .id,
            third.id
        );
    }

    #[test]
    fn root_lock_survives_clones_and_prevents_recovery_of_a_live_owner() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let project = ready(&store);
        let job = store
            .queue_publish(&project.id, export_request(), None)
            .unwrap();
        store.begin_job(&job.id).unwrap();
        let worker = store.clone();
        drop(store);
        assert!(Store::open(dir.path()).is_err());
        assert_eq!(worker.job(&job.id).unwrap().state, "running");
        drop(worker);
        let restarted = Store::open(dir.path()).unwrap();
        restarted.recover_interrupted().unwrap();
        assert_eq!(restarted.job(&job.id).unwrap().state, "unknown");
    }

    #[test]
    fn running_publication_freezes_script_and_new_render() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let project = ready(&store);
        let job = store
            .queue_publish(&project.id, export_request(), None)
            .unwrap();
        store.begin_job(&job.id).unwrap();
        assert!(store
            .update_project(&project.id, project.revision, spec())
            .is_err());
        assert!(store.start_render(&project.id).is_err());
        store
            .finish_job(&job.id, "failed", None, Some("known rejection".into()))
            .unwrap();
        assert!(store
            .update_project(&project.id, project.revision, spec())
            .is_ok());
    }

    #[test]
    fn trend_observations_are_roots_and_shared_urls_do_not_match_other_topics() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let trend = |name: &str| Trend {
            id: id(),
            title: name.into(),
            url: "https://trends.google.com/trending/rss?geo=RU".into(),
            source: "google_rss".into(),
            published_at: now(),
            fetched_at: now(),
            volume: None,
            score: 1.0,
            evidence_kind: "source_report".into(),
        };
        let selected = trend("Chosen topic");
        let other = trend("Unrelated topic");
        store.save_trends(&[selected.clone(), other]).unwrap();
        let observations = store.events().unwrap();
        assert!(observations.iter().all(|event| event.parents.is_empty()));
        let mut project = spec();
        project.topic = selected.title;
        project.source_urls = vec![selected.url];
        store.create_project(project, "manual").unwrap();
        let creation = store.events().unwrap().last().unwrap().clone();
        assert_eq!(creation.parents.len(), 1);
        assert_eq!(creation.parents[0].id, observations[0].id);
    }

    #[test]
    fn explicit_operator_intent_is_context_and_space_contains_only_known_coordinates() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let intent = store
            .record_intent(
                "pipeline:test",
                "Configured this cycle",
                json!({"mode":"local_export"}),
            )
            .unwrap();
        assert_eq!(intent.claim_level, "INTENT");
        assert!(intent.parents.is_empty());
        assert_eq!(intent.evidence["authority"], "local_operator_config");
        let project = store
            .create_project_with_parent(spec(), "manual", Some(&intent.id))
            .unwrap();
        let creation = store.events().unwrap().last().unwrap().clone();
        assert_eq!(creation.parents[0].id, intent.id);
        assert_eq!(creation.space["subject"], project.id);
        assert_eq!(creation.space["project_revision"], 1);
        assert_eq!(
            creation.space["geometry"],
            json!({"width":1080,"height":1920})
        );
        assert!(creation.space.get("account").is_none());
    }

    #[test]
    fn pipeline_admission_uses_offset_midnight_and_counts_every_project_state() {
        let sample = |topic: &str, created: &str, status: &str| {
            let mut spec = spec();
            spec.topic = topic.into();
            Project {
                id: id(),
                revision: 1,
                created_at: created.into(),
                updated_at: created.into(),
                status: status.into(),
                spec,
                artifact: None,
                error: None,
            }
        };
        let at = chrono::DateTime::parse_from_rfc3339("2026-10-03T21:30:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let offset = chrono::FixedOffset::east_opt(3 * 3600).unwrap();
        let projects = vec![
            sample("Yesterday", "2026-10-03T20:59:59Z", "ready"),
            sample("Today draft", "2026-10-03T21:00:00Z", "draft"),
            sample("Today failed", "2026-10-04T00:20:00+03:00", "failed"),
        ];
        assert!(!Store::pipeline_admission(&projects, "Fresh", 2, at, offset).unwrap());
        assert!(Store::pipeline_admission(&projects, "Fresh", 3, at, offset).unwrap());
        // Exact trimmed identity applies to historical projects too.
        assert!(!Store::pipeline_admission(&projects, " Yesterday ", 3, at, offset).unwrap());
        assert!(Store::pipeline_admission(&projects, "yesterday", 3, at, offset).unwrap());
        assert!(Store::pipeline_admission(
            &[sample("Bad", "unknown", "draft")],
            "Fresh",
            2,
            at,
            offset
        )
        .is_err());
    }

    #[test]
    fn skipped_pipeline_admission_creates_neither_project_nor_causal_event() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut manual = spec();
        manual.topic = "Shared topic".into();
        let project = store.create_project(manual, "manual").unwrap();
        let (job, _) = store.start_render(&project.id).unwrap();
        store
            .finish_render(&job.id, Err(anyhow::anyhow!("known render failure")))
            .unwrap();
        let events = store.events().unwrap().len();
        let mut candidate = spec();
        candidate.topic = "New topic".into();
        assert!(store
            .create_pipeline_project(candidate, "manual", None, 1, 3)
            .unwrap()
            .is_none());
        let mut duplicate = spec();
        duplicate.topic = " Shared topic ".into();
        assert!(store
            .create_pipeline_project(duplicate, "manual", None, 10, 3)
            .unwrap()
            .is_none());
        assert_eq!(store.projects().unwrap().len(), 1);
        assert_eq!(store.events().unwrap().len(), events);
    }

    #[test]
    fn concurrent_pipeline_admission_spends_the_last_slot_once() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let threads: Vec<_> = (0..2)
            .map(|index| {
                let store = store.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let mut candidate = spec();
                    candidate.topic = format!("Concurrent topic {index}");
                    barrier.wait();
                    store
                        .create_pipeline_project(candidate, "manual", None, 1, 3)
                        .unwrap()
                        .is_some()
                })
            })
            .collect();
        let admitted = threads
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .filter(|admitted| *admitted)
            .count();
        assert_eq!(admitted, 1);
        assert_eq!(store.projects().unwrap().len(), 1);
        assert_eq!(store.events().unwrap().len(), 1);
    }
}
