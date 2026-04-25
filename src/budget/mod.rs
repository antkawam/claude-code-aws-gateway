pub mod notifications;

use chrono::{DateTime, Datelike, Duration, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tokio::sync::RwLock;

/// A single threshold rule in a budget policy.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PolicyRule {
    pub at_percent: u32,
    pub action: PolicyAction,
    /// RPM throttle for `shape` action.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shaped_rpm: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum PolicyAction {
    Notify,
    Shape,
    Block,
}

/// Result of evaluating a budget policy against current spend.
#[derive(Debug, Clone, PartialEq)]
pub enum BudgetDecision {
    Allow,
    Notify { threshold_percent: u32 },
    Shape { threshold_percent: u32, rpm: u32 },
    Block { threshold_percent: u32 },
}

/// Budget period types.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetPeriod {
    Daily,
    Weekly,
    Monthly,
}

impl BudgetPeriod {
    pub fn parse(s: &str) -> Self {
        match s {
            "daily" => Self::Daily,
            "weekly" => Self::Weekly,
            _ => Self::Monthly,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Daily => "daily",
            Self::Weekly => "weekly",
            Self::Monthly => "monthly",
        }
    }

    /// Get the start of the current period (UTC).
    pub fn period_start(&self) -> DateTime<Utc> {
        let now = Utc::now();
        match self {
            Self::Daily => Utc
                .with_ymd_and_hms(now.year(), now.month(), now.day(), 0, 0, 0)
                .unwrap(),
            Self::Weekly => {
                let days_since_monday = now.weekday().num_days_from_monday();
                let monday = now - Duration::days(days_since_monday as i64);
                Utc.with_ymd_and_hms(monday.year(), monday.month(), monday.day(), 0, 0, 0)
                    .unwrap()
            }
            Self::Monthly => Utc
                .with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
                .unwrap(),
        }
    }

    /// Get the start of the next period (UTC) -- when the budget resets.
    pub fn period_next_start(&self) -> DateTime<Utc> {
        let now = Utc::now();
        match self {
            Self::Daily => {
                let tomorrow = now + Duration::days(1);
                Utc.with_ymd_and_hms(tomorrow.year(), tomorrow.month(), tomorrow.day(), 0, 0, 0)
                    .unwrap()
            }
            Self::Weekly => {
                let days_until_next_monday =
                    7 - now.weekday().num_days_from_monday() as i64;
                let next_monday = now + Duration::days(days_until_next_monday);
                Utc.with_ymd_and_hms(
                    next_monday.year(),
                    next_monday.month(),
                    next_monday.day(),
                    0,
                    0,
                    0,
                )
                .unwrap()
            }
            Self::Monthly => {
                let (year, month) = if now.month() == 12 {
                    (now.year() + 1, 1)
                } else {
                    (now.year(), now.month() + 1)
                };
                Utc.with_ymd_and_hms(year, month, 1, 0, 0, 0).unwrap()
            }
        }
    }

    /// SQL date_trunc arg.
    pub fn trunc_arg(&self) -> &'static str {
        match self {
            Self::Daily => "day",
            Self::Weekly => "week",
            Self::Monthly => "month",
        }
    }
}

/// Preset budget policies.
pub fn preset_standard() -> Vec<PolicyRule> {
    vec![
        PolicyRule {
            at_percent: 80,
            action: PolicyAction::Notify,
            shaped_rpm: None,
        },
        PolicyRule {
            at_percent: 100,
            action: PolicyAction::Block,
            shaped_rpm: None,
        },
    ]
}

pub fn preset_soft() -> Vec<PolicyRule> {
    vec![
        PolicyRule {
            at_percent: 80,
            action: PolicyAction::Notify,
            shaped_rpm: None,
        },
        PolicyRule {
            at_percent: 100,
            action: PolicyAction::Notify,
            shaped_rpm: None,
        },
        PolicyRule {
            at_percent: 150,
            action: PolicyAction::Block,
            shaped_rpm: None,
        },
    ]
}

pub fn preset_shaped() -> Vec<PolicyRule> {
    vec![
        PolicyRule {
            at_percent: 80,
            action: PolicyAction::Notify,
            shaped_rpm: None,
        },
        PolicyRule {
            at_percent: 100,
            action: PolicyAction::Shape,
            shaped_rpm: Some(5),
        },
        PolicyRule {
            at_percent: 150,
            action: PolicyAction::Block,
            shaped_rpm: None,
        },
    ]
}

/// Validate a budget policy: rules must be ascending by at_percent, only one block (must be last), max 5 rules.
pub fn validate_policy(rules: &[PolicyRule]) -> Result<(), String> {
    if rules.is_empty() {
        return Err("Policy must have at least one rule".into());
    }
    if rules.len() > 5 {
        return Err("Policy can have at most 5 rules".into());
    }

    let mut prev_pct = 0;
    let mut has_block = false;
    for (i, rule) in rules.iter().enumerate() {
        if rule.at_percent == 0 {
            return Err("at_percent must be > 0".into());
        }
        if rule.at_percent < prev_pct {
            return Err("Rules must be in ascending order by at_percent".into());
        }
        if has_block {
            return Err("Block rule must be the last rule".into());
        }
        if rule.action == PolicyAction::Block {
            has_block = true;
        }
        if rule.action == PolicyAction::Shape && rule.shaped_rpm.is_none() {
            return Err(format!(
                "Rule {} (at {}%): shape action requires shaped_rpm",
                i, rule.at_percent
            ));
        }
        prev_pct = rule.at_percent;
    }
    Ok(())
}

/// Evaluate a budget policy given current spend and limit.
/// Returns the most restrictive triggered action.
pub fn evaluate(rules: &[PolicyRule], spend_usd: f64, limit_usd: f64) -> BudgetDecision {
    if limit_usd <= 0.0 || rules.is_empty() {
        return BudgetDecision::Allow;
    }

    let percent = (spend_usd / limit_usd) * 100.0;
    let mut decision = BudgetDecision::Allow;

    for rule in rules {
        if percent >= rule.at_percent as f64 {
            decision = match &rule.action {
                PolicyAction::Notify => BudgetDecision::Notify {
                    threshold_percent: rule.at_percent,
                },
                PolicyAction::Shape => BudgetDecision::Shape {
                    threshold_percent: rule.at_percent,
                    rpm: rule.shaped_rpm.unwrap_or(5),
                },
                PolicyAction::Block => BudgetDecision::Block {
                    threshold_percent: rule.at_percent,
                },
            };
        }
    }

    decision
}

/// Combine two budget decisions, returning the most restrictive.
pub fn most_restrictive(a: BudgetDecision, b: BudgetDecision) -> BudgetDecision {
    fn severity(d: &BudgetDecision) -> u8 {
        match d {
            BudgetDecision::Allow => 0,
            BudgetDecision::Notify { .. } => 1,
            BudgetDecision::Shape { .. } => 2,
            BudgetDecision::Block { .. } => 3,
        }
    }
    if severity(&b) > severity(&a) { b } else { a }
}

/// Cached spend entry with TTL.
#[derive(Clone)]
struct CachedSpend {
    amount: f64,
    fetched_at: std::time::Instant,
}

/// In-memory cache for user/team spend to avoid DB pressure.
/// Entries expire after `ttl` seconds.
pub struct BudgetSpendCache {
    user_cache: RwLock<HashMap<String, CachedSpend>>,
    team_cache: RwLock<HashMap<uuid::Uuid, CachedSpend>>,
    ttl: std::time::Duration,
}

impl BudgetSpendCache {
    pub fn new(ttl_secs: u64) -> Self {
        Self {
            user_cache: RwLock::new(HashMap::new()),
            team_cache: RwLock::new(HashMap::new()),
            ttl: std::time::Duration::from_secs(ttl_secs),
        }
    }

    pub async fn get_user_spend(&self, identity: &str) -> Option<f64> {
        let cache = self.user_cache.read().await;
        if let Some(entry) = cache.get(identity)
            && entry.fetched_at.elapsed() < self.ttl
        {
            return Some(entry.amount);
        }
        None
    }

    pub async fn set_user_spend(&self, identity: &str, amount: f64) {
        let mut cache = self.user_cache.write().await;
        cache.insert(
            identity.to_string(),
            CachedSpend {
                amount,
                fetched_at: std::time::Instant::now(),
            },
        );
    }

    pub async fn get_team_spend(&self, team_id: uuid::Uuid) -> Option<f64> {
        let cache = self.team_cache.read().await;
        if let Some(entry) = cache.get(&team_id)
            && entry.fetched_at.elapsed() < self.ttl
        {
            return Some(entry.amount);
        }
        None
    }

    pub async fn set_team_spend(&self, team_id: uuid::Uuid, amount: f64) {
        let mut cache = self.team_cache.write().await;
        cache.insert(
            team_id,
            CachedSpend {
                amount,
                fetched_at: std::time::Instant::now(),
            },
        );
    }

    /// Evict expired entries.
    pub async fn cleanup(&self) {
        let mut user_cache = self.user_cache.write().await;
        user_cache.retain(|_, v| v.fetched_at.elapsed() < self.ttl * 2);
        let mut team_cache = self.team_cache.write().await;
        team_cache.retain(|_, v| v.fetched_at.elapsed() < self.ttl * 2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Timelike, Weekday};

    #[test]
    fn test_evaluate_allow() {
        let rules = preset_standard();
        let decision = evaluate(&rules, 50.0, 100.0);
        assert_eq!(decision, BudgetDecision::Allow);
    }

    #[test]
    fn test_evaluate_notify_at_80() {
        let rules = preset_standard();
        let decision = evaluate(&rules, 80.0, 100.0);
        assert_eq!(
            decision,
            BudgetDecision::Notify {
                threshold_percent: 80
            }
        );
    }

    #[test]
    fn test_evaluate_block_at_100() {
        let rules = preset_standard();
        let decision = evaluate(&rules, 100.0, 100.0);
        assert_eq!(
            decision,
            BudgetDecision::Block {
                threshold_percent: 100
            }
        );
    }

    #[test]
    fn test_evaluate_shape() {
        let rules = preset_shaped();
        let decision = evaluate(&rules, 110.0, 100.0);
        assert_eq!(
            decision,
            BudgetDecision::Shape {
                threshold_percent: 100,
                rpm: 5
            }
        );
    }

    #[test]
    fn test_evaluate_shaped_block() {
        let rules = preset_shaped();
        let decision = evaluate(&rules, 160.0, 100.0);
        assert_eq!(
            decision,
            BudgetDecision::Block {
                threshold_percent: 150
            }
        );
    }

    #[test]
    fn test_evaluate_soft_overrun() {
        let rules = preset_soft();
        // At 120%, only 100% notify fires (not yet at 150% block)
        let decision = evaluate(&rules, 120.0, 100.0);
        assert_eq!(
            decision,
            BudgetDecision::Notify {
                threshold_percent: 100
            }
        );
    }

    #[test]
    fn test_validate_policy_ok() {
        assert!(validate_policy(&preset_standard()).is_ok());
        assert!(validate_policy(&preset_soft()).is_ok());
        assert!(validate_policy(&preset_shaped()).is_ok());
    }

    #[test]
    fn test_validate_empty() {
        assert!(validate_policy(&[]).is_err());
    }

    #[test]
    fn test_validate_too_many() {
        let rules: Vec<PolicyRule> = (1..=6)
            .map(|i| PolicyRule {
                at_percent: i * 10,
                action: PolicyAction::Notify,
                shaped_rpm: None,
            })
            .collect();
        assert!(validate_policy(&rules).is_err());
    }

    #[test]
    fn test_validate_wrong_order() {
        let rules = vec![
            PolicyRule {
                at_percent: 100,
                action: PolicyAction::Notify,
                shaped_rpm: None,
            },
            PolicyRule {
                at_percent: 80,
                action: PolicyAction::Block,
                shaped_rpm: None,
            },
        ];
        assert!(validate_policy(&rules).is_err());
    }

    #[test]
    fn test_validate_block_not_last() {
        let rules = vec![
            PolicyRule {
                at_percent: 80,
                action: PolicyAction::Block,
                shaped_rpm: None,
            },
            PolicyRule {
                at_percent: 100,
                action: PolicyAction::Notify,
                shaped_rpm: None,
            },
        ];
        assert!(validate_policy(&rules).is_err());
    }

    #[test]
    fn test_validate_shape_without_rpm() {
        let rules = vec![PolicyRule {
            at_percent: 100,
            action: PolicyAction::Shape,
            shaped_rpm: None,
        }];
        assert!(validate_policy(&rules).is_err());
    }

    #[test]
    fn test_most_restrictive() {
        let allow = BudgetDecision::Allow;
        let notify = BudgetDecision::Notify {
            threshold_percent: 80,
        };
        let block = BudgetDecision::Block {
            threshold_percent: 100,
        };

        assert_eq!(most_restrictive(allow.clone(), notify.clone()), notify);
        assert_eq!(most_restrictive(notify.clone(), block.clone()), block);
        assert_eq!(most_restrictive(block.clone(), allow), block);
    }

    #[test]
    fn test_period_start_monthly() {
        let start = BudgetPeriod::Monthly.period_start();
        assert_eq!(start.day(), 1);
        assert_eq!(start.hour(), 0);
    }

    #[test]
    fn test_period_start_weekly() {
        let start = BudgetPeriod::Weekly.period_start();
        assert_eq!(start.weekday(), Weekday::Mon);
    }

    #[tokio::test]
    async fn test_spend_cache() {
        let cache = BudgetSpendCache::new(60);
        assert!(cache.get_user_spend("alice").await.is_none());

        cache.set_user_spend("alice", 42.0).await;
        assert_eq!(cache.get_user_spend("alice").await, Some(42.0));
    }
}
