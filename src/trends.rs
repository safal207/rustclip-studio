//! Public trend signals. Scores rank signals; they do not predict reach or revenue.
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use quick_xml::{events::Event, Reader};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::time::Duration;

use crate::models::{now, Trend};

const MAX_RESPONSE: usize = 2 * 1024 * 1024;

pub async fn fetch(source: &str, region: &str) -> Result<Vec<Trend>> {
    let region = normalized_region(region)?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent("RustClipStudio/0.1 (+public trend feeds)")
        .redirect(reqwest::redirect::Policy::limited(3))
        .build()?;
    let fetched_at = now();
    let reference_time = Utc::now();
    match source {
        "google" => {
            let response = client
                .get("https://trends.google.com/trending/rss")
                .query(&[("geo", &region)])
                .send()
                .await
                .map_err(|e| e.without_url())
                .context("Не удалось получить публичный RSS Google Trends")?;
            let body = bounded_body(response, "Google Trends").await?;
            parse_google_rss(&body, &fetched_at, reference_time)
        }
        "youtube" => {
            let key = std::env::var("YOUTUBE_API_KEY")
                .ok().filter(|k| !k.trim().is_empty())
                .context("Для YouTube mostPopular нужен YOUTUBE_API_KEY из бесплатной квоты YouTube Data API; Google RSS работает без ключа")?;
            let response = client
                .get("https://www.googleapis.com/youtube/v3/videos")
                .query(&[
                    ("part", "snippet,statistics"),
                    ("chart", "mostPopular"),
                    ("regionCode", region.as_str()),
                    ("maxResults", "25"),
                    ("key", key.as_str()),
                ])
                .send()
                .await
                .map_err(|e| e.without_url())
                .context("Не удалось получить YouTube mostPopular")?;
            let body = bounded_body(response, "YouTube Data API").await?;
            parse_youtube(&body, &fetched_at, reference_time)
        }
        _ => bail!("Источник трендов: google или youtube"),
    }
}

fn normalized_region(region: &str) -> Result<String> {
    if region.len() != 2 || !region.bytes().all(|b| b.is_ascii_alphabetic()) {
        bail!("Регион должен состоять из двух латинских букв, например US или TR");
    }
    Ok(region.to_ascii_uppercase())
}

async fn bounded_body(mut response: reqwest::Response, source: &str) -> Result<String> {
    if !response.status().is_success() {
        // Do not return request URLs: YouTube keys are query parameters.
        bail!(
            "{source}: HTTP {}. Проверьте регион, доступность сервиса и квоту API",
            response.status().as_u16()
        );
    }
    if response
        .content_length()
        .is_some_and(|n| n > MAX_RESPONSE as u64)
    {
        bail!("Ответ {source} превышает 2 МиБ");
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| e.without_url())? {
        if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE {
            bail!("Ответ {source} превышает 2 МиБ");
        }
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes).context("Ответ источника должен быть UTF-8")
}

#[derive(Default)]
struct RssItem {
    title: String,
    url: String,
    published: String,
    traffic: String,
}

fn parse_google_rss(
    xml: &str,
    fetched_at: &str,
    reference_time: DateTime<Utc>,
) -> Result<Vec<Trend>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut item: Option<RssItem> = None;
    let mut tags = Vec::<String>::new();
    let mut result = Vec::new();
    let mut saw_rss = false;
    loop {
        match reader
            .read_event()
            .context("Некорректный RSS Google Trends")?
        {
            Event::Start(e) => {
                let tag = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                if tag == "rss" {
                    saw_rss = true;
                }
                if tag == "item" {
                    item = Some(RssItem::default());
                }
                tags.push(tag);
            }
            Event::Text(e) => {
                let raw = std::str::from_utf8(e.as_ref()).context("RSS содержит не UTF-8 текст")?;
                let value =
                    quick_xml::escape::unescape(raw).context("Некорректное экранирование в RSS")?;
                append_item_text(&mut item, &tags, &value);
            }
            Event::CData(e) => {
                let value =
                    std::str::from_utf8(e.as_ref()).context("RSS содержит не UTF-8 CDATA")?;
                append_item_text(&mut item, &tags, value);
            }
            Event::End(e) => {
                if e.local_name().as_ref() == b"item" {
                    if let Some(i) = item.take().filter(|i| !i.title.trim().is_empty()) {
                        let title = i.title.trim().to_owned();
                        let url = validated_https(&i.url).unwrap_or_else(|| {
                            let mut u = reqwest::Url::parse("https://trends.google.com/trending")
                                .expect("constant URL");
                            u.query_pairs_mut().append_pair("q", &title);
                            u.into()
                        });
                        let published_at = DateTime::parse_from_rfc2822(i.published.trim())
                            .ok()
                            .map(|d| d.with_timezone(&Utc).to_rfc3339())
                            .unwrap_or_default();
                        let volume = parse_traffic(&i.traffic);
                        result.push(Trend {
                            id: signal_id("google", &url, &title),
                            title,
                            url,
                            source: "google".into(),
                            fetched_at: fetched_at.into(),
                            score: signal_score(volume, &published_at, reference_time),
                            published_at,
                            volume,
                            evidence_kind: "popular_search".into(),
                        });
                    }
                }
                tags.pop();
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if !saw_rss {
        bail!("Источник вернул не RSS; тренды не сохранены");
    }
    rank_and_deduplicate(&mut result);
    Ok(result)
}

fn append_item_text(item: &mut Option<RssItem>, tags: &[String], value: &str) {
    let Some(i) = item.as_mut() else {
        return;
    };
    let Some(last) = tags.last().map(String::as_str) else {
        return;
    };
    // Only direct item fields; a nested news headline is not the search query.
    if tags.get(tags.len().saturating_sub(2)).map(String::as_str) != Some("item") {
        return;
    }
    match last {
        "title" => i.title.push_str(value),
        "link" => i.url.push_str(value),
        "pubDate" => i.published.push_str(value),
        "approx_traffic" => i.traffic.push_str(value),
        _ => {}
    }
}

fn parse_traffic(raw: &str) -> Option<u64> {
    let value = raw
        .trim()
        .trim_end_matches('+')
        .replace([',', ' ', '\u{a0}'], "")
        .to_ascii_uppercase();
    let (number, multiplier) = if let Some(v) = value.strip_suffix('K') {
        (v, 1_000.0)
    } else if let Some(v) = value.strip_suffix('M') {
        (v, 1_000_000.0)
    } else {
        (value.as_str(), 1.0)
    };
    let parsed = number.parse::<f64>().ok()? * multiplier;
    if !parsed.is_finite() || parsed < 0.0 || parsed >= u64::MAX as f64 {
        return None;
    }
    Some(parsed.round() as u64)
}

#[derive(Deserialize)]
struct YoutubeResponse {
    items: Vec<YoutubeItem>,
}
#[derive(Deserialize)]
struct YoutubeItem {
    id: String,
    snippet: YoutubeSnippet,
    #[serde(default)]
    statistics: YoutubeStatistics,
}
#[derive(Deserialize)]
struct YoutubeSnippet {
    title: String,
    #[serde(rename = "publishedAt", default)]
    published_at: String,
}
#[derive(Deserialize, Default)]
struct YoutubeStatistics {
    #[serde(rename = "viewCount", default)]
    view_count: Option<String>,
}

fn parse_youtube(
    json: &str,
    fetched_at: &str,
    reference_time: DateTime<Utc>,
) -> Result<Vec<Trend>> {
    let parsed: YoutubeResponse =
        serde_json::from_str(json).context("Некорректный ответ YouTube Data API")?;
    let mut result = Vec::new();
    for item in parsed.items {
        if item.id.is_empty()
            || !item
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            bail!("YouTube вернул некорректный идентификатор видео");
        }
        if item.snippet.title.trim().is_empty() {
            continue;
        }
        let title = item.snippet.title;
        let url = format!("https://www.youtube.com/watch?v={}", item.id);
        let published_at = DateTime::parse_from_rfc3339(&item.snippet.published_at)
            .ok()
            .map(|d| d.with_timezone(&Utc).to_rfc3339())
            .unwrap_or_default();
        let volume = item.statistics.view_count.and_then(|v| v.parse().ok());
        result.push(Trend {
            id: signal_id("youtube", &url, &title),
            title,
            url,
            source: "youtube".into(),
            fetched_at: fetched_at.into(),
            score: signal_score(volume, &published_at, reference_time),
            published_at,
            volume,
            evidence_kind: "most_popular_video".into(),
        });
    }
    rank_and_deduplicate(&mut result);
    Ok(result)
}

fn validated_https(value: &str) -> Option<String> {
    let url = reqwest::Url::parse(value.trim()).ok()?;
    (url.scheme() == "https" && url.username().is_empty() && url.password().is_none())
        .then(|| url.into())
}

fn signal_id(source: &str, url: &str, title: &str) -> String {
    let mut digest = Sha256::new();
    for value in [source, url, title] {
        digest.update(value.as_bytes());
        digest.update([0]);
    }
    hex::encode(digest.finalize())
}

/// A 0..100 ranking heuristic: 65% log volume and 35% recency (72-hour decay).
/// Missing publication dates use neutral recency; volumes have source-specific meaning.
fn signal_score(volume: Option<u64>, published_at: &str, reference_time: DateTime<Utc>) -> f64 {
    let volume_component = volume
        .map(|v| (1.0 + v as f64).log10() / 7.0)
        .unwrap_or(0.0)
        .clamp(0.0, 1.0);
    let recency = DateTime::parse_from_rfc3339(published_at)
        .ok()
        .map(|d| {
            let hours = (reference_time - d.with_timezone(&Utc))
                .num_seconds()
                .max(0) as f64
                / 3600.0;
            (-hours / 72.0).exp()
        })
        .unwrap_or(0.5);
    ((65.0 * volume_component + 35.0 * recency) * 100.0).round() / 100.0
}

fn rank_and_deduplicate(result: &mut Vec<Trend>) {
    result.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
    let mut seen = std::collections::HashSet::new();
    result.retain(|t| seen.insert(t.id.clone()));
    result.truncate(50);
}

#[cfg(test)]
mod tests {
    use super::*;
    fn at() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-03T22:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn namespaced_rss_preserves_query_and_evidence() {
        let xml = r#"<?xml version="1.0"?><rss xmlns:ht="https://trends.google.com/trending/rss"><channel><item><title>Cats &amp; dogs</title><ht:approx_traffic>20K+</ht:approx_traffic><link>https://trends.google.com/trending/rss?geo=US</link><pubDate>Sat, 3 Oct 2026 14:40:00 -0700</pubDate><ht:news_item><ht:news_item_title>Must not replace query</ht:news_item_title></ht:news_item></item><item><title><![CDATA[Original <title>]]></title><ht:approx_traffic>1,000+</ht:approx_traffic></item></channel></rss>"#;
        let result = parse_google_rss(xml, "2026-10-03T22:00:00Z", at()).unwrap();
        let query = result.iter().find(|t| t.title == "Cats & dogs").unwrap();
        assert_eq!(query.volume, Some(20_000));
        assert_eq!(query.evidence_kind, "popular_search");
        assert_eq!(query.published_at, "2026-10-03T21:40:00+00:00");
        assert_eq!(query.id, signal_id("google", &query.url, &query.title));
        assert!(result
            .iter()
            .any(|t| t.title == "Original <title>" && t.published_at.is_empty()));
    }

    #[test]
    fn approximate_traffic_handles_feed_formats() {
        for (value, expected) in [
            ("1000+", Some(1000)),
            ("1,000+", Some(1000)),
            ("1.5M+", Some(1_500_000)),
            ("50K+", Some(50_000)),
            ("bad", None),
            ("-1", None),
            ("NaN", None),
        ] {
            assert_eq!(parse_traffic(value), expected, "{value}");
        }
    }

    #[test]
    fn youtube_maps_views_without_claiming_search_interest() {
        let json = r#"{"items":[{"id":"aB_c-d12345","snippet":{"title":"A real video","publishedAt":"2026-10-03T20:00:00Z"},"statistics":{"viewCount":"123456"}},{"id":"another","snippet":{"title":"Without views"}}]}"#;
        let result = parse_youtube(json, "2026-10-03T22:00:00Z", at()).unwrap();
        let video = result.iter().find(|t| t.volume.is_some()).unwrap();
        assert_eq!(video.volume, Some(123456));
        assert_eq!(video.evidence_kind, "most_popular_video");
        assert_eq!(video.url, "https://www.youtube.com/watch?v=aB_c-d12345");
        assert!(result.iter().all(|t| (0.0..=100.0).contains(&t.score)));
    }

    #[test]
    fn scores_decay_and_invalid_input_is_rejected() {
        assert!(
            signal_score(Some(1000), "2026-10-03T21:00:00Z", at())
                > signal_score(Some(1000), "2026-09-01T00:00:00Z", at())
        );
        assert_eq!(normalized_region("tr").unwrap(), "TR");
        for region in ["", "USA", "12", "РФ", " US"] {
            assert!(normalized_region(region).is_err());
        }
        assert!(parse_google_rss("<html>no RSS</html>", "", at()).is_err());
        assert!(parse_youtube(
            r#"{"items":[{"id":"../escape","snippet":{"title":"x"}}]}"#,
            "",
            at()
        )
        .is_err());
    }
}
