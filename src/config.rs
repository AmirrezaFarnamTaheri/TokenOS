//! Loads and validates the TokenOS configuration: provider profiles, the
//! two-tier model filter matrix, routing policy thresholds and fallback
//! chains.

use crate::kernel::RouterPolicy;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Two-Tier Filtering Matrix.
///
/// Precedence rules (deterministic, short-circuiting):
///  1. Absolute blacklist: any match in `exclude` drops the model immediately.
///  2. Explicit whitelist: if `include` is non-empty, the model must match at
///     least one pattern; otherwise it is dropped.
///  3. Default fallback: with neither list defined, all models are permitted.
///
/// Patterns support shell-style wildcards ('*', '?') plus exact equality.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelFilter {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
}

impl ModelFilter {
    /// Evaluates the precedence rules for a given model ID.
    pub fn is_model_allowed(&self, model_id: &str) -> bool {
        // Rule 1: absolute blacklist — exclusion always wins.
        if self.exclude.iter().any(|p| pattern_match(p, model_id)) {
            return false;
        }
        // Rule 3: default allow when no whitelist defined.
        if self.include.is_empty() {
            return true;
        }
        // Rule 2: explicit whitelist.
        self.include.iter().any(|p| pattern_match(p, model_id))
    }

    /// Subset of candidate model IDs that pass the matrix.
    pub fn filter<'a>(&self, models: impl IntoIterator<Item = &'a str>) -> Vec<String> {
        models
            .into_iter()
            .filter(|m| self.is_model_allowed(m))
            .map(|m| m.to_string())
            .collect()
    }
}

/// Shell-style wildcard match ('*' any run, '?' any single char).
fn pattern_match(pattern: &str, s: &str) -> bool {
    if pattern == s {
        return true;
    }
    glob_match(pattern.as_bytes(), s.as_bytes())
}

fn glob_match(p: &[u8], s: &[u8]) -> bool {
    // Iterative glob with backtracking on '*' (linear in practice).
    let (mut pi, mut si) = (0usize, 0usize);
    let (mut star, mut mark) = (usize::MAX, 0usize);
    while si < s.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = pi;
            mark = si;
            pi += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

/// One upstream platform profile.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Provider {
    pub adapter: String, // mock | openai | anthropic | gemini | proxy
    #[serde(default)]
    pub auth_type: String, // api_key | oauth2 | none
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub api_key_env: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub endpoint: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    #[serde(default)]
    pub priority: i32, // lower = preferred
    #[serde(default, rename = "quota_limit_per_min")]
    pub quota_per_min: u32,
    #[serde(default, rename = "max_context_tokens")]
    pub max_context: usize,
    #[serde(default)]
    pub cost_per_mtok_in: f64,
    #[serde(default)]
    pub cost_per_mtok_out: f64,
    #[serde(default)]
    pub models: ModelFilter,
    #[serde(default)]
    pub disabled: bool,
}

/// Binds kernel routes to a provider with a fallback chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingRule {
    pub provider: String,
    pub route_types: Vec<String>,
    #[serde(default, rename = "max_context_tokens")]
    pub max_context: usize,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub fallback: String,
    #[serde(default)]
    pub timeout_ms: u64,
}

/// Tunes the shadow-pricing utility function:
///   U = confidence / (alpha*tokenCost + beta*latency)
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PricingWeights {
    pub alpha: f64,
    pub beta: f64,
}

impl Default for PricingWeights {
    fn default() -> Self {
        PricingWeights {
            alpha: 1.0,
            beta: 0.002,
        }
    }
}

/// Security and retention settings (F-11, F-13).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityPolicy {
    /// Whether trace recording is disabled
    #[serde(default)]
    pub disable_traces: bool,
    /// Maximum days to retain trace and database records (0 = keep forever)
    #[serde(default = "default_retention_days")]
    pub retention_days: usize,
    /// File permission override (e.g. 0o600 for owner-only on Unix, ignored on Windows)
    #[serde(default = "default_true_bool")]
    pub owner_only_permissions: bool,
    /// Daily spend limit in USD (0.0 = no limit)
    #[serde(default)]
    pub daily_spend_limit_usd: f64,
    /// Monthly spend limit in USD (0.0 = no limit)
    #[serde(default)]
    pub monthly_spend_limit_usd: f64,
}

fn default_retention_days() -> usize {
    30
}

fn default_true_bool() -> bool {
    true
}

impl Default for SecurityPolicy {
    fn default() -> Self {
        SecurityPolicy {
            disable_traces: false,
            retention_days: 30,
            owner_only_permissions: true,
            daily_spend_limit_usd: 0.0,
            monthly_spend_limit_usd: 0.0,
        }
    }
}

/// Root configuration document (~/.config/tokenos/config.yaml).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub current_profile: String,
    pub policy: RouterPolicy,
    pub providers: BTreeMap<String, Provider>,
    #[serde(rename = "execution_routing")]
    pub routing: Vec<RoutingRule>,
    pub pricing: PricingWeights,
    #[serde(default)]
    pub security: SecurityPolicy,
}

impl Default for Config {
    /// Complete, working default configuration with a mock provider so the
    /// system is testable offline out of the box.
    fn default() -> Self {
        let mut providers = BTreeMap::new();
        providers.insert(
            "mock".into(),
            Provider {
                adapter: "mock".into(),
                auth_type: "none".into(),
                api_key_env: String::new(),
                endpoint: String::new(),
                model: "mock-1".into(),
                priority: 100,
                quota_per_min: 0,
                max_context: 128_000,
                cost_per_mtok_in: 0.0,
                cost_per_mtok_out: 0.0,
                models: ModelFilter::default(),
                disabled: false,
            },
        );
        providers.insert(
            "openai".into(),
            Provider {
                adapter: "openai".into(),
                auth_type: "api_key".into(),
                api_key_env: "OPENAI_API_KEY".into(),
                endpoint: "https://api.openai.com/v1".into(),
                model: "gpt-4o-mini".into(),
                priority: 2,
                quota_per_min: 0,
                max_context: 128_000,
                cost_per_mtok_in: 0.15,
                cost_per_mtok_out: 0.60,
                models: ModelFilter {
                    include: vec!["gpt-4o*".into(), "gpt-4.1*".into(), "o4*".into()],
                    exclude: vec![],
                },
                disabled: true,
            },
        );
        providers.insert(
            "anthropic".into(),
            Provider {
                adapter: "anthropic".into(),
                auth_type: "api_key".into(),
                api_key_env: "ANTHROPIC_API_KEY".into(),
                endpoint: "https://api.anthropic.com/v1".into(),
                model: "claude-sonnet-4-20250514".into(),
                priority: 1,
                quota_per_min: 0,
                max_context: 200_000,
                cost_per_mtok_in: 3.0,
                cost_per_mtok_out: 15.0,
                models: ModelFilter {
                    include: vec!["claude-*".into()],
                    exclude: vec!["claude-2*".into()],
                },
                disabled: true,
            },
        );
        providers.insert(
            "gemini".into(),
            Provider {
                adapter: "gemini".into(),
                auth_type: "api_key".into(),
                api_key_env: "GEMINI_API_KEY".into(),
                endpoint: "https://generativelanguage.googleapis.com/v1beta".into(),
                model: "gemini-2.0-flash".into(),
                priority: 3,
                quota_per_min: 0,
                max_context: 1_048_576,
                cost_per_mtok_in: 0.10,
                cost_per_mtok_out: 0.40,
                models: ModelFilter {
                    include: vec![
                        "gemini-2.0-flash-*".into(),
                        "gemini-1.5-*".into(),
                        "gemini-2.0-flash".into(),
                    ],
                    exclude: vec!["gemini-2.0-flash-thinking-*".into(), "gemini-1.0-*".into()],
                },
                disabled: true,
            },
        );
        Config {
            current_profile: "default".into(),
            policy: RouterPolicy::default(),
            pricing: PricingWeights::default(),
            providers,
            routing: vec![
                RoutingRule {
                    provider: "anthropic".into(),
                    route_types: vec!["IMPLEMENT".into(), "PATCH".into()],
                    max_context: 0,
                    fallback: "openai".into(),
                    timeout_ms: 120_000,
                },
                RoutingRule {
                    provider: "openai".into(),
                    route_types: vec![
                        "DIRECT".into(),
                        "REUSE".into(),
                        "DELEGATE".into(),
                        "PARTIAL".into(),
                    ],
                    max_context: 0,
                    fallback: "gemini".into(),
                    timeout_ms: 60_000,
                },
                RoutingRule {
                    provider: "gemini".into(),
                    route_types: vec!["VERIFY".into()],
                    max_context: 0,
                    fallback: "mock".into(),
                    timeout_ms: 30_000,
                },
                RoutingRule {
                    provider: "mock".into(),
                    route_types: vec!["*".into()],
                    max_context: 0,
                    fallback: String::new(),
                    timeout_ms: 10_000,
                },
            ],
            security: SecurityPolicy::default(),
        }
    }
}

impl Config {
    /// Canonical config file location.
    pub fn default_path() -> PathBuf {
        if let Ok(p) = std::env::var("TOKENOS_CONFIG") {
            if !p.is_empty() {
                return PathBuf::from(p);
            }
        }
        dirs::home_dir()
            .map(|h| h.join(".config").join("tokenos").join("config.yaml"))
            .unwrap_or_else(|| PathBuf::from("tokenos.yaml"))
    }

    /// Reads a config file, falling back to defaults if it does not exist.
    pub fn load(path: Option<&Path>) -> Result<Config> {
        let path = path.map(PathBuf::from).unwrap_or_else(Self::default_path);
        let data = match std::fs::read_to_string(&path) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(e) => return Err(e).context("read config"),
        };
        let cfg: Config = serde_yaml::from_str(&data).context("parse config")?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Writes the configuration to disk, creating parent directories.
    pub fn save(&self, path: Option<&Path>) -> Result<()> {
        let path = path.map(PathBuf::from).unwrap_or_else(Self::default_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let data = serde_yaml::to_string(self)?;
        std::fs::write(&path, data)?;
        Ok(())
    }

    /// Enforces structural invariants.
    pub fn validate(&self) -> Result<()> {
        if self.providers.is_empty() {
            bail!("config: at least one provider required");
        }
        if self.policy.ask_threshold < 0.0 || self.policy.ask_threshold > 1.0 {
            bail!("config: policy.ask_threshold must be between 0.0 and 1.0");
        }
        if self.policy.delegation_penalty < 0.0 {
            bail!("config: policy.delegation_penalty must be non-negative");
        }
        if self.policy.delegation_min_scale < 0.0 {
            bail!("config: policy.delegation_min_scale must be non-negative");
        }
        if self.policy.max_cost_per_task_usd < 0.0 {
            bail!("config: policy.max_cost_per_task_usd must be non-negative");
        }

        for (name, p) in &self.providers {
            if p.adapter.is_empty() {
                bail!("config: provider {name:?} missing adapter");
            }
            match p.adapter.as_str() {
                "mock" | "openai" | "anthropic" | "gemini" | "proxy" | "proxy_ide" => {}
                other => bail!("config: provider {name:?} uses unknown adapter {other:?}"),
            }
            if matches!(p.adapter.as_str(), "openai" | "anthropic" | "gemini")
                && !p.disabled
                && p.api_key_env.trim().is_empty()
            {
                bail!("config: enabled provider {name:?} requires api_key_env");
            }
            if matches!(p.adapter.as_str(), "proxy" | "proxy_ide") && p.endpoint.trim().is_empty() {
                bail!("config: provider {name:?} proxy adapter requires endpoint");
            }
            if p.max_context == 0 && p.adapter != "mock" {
                bail!("config: provider {name:?} max_context_tokens must be positive");
            }
            if p.cost_per_mtok_in < 0.0 || p.cost_per_mtok_out < 0.0 {
                bail!("config: provider {name:?} costs must be non-negative");
            }
        }
        for (i, r) in self.routing.iter().enumerate() {
            if !self.providers.contains_key(&r.provider) {
                bail!(
                    "config: routing rule {i} references unknown provider {:?}",
                    r.provider
                );
            }
            if !r.fallback.is_empty() && !self.providers.contains_key(&r.fallback) {
                bail!(
                    "config: routing rule {i} fallback references unknown provider {:?}",
                    r.fallback
                );
            }
            for rt in &r.route_types {
                match rt.as_str() {
                    "DIRECT" | "REUSE" | "PATCH" | "IMPLEMENT" | "PARTIAL" | "DELEGATE" | "ASK"
                    | "VERIFY" | "ESCALATE-CONFLICT" | "ESCALATE-SAFETY" | "ESCALATE-EXTERNAL"
                    | "*" => {}
                    other => bail!(
                        "config: routing rule {i} contains unknown route type {:?}",
                        other
                    ),
                }
            }
        }
        Ok(())
    }

    /// Resolves the ordered provider chain for a route: the first matching
    /// routing rule plus its fallback chain, then remaining enabled providers
    /// by priority. Cycles are guarded.
    pub fn provider_chain(&self, route: &str) -> Vec<String> {
        let mut seen: std::collections::HashSet<String> = Default::default();
        let mut chain: Vec<String> = Vec::new();

        let add =
            |name: &str, seen: &mut std::collections::HashSet<String>, chain: &mut Vec<String>| {
                if name.is_empty() || seen.contains(name) {
                    return;
                }
                if let Some(p) = self.providers.get(name) {
                    seen.insert(name.to_string());
                    if !p.disabled {
                        chain.push(name.to_string());
                    }
                }
            };

        for rule in &self.routing {
            if !matches_route(&rule.route_types, route) {
                continue;
            }
            add(&rule.provider, &mut seen, &mut chain);
            let mut fb = rule.fallback.clone();
            while !fb.is_empty() && !seen.contains(&fb) {
                let cur = fb.clone();
                add(&cur, &mut seen, &mut chain);
                // follow chained fallbacks declared by rules for the fallback provider
                fb = self
                    .routing
                    .iter()
                    .find(|r2| r2.provider == cur)
                    .map(|r2| r2.fallback.clone())
                    .unwrap_or_default();
            }
            break;
        }

        // Append remaining enabled providers ordered by priority as last resort.
        let mut rest: Vec<(&String, i32)> = self
            .providers
            .iter()
            .filter(|(name, p)| !seen.contains(*name) && !p.disabled)
            .map(|(name, p)| (name, p.priority))
            .collect();
        rest.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(b.0)));
        chain.extend(rest.into_iter().map(|(n, _)| n.clone()));
        chain
    }

    pub fn timeout_for(&self, route: &str) -> std::time::Duration {
        for r in &self.routing {
            for t in &r.route_types {
                if (t == "*" || t == route) && r.timeout_ms > 0 {
                    return std::time::Duration::from_millis(r.timeout_ms);
                }
            }
        }
        std::time::Duration::from_secs(120)
    }
}

fn matches_route(types: &[String], route: &str) -> bool {
    types.iter().any(|t| t == "*" || t == route)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclude_always_wins() {
        let f = ModelFilter {
            include: vec!["claude-*".into()],
            exclude: vec!["claude-2*".into()],
        };
        assert!(f.is_model_allowed("claude-sonnet-4"));
        assert!(!f.is_model_allowed("claude-2.1"));
    }

    #[test]
    fn include_acts_as_whitelist() {
        let f = ModelFilter {
            include: vec!["gpt-4o*".into()],
            exclude: vec![],
        };
        assert!(f.is_model_allowed("gpt-4o-mini"));
        assert!(!f.is_model_allowed("gpt-3.5-turbo"));
    }

    #[test]
    fn empty_filter_allows_all() {
        let f = ModelFilter::default();
        assert!(f.is_model_allowed("anything"));
    }

    #[test]
    fn wildcard_family_block() {
        let f = ModelFilter {
            include: vec![],
            exclude: vec!["gemini-1.0-*".into()],
        };
        assert!(!f.is_model_allowed("gemini-1.0-pro"));
        assert!(f.is_model_allowed("gemini-2.0-flash"));
    }

    #[test]
    fn glob_question_mark() {
        assert!(pattern_match("o?-mini", "o4-mini"));
        assert!(!pattern_match("o?-mini", "o44-mini"));
    }

    #[test]
    fn default_config_is_valid() {
        Config::default().validate().unwrap();
    }

    #[test]
    fn enabled_live_provider_requires_api_key_env_name() {
        let mut cfg = Config::default();
        let p = cfg.providers.get_mut("openai").unwrap();
        p.disabled = false;
        p.api_key_env.clear();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("requires api_key_env"), "{err}");
    }

    #[test]
    fn unknown_adapter_is_rejected() {
        let mut cfg = Config::default();
        cfg.providers.get_mut("mock").unwrap().adapter = "mystery".into();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("unknown adapter"), "{err}");
    }

    #[test]
    fn provider_chain_follows_fallbacks() {
        let mut cfg = Config::default();
        // Enable everything to exercise the chain.
        for p in cfg.providers.values_mut() {
            p.disabled = false;
        }
        let chain = cfg.provider_chain("IMPLEMENT");
        assert_eq!(chain[0], "anthropic");
        assert_eq!(chain[1], "openai");
        // gemini follows via openai's rule fallback
        assert!(chain.contains(&"gemini".to_string()));
        assert!(chain.contains(&"mock".to_string()));
    }

    #[test]
    fn disabled_providers_skipped() {
        let cfg = Config::default(); // only mock enabled
        let chain = cfg.provider_chain("IMPLEMENT");
        assert_eq!(chain, vec!["mock".to_string()]);
    }

    #[test]
    fn yaml_roundtrip() {
        let cfg = Config::default();
        let y = serde_yaml::to_string(&cfg).unwrap();
        let back: Config = serde_yaml::from_str(&y).unwrap();
        back.validate().unwrap();
        assert_eq!(back.providers.len(), cfg.providers.len());
    }

    #[test]
    fn invalid_policy_numeric_ranges_are_rejected() {
        let mut cfg = Config::default();
        cfg.policy.ask_threshold = 1.5;
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("ask_threshold"));

        let mut cfg = Config::default();
        cfg.policy.delegation_penalty = -0.5;
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("delegation_penalty"));

        let mut cfg = Config::default();
        cfg.policy.delegation_min_scale = -0.1;
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("delegation_min_scale"));

        let mut cfg = Config::default();
        cfg.policy.max_cost_per_task_usd = -10.0;
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("max_cost_per_task_usd"));
    }

    #[test]
    fn invalid_route_type_is_rejected() {
        let mut cfg = Config::default();
        cfg.routing[0].route_types.push("INVALID-ROUTE".into());
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("unknown route type"), "{err}");
    }
}
