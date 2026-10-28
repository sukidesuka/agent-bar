use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

pub fn timestamp(value: &str) -> Option<i64> {
    time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
        .ok()
        .map(|t| t.unix_timestamp())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct QuotaWindow {
    pub used_percent: f64,
    pub window_minutes: i64,
    pub resets_at: i64,
}

impl QuotaWindow {
    pub fn remaining_percent(&self) -> f64 {
        100.0 - self.used_percent.clamp(0.0, 100.0)
    }

    pub fn label(&self) -> String {
        match self.window_minutes {
            10080 => "每周".into(),
            n if n >= 1440 && n % 1440 == 0 => format!("{}天", n / 1440),
            n if n >= 60 && n % 60 == 0 => format!("{}小时", n / 60),
            n => format!("{n}分钟"),
        }
    }
    pub fn expired(&self, at: i64) -> bool {
        self.resets_at <= at
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Usage {
    pub primary: Option<QuotaWindow>,
    pub secondary: Option<QuotaWindow>,
    pub observed_at: i64,
    pub source: String,
}

impl Usage {
    pub fn parse(value: &Value, at: i64, source: &str) -> Option<Self> {
        // Select the Codex bucket, never an arbitrary model's quota bucket.
        let value = if let Some(buckets) = value.get("rateLimitsByLimitId") {
            buckets.get("codex").or_else(|| value.get("rateLimits"))?
        } else {
            value.get("rateLimits").unwrap_or(value)
        };
        if value
            .get("limit_id")
            .or_else(|| value.get("limitId"))
            .and_then(Value::as_str)
            .is_some_and(|id| id != "codex")
        {
            return None;
        }
        let read = |name: &str| -> Option<QuotaWindow> {
            let w = value.get(name)?;
            let used = w
                .get("used_percent")
                .or_else(|| w.get("usedPercent"))?
                .as_f64()?;
            let minutes = w
                .get("window_minutes")
                .or_else(|| w.get("windowDurationMins"))?
                .as_i64()?;
            let resets = w.get("resets_at").or_else(|| w.get("resetsAt"))?.as_i64()?;
            if !used.is_finite() || minutes <= 0 || resets <= 0 {
                return None;
            }
            Some(QuotaWindow {
                used_percent: used.clamp(0.0, 100.0),
                window_minutes: minutes,
                resets_at: resets,
            })
        };
        let usage = Self {
            primary: read("primary"),
            secondary: read("secondary"),
            observed_at: at,
            source: source.into(),
        };
        (usage.primary.is_some() || usage.secondary.is_some()).then_some(usage)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum TaskStatus {
    Active,
    Waiting,
    Idle,
    Complete,
    Interrupted,
    Unknown,
}
impl TaskStatus {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Active => "执行中",
            Self::Waiting => "等待确认",
            Self::Idle => "空闲",
            Self::Complete => "已完成",
            Self::Interrupted => "已中断",
            Self::Unknown => "状态未知",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub status: TaskStatus,
    pub updated_at: i64,
    pub started_at: Option<i64>,
    pub inferred: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub usage: Option<Usage>,
    pub tasks: Vec<Task>,
    pub connection: String,
    pub codex_home: String,
    pub error: Option<String>,
    pub scanned_at: i64,
}

pub fn relative_age(seconds: i64) -> String {
    let seconds = seconds.max(0);
    if seconds < 60 {
        format!("{seconds}秒")
    } else if seconds < 3600 {
        format!("{}分钟", seconds / 60)
    } else if seconds < 86400 {
        format!("{}小时", seconds / 3600)
    } else {
        format!("{}天", seconds / 86400)
    }
}

/// Use whole days for longer waits, otherwise round up to a half hour.
pub fn reset_countdown(seconds: i64) -> String {
    if seconds <= 0 {
        return "待刷新".into();
    }
    if seconds >= 86400 {
        return format!("{}d", seconds / 86400);
    }
    let half_hours = seconds / 1800 + i64::from(seconds % 1800 != 0);
    let hours = half_hours / 2;
    match (hours, half_hours % 2) {
        (0, _) => "30m".into(),
        (_, 0) => format!("{hours}h"),
        _ => format!("{hours}h 30m"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn accepts_both_protocol_and_log_names_and_preserves_unknown_windows() {
        let log = json!({"limit_id":"codex","primary":{"used_percent":23.0,"window_minutes":300,"resets_at":2000},"secondary":null});
        let rpc = json!({"rateLimitsByLimitId":{"other":{"primary":{}},"codex":{"primary":{"usedPercent":23,"windowDurationMins":300,"resetsAt":2000}}}});
        let a = Usage::parse(&log, 1000, "test").unwrap();
        let b = Usage::parse(&rpc, 1000, "test").unwrap();
        assert_eq!(a, b);
        let primary = a.primary.unwrap();
        assert_eq!(primary.label(), "5小时");
        assert_eq!(primary.remaining_percent(), 77.0);
        assert!(a.secondary.is_none());
        assert!(
            Usage::parse(
                &json!({"limit_id":"other","primary":log["primary"]}),
                1000,
                "test"
            )
            .is_none()
        );
        assert!(Usage::parse(&json!({"primary":null}), 1000, "test").is_none());
    }
    #[test]
    fn expired_quota_is_not_a_fresh_zero() {
        let q = QuotaWindow {
            used_percent: 85.0,
            window_minutes: 10080,
            resets_at: 10,
        };
        assert!(q.expired(11));
        assert_eq!(q.used_percent, 85.0);
        assert_eq!(timestamp("2026-10-28T00:00:00Z"), Some(1793145600));
    }

    #[test]
    fn countdown_uses_half_hours_below_one_day_and_whole_days_above() {
        for (seconds, expected) in [
            (-1, "待刷新"),
            (0, "待刷新"),
            (1, "30m"),
            (1799, "30m"),
            (1800, "30m"),
            (1801, "1h"),
            (3600, "1h"),
            (3601, "1h 30m"),
            (12600, "3h 30m"),
            (86399, "24h"),
            (86400, "1d"),
            (86401, "1d"),
            (176401, "2d"),
            (432000, "5d"),
            (518399, "5d"),
        ] {
            assert_eq!(
                reset_countdown(seconds),
                expected,
                "remaining seconds: {seconds}"
            );
        }
    }
}
