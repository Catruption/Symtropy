//! Shared policy for user-initiated source lookups.
//!
//! This crate deliberately has no background polling path. Every search must
//! come from a user action, and remote metadata is only allowed to become a
//! suggestion after a visual match.

use crate::model::SourceSite;
use symtropy_tags::MatchVerdict;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SourcePolicy {
    #[serde(default = "default_max_results")]
    pub max_results: usize,
    #[serde(default = "default_max_requests_per_run")]
    pub max_requests_per_run: usize,
    #[serde(default = "default_confirmed_at")]
    pub confirmed_at: f32,
    #[serde(default = "default_probable_at")]
    pub probable_at: f32,
    #[serde(default = "default_true")]
    pub user_initiated_only: bool,
    #[serde(default = "default_true")]
    pub match_before_enrichment: bool,
    #[serde(default)] pub allowed_sites: Vec<SourceSite>,
}

fn default_max_results() -> usize { 20 }
fn default_max_requests_per_run() -> usize { 40 }
fn default_confirmed_at() -> f32 { 0.95 }
fn default_probable_at() -> f32 { 0.82 }
fn default_true() -> bool { true }

impl Default for SourcePolicy {
    fn default() -> Self {
        Self {
            max_results: default_max_results(),
            max_requests_per_run: default_max_requests_per_run(),
            confirmed_at: default_confirmed_at(),
            probable_at: default_probable_at(),
            user_initiated_only: true,
            match_before_enrichment: true,
            allowed_sites: SourceSite::all(),
        }
    }
}

impl SourcePolicy {
    pub fn allows(&self, site: SourceSite) -> bool {
        self.allowed_sites.contains(&site)
    }

    pub fn verdict(&self, similarity: f32) -> MatchVerdict {
        MatchEvidenceVerdict::for_thresholds(similarity, self.confirmed_at, self.probable_at)
    }
}

struct MatchEvidenceVerdict;

impl MatchEvidenceVerdict {
    fn for_thresholds(similarity: f32, confirmed_at: f32, probable_at: f32) -> MatchVerdict {
        if similarity >= confirmed_at {
            MatchVerdict::Confirmed
        } else if similarity >= probable_at {
            MatchVerdict::Probable
        } else {
            MatchVerdict::Rejected
        }
    }
}

/// Exponential backoff used by adapters between retries. It never sleeps on
/// its own; an async/blocking runner asks for the next delay.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Backoff {
    pub base_ms: u64,
    pub max_ms: u64,
    pub factor: u32,
}

impl Default for Backoff {
    fn default() -> Self {
        Self { base_ms: 500, max_ms: 15_000, factor: 2 }
    }
}

impl Backoff {
    pub fn delay_ms(&self, attempt: u32) -> u64 {
        let multiplier = self.factor.saturating_pow(attempt.min(16));
        self.base_ms
            .saturating_mul(multiplier as u64)
            .min(self.max_ms)
    }
}

/// Small request-count guard. It prevents one user action from turning into a
/// mass crawl, without any background scheduling.
#[derive(Clone, Debug, Default)]
pub struct RequestBudget {
    used: usize,
}

impl RequestBudget {
    pub fn new() -> Self {
        Self { used: 0 }
    }

    pub fn try_spend(&mut self, limit: usize) -> bool {
        if self.used >= limit {
            return false;
        }
        self.used += 1;
        true
    }

    pub fn used(&self) -> usize {
        self.used
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_is_bounded() {
        let backoff = Backoff::default();
        assert_eq!(backoff.delay_ms(0), 500);
        assert_eq!(backoff.delay_ms(1), 1_000);
        assert_eq!(backoff.delay_ms(16), backoff.max_ms);
        assert_eq!(backoff.delay_ms(99), backoff.max_ms);
    }

    #[test]
    fn request_budget_stops_at_its_limit() {
        let mut budget = RequestBudget::new();
        assert!(budget.try_spend(2));
        assert!(budget.try_spend(2));
        assert!(!budget.try_spend(2));
        assert_eq!(budget.used(), 2);
    }

    #[test]
    fn policy_thresholds_match_verdicts() {
        let policy = SourcePolicy::default();
        assert_eq!(policy.verdict(0.99), MatchVerdict::Confirmed);
        assert_eq!(policy.verdict(0.85), MatchVerdict::Probable);
        assert_eq!(policy.verdict(0.5), MatchVerdict::Rejected);
    }
}
