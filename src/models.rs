use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
pub fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}
pub fn default_language() -> String {
    "ru".into()
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Profile {
    #[default]
    Shorts,
    Reels,
    Tiktok,
    Vk,
    Landscape,
    Square,
}
impl Profile {
    pub fn dimensions(self) -> (u32, u32) {
        match self {
            Self::Landscape => (1920, 1080),
            Self::Square => (1080, 1080),
            _ => (1080, 1920),
        }
    }
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Voice {
    None,
    #[default]
    Espeak,
    Piper,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scene {
    pub title: String,
    pub text: String,
    pub narration: String,
    pub duration_s: f64,
    #[serde(default)]
    pub asset: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectSpec {
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub topic: String,
    #[serde(default)]
    pub profile: Profile,
    #[serde(default = "default_language")]
    pub language: String,
    #[serde(default)]
    pub voice: Voice,
    pub scenes: Vec<Scene>,
    #[serde(default)]
    pub music_asset: Option<String>,
    #[serde(default)]
    pub source_urls: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}
impl ProjectSpec {
    pub fn validate(&self) -> Result<()> {
        if self.title.trim().is_empty() || self.title.chars().count() > 100 {
            bail!("Заголовок: от 1 до 100 символов");
        }
        if self.description.len() > 12000 || self.topic.len() > 1000 {
            bail!("Слишком длинное описание");
        }
        if !["ru", "en"].contains(&self.language.as_str()) {
            bail!("Язык: ru или en");
        }
        if self.scenes.is_empty() || self.scenes.len() > 20 {
            bail!("Нужно от 1 до 20 сцен");
        }
        let mut total = 0.0;
        for s in &self.scenes {
            if !s.duration_s.is_finite() || !(1.0..=30.0).contains(&s.duration_s) {
                bail!("Сцена должна длиться 1–30 секунд");
            }
            if s.title.chars().count() > 100
                || s.text.chars().count() > 400
                || s.narration.chars().count() > 700
            {
                bail!("Текст сцены слишком длинный");
            }
            if s.title.trim().is_empty() {
                bail!("Сцене нужен заголовок");
            }
            total += s.duration_s;
            if let Some(a) = &s.asset {
                validate_asset(a)?;
            }
        }
        if total > 180.0 {
            bail!("В первой версии ролик ограничен 180 секундами");
        }
        if let Some(a) = &self.music_asset {
            validate_asset(a)?;
        }
        if self.tags.len() > 20 || self.tags.iter().any(|t| t.len() > 100) {
            bail!("Не более 20 коротких тегов");
        }
        if self.source_urls.len() > 20 {
            bail!("Не более 20 источников");
        }
        for url in &self.source_urls {
            if url.len() > 2000 {
                bail!("Ссылка на источник слишком длинная");
            }
            let u = reqwest::Url::parse(url)?;
            if u.scheme() != "https" {
                bail!("Ссылки на источники должны начинаться с https://");
            }
        }
        Ok(())
    }
}
pub fn validate_asset(path: &str) -> Result<()> {
    let p = std::path::Path::new(path);
    if p.is_absolute()
        || !path.starts_with("assets/")
        || path.contains('\\')
        || path
            .split('/')
            .any(|s| s.is_empty() || s == "." || s == "..")
        || p.components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        bail!("Материалы должны находиться в папке assets студии");
    }
    Ok(())
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artifact {
    pub id: String,
    pub path: String,
    pub width: u32,
    pub height: u32,
    pub duration_s: f64,
    pub revision: u32,
    pub sha256: String,
    pub created_at: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub revision: u32,
    pub created_at: String,
    pub updated_at: String,
    pub status: String,
    pub spec: ProjectSpec,
    pub artifact: Option<Artifact>,
    pub error: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trend {
    pub id: String,
    pub title: String,
    pub url: String,
    pub source: String,
    pub published_at: String,
    pub fetched_at: String,
    pub volume: Option<u64>,
    pub score: f64,
    pub evidence_kind: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub project_id: String,
    pub kind: String,
    pub state: String,
    pub progress: f32,
    pub created_at: String,
    pub updated_at: String,
    pub due_at: Option<String>,
    pub request: Value,
    pub result: Option<Value>,
    pub error: Option<String>,
    pub operation_key: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParentRef {
    pub id: String,
    pub hash: String,
    pub relation: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CausalEvent {
    pub seq: u64,
    pub id: String,
    pub subject_id: String,
    pub operation_id: String,
    pub execution_id: String,
    pub valid_at: String,
    pub recorded_at: String,
    pub from_state: String,
    pub to_state: String,
    pub reason: String,
    pub claim_level: String,
    pub spatial_scope: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub space: Value,
    pub confidence: Option<f64>,
    pub parents: Vec<ParentRef>,
    pub supersedes: Option<String>,
    pub expected: Value,
    pub observed: Value,
    pub evidence: Value,
    pub previous_hash: String,
    pub hash: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn paths_cannot_escape_assets() {
        for path in ["/etc/passwd", "assets/../secret", "assets/./x", "foo/a.png"] {
            assert!(validate_asset(path).is_err(), "{path}");
        }
        assert!(validate_asset("assets/a.png").is_ok());
    }
}
