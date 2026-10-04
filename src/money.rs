use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

fn eligible_default() -> u32 {
    10000
}
fn default_source() -> String {
    "manual".into()
}

/// One *daily increment*, not lifetime view counters. All monetary values are minor units.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LedgerRow {
    pub project_id: String,
    pub platform: String,
    pub date: String,
    pub views: u64,
    pub currency: String,
    #[serde(default)]
    pub rpm_minor: Option<i64>,
    #[serde(default)]
    pub monetized: bool,
    #[serde(default)]
    pub actual_revenue_minor: Option<i64>,
    #[serde(default)]
    pub api_estimated_revenue_minor: Option<i64>,
    #[serde(default)]
    pub cost_minor: i64,
    #[serde(default = "eligible_default")]
    pub eligible_bps: u32,
    #[serde(default = "default_source")]
    pub source: String,
}
impl LedgerRow {
    pub fn validate(&self) -> Result<()> {
        if uuid::Uuid::parse_str(&self.project_id).is_err() {
            bail!("Неверный ID проекта");
        }
        if !["youtube", "telegram", "tiktok", "reels", "vk", "other"]
            .contains(&self.platform.as_str())
        {
            bail!("Неизвестная площадка");
        }
        let d = chrono::NaiveDate::parse_from_str(&self.date, "%Y-%m-%d")
            .context("Дата должна иметь формат YYYY-MM-DD")?;
        if d.format("%Y-%m-%d").to_string() != self.date {
            bail!("Нестрогий формат даты");
        }
        if !["RUB", "USD", "EUR"].contains(&self.currency.as_str()) {
            bail!("Валюта: RUB, USD или EUR");
        }
        if self.views > 9_000_000_000_000 || self.eligible_bps > 10000 {
            bail!("Некорректные просмотры или доля монетизируемых просмотров");
        }
        for amount in [
            self.rpm_minor,
            self.actual_revenue_minor,
            self.api_estimated_revenue_minor,
            Some(self.cost_minor),
        ]
        .into_iter()
        .flatten()
        {
            if !(0..=9_000_000_000_000).contains(&amount) {
                bail!("Денежная сумма должна быть неотрицательной и в минимальных единицах валюты");
            }
        }
        if !["manual", "csv", "youtube_analytics"].contains(&self.source.as_str()) {
            bail!("Неизвестный источник статистики");
        }
        Ok(())
    }
    pub fn key(&self) -> String {
        format!(
            "{}:{}:{}:{}",
            self.project_id, self.platform, self.date, self.currency
        )
    }
    pub fn forecast(&self) -> Result<Option<i64>> {
        if let Some(v) = self.api_estimated_revenue_minor {
            return Ok(Some(v));
        }
        if !self.monetized {
            return Ok(Some(0));
        }
        let Some(rpm) = self.rpm_minor else {
            return Ok(None);
        };
        // Integer arithmetic, rounded once, no float or exchange-rate assumptions.
        let n = i128::from(self.views)
            .checked_mul(i128::from(self.eligible_bps))
            .and_then(|v| v.checked_mul(i128::from(rpm)))
            .context("Переполнение прогноза")?;
        let amount = n.checked_add(5_000_000).context("Переполнение прогноза")? / 10_000_000;
        Ok(Some(
            i64::try_from(amount).context("Переполнение прогноза")?,
        ))
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct MoneyTotal {
    pub currency: String,
    pub views: u64,
    pub forecast_minor_known: i64,
    pub forecast_complete: bool,
    pub unknown_forecast_rows: usize,
    pub actual_revenue_minor: i64,
    pub actual_rows: usize,
    pub cost_minor: i64,
    pub forecast_profit_minor: Option<i64>,
    pub actual_profit_minor: i64,
}
pub fn totals(rows: &[LedgerRow]) -> Result<Vec<MoneyTotal>> {
    let mut groups: BTreeMap<String, MoneyTotal> = BTreeMap::new();
    for r in rows {
        r.validate()?;
        let t = groups.entry(r.currency.clone()).or_insert(MoneyTotal {
            currency: r.currency.clone(),
            views: 0,
            forecast_minor_known: 0,
            forecast_complete: true,
            unknown_forecast_rows: 0,
            actual_revenue_minor: 0,
            actual_rows: 0,
            cost_minor: 0,
            forecast_profit_minor: None,
            actual_profit_minor: 0,
        });
        t.views = t
            .views
            .checked_add(r.views)
            .context("Переполнение просмотров")?;
        t.cost_minor = t
            .cost_minor
            .checked_add(r.cost_minor)
            .context("Переполнение затрат")?;
        if let Some(f) = r.forecast()? {
            t.forecast_minor_known = t
                .forecast_minor_known
                .checked_add(f)
                .context("Переполнение дохода")?;
        } else {
            t.forecast_complete = false;
            t.unknown_forecast_rows += 1;
        }
        if let Some(a) = r.actual_revenue_minor {
            t.actual_revenue_minor = t
                .actual_revenue_minor
                .checked_add(a)
                .context("Переполнение дохода")?;
            t.actual_rows += 1;
        }
    }
    for t in groups.values_mut() {
        t.forecast_profit_minor = if t.forecast_complete {
            Some(t.forecast_minor_known - t.cost_minor)
        } else {
            None
        };
        t.actual_profit_minor = t.actual_revenue_minor - t.cost_minor;
    }
    Ok(groups.into_values().collect())
}
pub fn parse_csv(bytes: &[u8]) -> Result<Vec<LedgerRow>> {
    let mut reader = csv::ReaderBuilder::new()
        .trim(csv::Trim::All)
        .from_reader(bytes);
    let mut rows = Vec::new();
    for row in reader.deserialize::<LedgerRow>() {
        let mut row = row.context("CSV: неверные столбцы или значения")?;
        row.source = "csv".into();
        row.validate()?;
        rows.push(row);
        if rows.len() > 10000 {
            bail!("CSV: не более 10000 строк за импорт");
        }
    }
    if rows.is_empty() {
        bail!("CSV пуст");
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn row() -> LedgerRow {
        LedgerRow {
            project_id: uuid::Uuid::new_v4().to_string(),
            platform: "youtube".into(),
            date: "2026-10-03".into(),
            views: 1250,
            currency: "RUB".into(),
            rpm_minor: Some(5000),
            monetized: true,
            actual_revenue_minor: None,
            api_estimated_revenue_minor: None,
            cost_minor: 1000,
            eligible_bps: 8000,
            source: "manual".into(),
        }
    }
    #[test]
    fn forecast_uses_eligible_views_once() {
        assert_eq!(row().forecast().unwrap(), Some(5000));
    }
    #[test]
    fn not_eligible_means_zero_not_promised_income() {
        let mut r = row();
        r.monetized = false;
        assert_eq!(r.forecast().unwrap(), Some(0));
    }
    #[test]
    fn no_rpm_means_unknown() {
        let mut r = row();
        r.rpm_minor = None;
        let t = totals(&[r]).unwrap();
        assert!(!t[0].forecast_complete);
        assert_eq!(t[0].forecast_profit_minor, None);
    }
    #[test]
    fn never_adds_distinct_currencies() {
        let a = row();
        let mut b = a.clone();
        b.currency = "USD".into();
        assert_eq!(totals(&[a, b]).unwrap().len(), 2);
    }
    #[test]
    fn api_estimate_is_not_actual_payout() {
        let mut r = row();
        r.api_estimated_revenue_minor = Some(7200);
        let t = totals(&[r]).unwrap();
        assert_eq!(t[0].forecast_minor_known, 7200);
        assert_eq!(t[0].actual_rows, 0);
    }
    #[test]
    fn rounding_and_overflow_are_explicit() {
        let mut r = row();
        r.views = 1;
        r.eligible_bps = 10000;
        r.rpm_minor = Some(500);
        assert_eq!(r.forecast().unwrap(), Some(1));
        r.views = u64::MAX;
        r.rpm_minor = Some(i64::MAX);
        assert!(r.forecast().is_err());
    }
}
