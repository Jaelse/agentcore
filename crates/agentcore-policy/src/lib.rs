//! Policy engine.
//!
//! Policies are TOML documents written by humans. They contain an ordered list
//! of rules; each rule matches a set of actions and assigns an [`Effect`].
//!
//! Evaluation uses **deny-overrides** semantics, which keeps policies safe to
//! compose: if any matching rule denies, the action is denied; otherwise if any
//! matching rule requires approval, a human must approve; otherwise if any rule
//! allows, it is allowed; otherwise the policy `default` applies.
//!
//! ```toml
//! name = "default"
//! default = "require_approval"
//!
//! [[rules]]
//! id = "no-secrets"
//! effect = "deny"
//! kinds = ["file_read", "file_write"]
//! paths = ["**/.env", "**/*.pem"]
//! ```

mod path;

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use agentcore_core::{Action, ActionKind, Verdict};
use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub use path::{normalize_action, normalize_path};

#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    #[error("failed to read policy {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("failed to parse policy {path}: {source}")]
    Parse {
        path: String,
        source: Box<toml::de::Error>,
    },
    #[error("policy `{policy}` rule `{rule}`: invalid pattern `{pattern}`: {source}")]
    Pattern {
        policy: String,
        rule: String,
        pattern: String,
        source: globset::Error,
    },
    #[error("policy `{policy}`: {message}")]
    Invalid { policy: String, message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    Allow,
    Deny,
    RequireApproval,
}

/// Session-wide limits enforced by the runtime regardless of rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    /// Hard wall-clock limit for a session.
    pub max_session_secs: u64,
    /// Maximum number of actions an agent may request.
    pub max_actions: u64,
    /// How long to wait for a human before treating an approval as denied.
    pub approval_timeout_secs: u64,
    /// Truncate tool output returned to the agent beyond this size.
    pub max_output_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_session_secs: 3600,
            max_actions: 1000,
            approval_timeout_secs: 900,
            max_output_bytes: 64 * 1024,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub id: String,
    #[serde(default)]
    pub description: String,
    pub effect: Effect,
    /// Action kinds this rule applies to. Empty means all kinds.
    #[serde(default)]
    pub kinds: Vec<ActionKind>,
    /// Globs matched against the full command line of `exec` actions.
    #[serde(default)]
    pub commands: Vec<String>,
    /// Globs matched against normalised absolute paths of file actions.
    /// `*` does not cross `/`; use `**` for that.
    #[serde(default)]
    pub paths: Vec<String>,
    /// Globs matched against hosts of `network` actions.
    #[serde(default)]
    pub hosts: Vec<String>,
    /// Globs matched against tool names of `tool_call` actions.
    #[serde(default)]
    pub tools: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_effect")]
    pub default: Effect,
    #[serde(default)]
    pub limits: Limits,
    #[serde(default)]
    pub rules: Vec<Rule>,
}

fn default_effect() -> Effect {
    Effect::RequireApproval
}

impl Policy {
    pub fn from_toml(src: &str, origin: &str) -> Result<Self, PolicyError> {
        toml::from_str(src).map_err(|e| PolicyError::Parse {
            path: origin.to_string(),
            source: Box::new(e),
        })
    }

    pub fn compile(self) -> Result<CompiledPolicy, PolicyError> {
        CompiledPolicy::new(self)
    }
}

#[derive(Debug, Clone)]
struct CompiledRule {
    rule: Rule,
    commands: Option<GlobSet>,
    paths: Option<GlobSet>,
    hosts: Option<GlobSet>,
    tools: Option<GlobSet>,
}

impl CompiledRule {
    fn matches(&self, action: &Action) -> bool {
        if !self.rule.kinds.is_empty() && !self.rule.kinds.contains(&action.kind()) {
            return false;
        }
        // Every matcher the rule specifies must match a field the action has.
        let checks: [(&Option<GlobSet>, Option<String>); 4] = [
            (&self.commands, action.command_line()),
            (
                &self.paths,
                match action {
                    Action::FileRead { path } | Action::FileWrite { path, .. } => {
                        Some(path.clone())
                    }
                    _ => None,
                },
            ),
            (
                &self.hosts,
                match action {
                    Action::Network { host, .. } => Some(host.to_ascii_lowercase()),
                    _ => None,
                },
            ),
            (
                &self.tools,
                match action {
                    Action::ToolCall { tool, .. } => Some(tool.clone()),
                    _ => None,
                },
            ),
        ];
        checks.into_iter().all(|(set, value)| match (set, value) {
            (None, _) => true,
            (Some(set), Some(value)) => set.is_match(&value),
            (Some(_), None) => false,
        })
    }
}

/// A validated policy ready for evaluation.
#[derive(Debug, Clone)]
pub struct CompiledPolicy {
    policy: Policy,
    rules: Vec<CompiledRule>,
    digest: String,
}

impl CompiledPolicy {
    fn new(policy: Policy) -> Result<Self, PolicyError> {
        let invalid = |message: String| PolicyError::Invalid {
            policy: policy.name.clone(),
            message,
        };
        if policy.name.trim().is_empty() {
            return Err(invalid("name must not be empty".into()));
        }
        let mut seen = HashSet::new();
        let mut rules = Vec::with_capacity(policy.rules.len());
        for rule in &policy.rules {
            if rule.id.trim().is_empty() {
                return Err(invalid("every rule needs a non-empty id".into()));
            }
            if !seen.insert(rule.id.as_str()) {
                return Err(invalid(format!("duplicate rule id `{}`", rule.id)));
            }
            let build = |patterns: &[String], literal_separator: bool| {
                build_set(&policy.name, &rule.id, patterns, literal_separator)
            };
            rules.push(CompiledRule {
                commands: build(&rule.commands, false)?,
                paths: build(&rule.paths, true)?,
                hosts: build(&rule.hosts, false)?,
                tools: build(&rule.tools, false)?,
                rule: rule.clone(),
            });
        }
        let canonical = serde_json::to_vec(&policy).expect("policy serialises");
        let digest = hex::encode(Sha256::digest(&canonical));
        Ok(Self {
            policy,
            rules,
            digest,
        })
    }

    pub fn name(&self) -> &str {
        &self.policy.name
    }

    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    pub fn limits(&self) -> &Limits {
        &self.policy.limits
    }

    /// SHA-256 of the canonical policy, recorded in the audit log so it is
    /// provable which policy governed a session.
    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// Evaluate an action. Callers must pass an action that has already been
    /// normalised with [`normalize_action`], otherwise path rules can be
    /// bypassed with `..` segments.
    pub fn evaluate(&self, action: &Action) -> Verdict {
        let mut approval: Option<&Rule> = None;
        let mut allow: Option<&Rule> = None;
        for compiled in self.rules.iter().filter(|r| r.matches(action)) {
            let rule = &compiled.rule;
            match rule.effect {
                Effect::Deny => {
                    return Verdict::Deny {
                        rule: Some(rule.id.clone()),
                        reason: reason(rule, "denied by policy"),
                    };
                }
                Effect::RequireApproval => {
                    approval.get_or_insert(rule);
                }
                Effect::Allow => {
                    allow.get_or_insert(rule);
                }
            }
        }
        if let Some(rule) = approval {
            return Verdict::RequireApproval {
                rule: Some(rule.id.clone()),
                reason: reason(rule, "policy requires human approval"),
            };
        }
        if let Some(rule) = allow {
            return Verdict::Allow {
                rule: Some(rule.id.clone()),
            };
        }
        match self.policy.default {
            Effect::Allow => Verdict::Allow { rule: None },
            Effect::Deny => Verdict::Deny {
                rule: None,
                reason: "no rule matched and the policy default is deny".into(),
            },
            Effect::RequireApproval => Verdict::RequireApproval {
                rule: None,
                reason: "no rule matched; policy default requires approval".into(),
            },
        }
    }
}

fn reason(rule: &Rule, fallback: &str) -> String {
    if rule.description.is_empty() {
        format!("{fallback} (rule `{}`)", rule.id)
    } else {
        rule.description.clone()
    }
}

fn build_set(
    policy: &str,
    rule: &str,
    patterns: &[String],
    literal_separator: bool,
) -> Result<Option<GlobSet>, PolicyError> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob: Result<Glob, _> = GlobBuilder::new(pattern)
            .literal_separator(literal_separator)
            .backslash_escape(true)
            .build();
        builder.add(glob.map_err(|source| PolicyError::Pattern {
            policy: policy.into(),
            rule: rule.into(),
            pattern: pattern.clone(),
            source,
        })?);
    }
    builder
        .build()
        .map(Some)
        .map_err(|source| PolicyError::Pattern {
            policy: policy.into(),
            rule: rule.into(),
            pattern: patterns.join(", "),
            source,
        })
}

/// All policies known to a deployment, keyed by name.
#[derive(Debug, Clone, Default)]
pub struct PolicySet {
    policies: BTreeMap<String, std::sync::Arc<CompiledPolicy>>,
}

impl PolicySet {
    /// Load every `*.toml` file in `dir`.
    pub fn load_dir(dir: &Path) -> Result<Self, PolicyError> {
        let io = |source| PolicyError::Io {
            path: dir.display().to_string(),
            source,
        };
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .map_err(io)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(io)?;
        entries.sort_by_key(|e| e.path());
        let mut set = Self::default();
        for entry in entries {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            set.insert(load_file(&path)?.compile()?)?;
        }
        Ok(set)
    }

    pub fn insert(&mut self, policy: CompiledPolicy) -> Result<(), PolicyError> {
        let name = policy.name().to_string();
        if self.policies.contains_key(&name) {
            return Err(PolicyError::Invalid {
                policy: name,
                message: "defined more than once".into(),
            });
        }
        self.policies.insert(name, std::sync::Arc::new(policy));
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<std::sync::Arc<CompiledPolicy>> {
        self.policies.get(name).cloned()
    }

    pub fn iter(&self) -> impl Iterator<Item = &std::sync::Arc<CompiledPolicy>> {
        self.policies.values()
    }

    pub fn is_empty(&self) -> bool {
        self.policies.is_empty()
    }
}

pub fn load_file(path: &Path) -> Result<Policy, PolicyError> {
    let src = std::fs::read_to_string(path).map_err(|source| PolicyError::Io {
        path: path.display().to_string(),
        source,
    })?;
    Policy::from_toml(&src, &path.display().to_string())
}

#[cfg(test)]
mod tests;
