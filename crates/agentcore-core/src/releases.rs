//! Releases: shipping what agents build to the people who use it.
//!
//! A **product** is something the organisation ships. Its departments'
//! agents propose **releases** (a semantic version, the commit, notes,
//! artifacts with SHA-256 digests); people approve them; the server rolls
//! them out in **stages** to a growing share of **units** (installations),
//! watching **health gates** and rolling back on its own when one fails.
//!
//! Everything here is pure and deterministic, so the same inputs give the
//! same risk, plan and cohort on every node.

use std::cmp::Ordering;
use std::fmt;

use chrono::{DateTime, Datelike, Timelike, Utc, Weekday};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{DepartmentId, OrgAgentId};

// ---- versions ------------------------------------------------------------------------

/// A semantic version (<https://semver.org>): `MAJOR.MINOR.PATCH[-PRE][+BUILD]`.
/// Ordering follows semver precedence (build metadata is ignored).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub pre: Vec<PreIdent>,
    pub build: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreIdent {
    Num(u64),
    Alpha(String),
}

impl Ord for PreIdent {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::Num(a), Self::Num(b)) => a.cmp(b),
            (Self::Num(_), Self::Alpha(_)) => Ordering::Less,
            (Self::Alpha(_), Self::Num(_)) => Ordering::Greater,
            (Self::Alpha(a), Self::Alpha(b)) => a.cmp(b),
        }
    }
}

impl PartialOrd for PreIdent {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn numeric(part: &str, what: &str) -> Result<u64, String> {
    if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("the {what} version is a number"));
    }
    if part.len() > 1 && part.starts_with('0') {
        return Err(format!("the {what} version has no leading zeros"));
    }
    part.parse()
        .map_err(|_| format!("the {what} version is too large"))
}

fn idents<'a>(s: &'a str, what: &str) -> Result<Vec<&'a str>, String> {
    let parts: Vec<&str> = s.split('.').collect();
    if parts
        .iter()
        .any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
    {
        return Err(format!(
            "{what} identifiers are dot-separated and use 0-9, A-Z, a-z and -"
        ));
    }
    Ok(parts)
}

impl std::str::FromStr for Version {
    type Err = String;

    /// Parses `1.2.3`, `1.2.3-rc.1+build.5`; a leading `v` is allowed.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        let s = s.strip_prefix('v').unwrap_or(s);
        if s.len() > 128 {
            return Err("a version is at most 128 characters".into());
        }
        let (rest, build) = match s.split_once('+') {
            Some((r, b)) => (r, idents(b, "build")?.join(".")),
            None => (s, String::new()),
        };
        let (core, pre) = match rest.split_once('-') {
            Some((c, p)) => (c, Some(p)),
            None => (rest, None),
        };
        let nums: Vec<&str> = core.split('.').collect();
        let [major, minor, patch] = nums.as_slice() else {
            return Err(format!(
                "`{s}` is not a semantic version (MAJOR.MINOR.PATCH, e.g. 1.4.0)"
            ));
        };
        let pre = match pre {
            None => Vec::new(),
            Some(p) => idents(p, "pre-release")?
                .into_iter()
                .map(|i| {
                    if i.bytes().all(|b| b.is_ascii_digit()) {
                        numeric(i, "pre-release").map(PreIdent::Num)
                    } else {
                        Ok(PreIdent::Alpha(i.to_string()))
                    }
                })
                .collect::<Result<_, _>>()?,
        };
        Ok(Self {
            major: numeric(major, "major")?,
            minor: numeric(minor, "minor")?,
            patch: numeric(patch, "patch")?,
            pre,
            build,
        })
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        for (i, p) in self.pre.iter().enumerate() {
            f.write_str(if i == 0 { "-" } else { "." })?;
            match p {
                PreIdent::Num(n) => write!(f, "{n}")?,
                PreIdent::Alpha(a) => f.write_str(a)?,
            }
        }
        if !self.build.is_empty() {
            write!(f, "+{}", self.build)?;
        }
        Ok(())
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                // A pre-release comes before its release.
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => self.pre.cmp(&other.pre),
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// What kind of change a version is compared to the one before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Bump {
    /// The product's first release.
    Initial,
    /// Breaking changes (or a minor bump below 1.0, where anything may change).
    Major,
    Minor,
    Patch,
    /// Only the pre-release part changed.
    Prerelease,
}

impl Bump {
    pub fn between(previous: Option<&Version>, next: &Version) -> Self {
        let Some(prev) = previous else {
            return Self::Initial;
        };
        if next.major != prev.major || (next.major == 0 && next.minor != prev.minor) {
            Self::Major
        } else if next.minor != prev.minor {
            Self::Minor
        } else if next.patch != prev.patch {
            Self::Patch
        } else {
            Self::Prerelease
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Initial => "initial",
            Self::Major => "major",
            Self::Minor => "minor",
            Self::Patch => "patch",
            Self::Prerelease => "prerelease",
        }
    }
}

// ---- rollout plans -------------------------------------------------------------------

/// One step of a staged rollout: `percent` of units get the release, and it
/// stays there for `soak_minutes` (with healthy gates) before the next step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Stage {
    pub name: String,
    pub percent: f64,
    #[serde(default)]
    pub soak_minutes: u32,
}

impl Stage {
    pub fn new(name: &str, percent: f64, soak_minutes: u32) -> Self {
        Self {
            name: name.into(),
            percent,
            soak_minutes,
        }
    }
}

pub const MAX_STAGES: usize = 10;
/// A week.
pub const MAX_SOAK_MINUTES: u32 = 7 * 24 * 60;

/// Canary, early adopters, half, everyone.
pub fn default_plan() -> Vec<Stage> {
    vec![
        Stage::new("canary", 1.0, 60),
        Stage::new("early", 10.0, 120),
        Stage::new("half", 50.0, 240),
        Stage::new("everyone", 100.0, 0),
    ]
}

/// Stages grow strictly and end with everyone.
pub fn check_plan(plan: &[Stage]) -> Result<(), String> {
    if plan.is_empty() || plan.len() > MAX_STAGES {
        return Err(format!("a rollout plan has 1 to {MAX_STAGES} stages"));
    }
    let mut last = 0.0;
    for s in plan {
        let name = s.name.trim();
        if name.is_empty() || name.chars().count() > 40 {
            return Err("every stage has a name of up to 40 characters".into());
        }
        if !(s.percent > last && s.percent <= 100.0) || !s.percent.is_finite() {
            return Err(format!(
                "stage `{name}`: percentages grow from stage to stage, up to 100"
            ));
        }
        if s.soak_minutes > MAX_SOAK_MINUTES {
            return Err(format!(
                "stage `{name}`: a stage soaks at most {MAX_SOAK_MINUTES} minutes"
            ));
        }
        last = s.percent;
    }
    if (last - 100.0).abs() > f64::EPSILON {
        return Err("the last stage reaches 100% of units".into());
    }
    Ok(())
}

/// The plan for a release of this risk: riskier releases soak longer and
/// start smaller.
pub fn adapt_plan(plan: &[Stage], level: RiskLevel) -> Vec<Stage> {
    let factor = match level {
        RiskLevel::Low => return plan.to_vec(),
        RiskLevel::Medium => 2,
        RiskLevel::High => 3,
    };
    let last = plan.len().saturating_sub(1);
    let mut out: Vec<Stage> = plan
        .iter()
        .enumerate()
        .map(|(i, s)| Stage {
            name: s.name.clone(),
            percent: s.percent,
            soak_minutes: if i == last {
                s.soak_minutes
            } else {
                (s.soak_minutes.saturating_mul(factor)).min(MAX_SOAK_MINUTES)
            },
        })
        .collect();
    if level == RiskLevel::High
        && let Some(first) = out.first().cloned()
        && first.percent > 0.2
        && out.len() < MAX_STAGES
    {
        let pilot = ((first.percent / 4.0).max(0.1) * 100.0).round() / 100.0;
        out.insert(0, Stage::new("pilot", pilot, first.soak_minutes));
    }
    out
}

/// Approvals beyond the product's own a release of this risk needs.
pub fn extra_approvals(level: RiskLevel) -> u32 {
    u32::from(level == RiskLevel::High)
}

// ---- cohorts -------------------------------------------------------------------------

/// Where a unit falls in a release's rollout: 0..10000 (hundredths of a
/// percent). Stable for a unit and release, independent between releases
/// (so the same units are not always first).
pub fn bucket(release: Uuid, unit: &str) -> u32 {
    let digest = Sha256::digest(format!("{release}:{unit}").as_bytes());
    let mut first = [0u8; 8];
    first.copy_from_slice(&digest[..8]);
    (u64::from_be_bytes(first) % 10_000) as u32
}

/// Whether a unit is among the `percent` of units that get a release.
pub fn in_rollout(release: Uuid, unit: &str, percent: f64) -> bool {
    f64::from(bucket(release, unit)) < percent * 100.0
}

/// Which units follow a rollout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Track {
    /// The staged rollout decides.
    #[default]
    Stable,
    /// Early adopters: the newest release that is rolling out.
    Beta,
}

/// The release a unit should run: the one rolling out if the unit is in
/// its cohort (or follows beta), otherwise the last fully released one.
pub fn choose<'a>(
    rollout: Option<&'a Release>,
    baseline: Option<&'a Release>,
    unit: &str,
    track: Track,
) -> Option<&'a Release> {
    match rollout {
        Some(r) if r.status == ReleaseStatus::RollingOut || r.status == ReleaseStatus::Paused => {
            let take = match track {
                Track::Beta => true,
                Track::Stable => in_rollout(r.id, unit, r.percent()),
            };
            if take { Some(r) } else { baseline }
        }
        _ => baseline,
    }
}

// ---- health gates --------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Comparator {
    #[serde(alias = "<")]
    Lt,
    #[serde(alias = "<=")]
    Lte,
    #[serde(alias = ">")]
    Gt,
    #[serde(alias = ">=")]
    Gte,
}

impl Comparator {
    pub fn holds(self, value: f64, threshold: f64) -> bool {
        match self {
            Self::Lt => value < threshold,
            Self::Lte => value <= threshold,
            Self::Gt => value > threshold,
            Self::Gte => value >= threshold,
        }
    }

    pub fn symbol(self) -> &'static str {
        match self {
            Self::Lt => "<",
            Self::Lte => "<=",
            Self::Gt => ">",
            Self::Gte => ">=",
        }
    }
}

/// Where a gate's number comes from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum GateSource {
    /// Share of units on the release that reported no crash (0..1), from
    /// the reports update clients send.
    CrashFree,
    /// Share of units that installed the release without failure (0..1).
    InstallSuccess,
    /// A business data source: `sql` (PostgreSQL) or `path` (HTTP API);
    /// `field` is a column of the first row, or a JSON pointer (`/a/0/b`).
    /// `{version}`, `{release}` and `{since}` (when the rollout started,
    /// RFC 3339) are replaced in `sql` and `path`.
    Data {
        source: String,
        #[serde(default)]
        sql: Option<String>,
        #[serde(default)]
        path: Option<String>,
        field: String,
    },
}

/// A condition a rollout must keep meeting. A failing gate rolls the
/// release back; a gate without enough data holds the rollout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HealthGate {
    pub name: String,
    pub metric: GateSource,
    pub comparator: Comparator,
    pub threshold: f64,
    /// Units that must have reported before a client-report gate counts.
    #[serde(default)]
    pub min_samples: u64,
}

pub const MAX_GATES: usize = 10;

pub fn check_gates(gates: &[HealthGate]) -> Result<(), String> {
    if gates.len() > MAX_GATES {
        return Err(format!("a product has at most {MAX_GATES} health gates"));
    }
    for g in gates {
        let name = g.name.trim();
        if name.is_empty() || name.chars().count() > 60 {
            return Err("every health gate has a name of up to 60 characters".into());
        }
        if !g.threshold.is_finite() {
            return Err(format!("gate `{name}`: the threshold is a number"));
        }
        match &g.metric {
            GateSource::CrashFree | GateSource::InstallSuccess => {
                if !(0.0..=1.0).contains(&g.threshold) {
                    return Err(format!(
                        "gate `{name}`: a rate is between 0 and 1 (0.99 = 99%)"
                    ));
                }
            }
            GateSource::Data {
                source,
                sql,
                path,
                field,
            } => {
                if source.trim().is_empty() || field.trim().is_empty() {
                    return Err(format!("gate `{name}`: give the data `source` and `field`"));
                }
                if sql.is_some() == path.is_some() {
                    return Err(format!(
                        "gate `{name}`: give either `sql` (PostgreSQL) or `path` (HTTP API)"
                    ));
                }
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateStatus {
    Pass,
    /// Rolls the release back.
    Fail,
    /// Not enough data yet: the rollout holds.
    Waiting,
    /// The number could not be read: the rollout holds (never rolls back).
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GateResult {
    pub name: String,
    pub status: GateStatus,
    pub value: Option<f64>,
    pub samples: Option<u64>,
    pub detail: String,
}

impl GateResult {
    /// Judge a measured value.
    pub fn judge(gate: &HealthGate, value: f64, samples: Option<u64>) -> Self {
        let status = if samples.is_some_and(|n| n < gate.min_samples.max(1)) {
            GateStatus::Waiting
        } else if gate.comparator.holds(value, gate.threshold) {
            GateStatus::Pass
        } else {
            GateStatus::Fail
        };
        let measured = match samples {
            Some(n) => format!("{value:.4} from {n} unit(s)"),
            None => format!("{value:.4}"),
        };
        let detail = match status {
            GateStatus::Waiting => format!(
                "waiting for data: {} of {} unit(s) reported",
                samples.unwrap_or(0),
                gate.min_samples.max(1)
            ),
            _ => format!(
                "{measured}; needs {} {}",
                gate.comparator.symbol(),
                gate.threshold
            ),
        };
        Self {
            name: gate.name.clone(),
            status,
            value: Some(value),
            samples,
            detail,
        }
    }

    pub fn waiting(gate: &HealthGate, samples: u64) -> Self {
        Self {
            name: gate.name.clone(),
            status: GateStatus::Waiting,
            value: None,
            samples: Some(samples),
            detail: format!(
                "waiting for data: {samples} of {} unit(s) reported",
                gate.min_samples.max(1)
            ),
        }
    }

    pub fn error(gate: &HealthGate, detail: String) -> Self {
        Self {
            name: gate.name.clone(),
            status: GateStatus::Error,
            value: None,
            samples: None,
            detail,
        }
    }
}

/// Read a gate's number from a data source's answer: a JSON pointer, or a
/// column of the first row (`{"rows": [...]}`), or a field of the body.
pub fn extract_number(result: &Value, field: &str) -> Result<f64, String> {
    let found = if field.starts_with('/') {
        result.pointer(field)
    } else {
        result
            .get("rows")
            .and_then(|r| r.get(0))
            .and_then(|row| row.get(field))
            .or_else(|| result.get("body").and_then(|b| b.get(field)))
            .or_else(|| result.get(field))
    };
    match found {
        Some(Value::Number(n)) => n.as_f64().ok_or_else(|| "not a number".into()),
        Some(Value::String(s)) => s
            .trim()
            .parse()
            .map_err(|_| format!("`{field}` is `{s}`, not a number")),
        Some(Value::Bool(b)) => Ok(if *b { 1.0 } else { 0.0 }),
        Some(Value::Null) | None => Err(format!("the answer has no `{field}`")),
        Some(_) => Err(format!("`{field}` is not a number")),
    }
}

// ---- risk ----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    Low,
    Medium,
    High,
}

impl RiskLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    fn of(score: u32) -> Self {
        match score {
            0..30 => Self::Low,
            30..60 => Self::Medium,
            _ => Self::High,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RiskFactor {
    pub name: String,
    pub points: u32,
    pub detail: String,
}

/// Why a release is as risky as it is. The score comes from facts (the
/// version, the size of the change, checks, the product's history, the
/// time) plus the proposing agent's own judgement as one factor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Risk {
    /// 0 (routine) to 100.
    pub score: u32,
    pub level: RiskLevel,
    pub factors: Vec<RiskFactor>,
    /// The agent's (or person's) own assessment, in words.
    #[serde(default)]
    pub assessment: String,
}

/// A check reported with a release (tests, lint, security scan, ...).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckOutcome {
    pub name: String,
    pub passed: bool,
    #[serde(default)]
    pub detail: String,
}

/// What the size of a change is.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ChangeSize {
    #[serde(default)]
    pub files_changed: Option<u64>,
    #[serde(default)]
    pub lines_added: Option<u64>,
    #[serde(default)]
    pub lines_removed: Option<u64>,
    #[serde(default)]
    pub commits: Option<u64>,
}

impl ChangeSize {
    pub fn lines(&self) -> Option<u64> {
        match (self.lines_added, self.lines_removed) {
            (None, None) => None,
            (a, r) => Some(a.unwrap_or(0) + r.unwrap_or(0)),
        }
    }
}

pub struct RiskInput<'a> {
    pub bump: Bump,
    pub changes: &'a ChangeSize,
    pub checks: &'a [CheckOutcome],
    pub has_sbom: bool,
    pub gates: usize,
    /// Releases of the product that finished (released or rolled back),
    /// most recent ten.
    pub recent_finished: u32,
    pub recent_rollbacks: u32,
    pub days_since_last: Option<i64>,
    pub at: DateTime<Utc>,
    /// The proposer's own rating.
    pub declared: Option<RiskLevel>,
    pub assessment: String,
}

pub fn assess(input: &RiskInput<'_>) -> Risk {
    let mut factors = Vec::new();
    let mut add = |name: &str, points: u32, detail: String| {
        if points > 0 {
            factors.push(RiskFactor {
                name: name.into(),
                points,
                detail,
            });
        }
    };
    let (points, detail) = match input.bump {
        Bump::Initial => (20, "the product's first release"),
        Bump::Major => (25, "a major version: breaking changes are allowed"),
        Bump::Minor => (10, "a minor version: new features"),
        Bump::Patch => (3, "a patch version: fixes only"),
        Bump::Prerelease => (5, "a new pre-release of the same version"),
    };
    add("version", points, detail.into());
    match input.changes.lines() {
        None => add("change size", 8, "the size of the change is unknown".into()),
        Some(lines) => {
            let points = match lines {
                0..=100 => 0,
                101..=500 => 5,
                501..=2000 => 12,
                _ => 20,
            };
            add("change size", points, format!("{lines} line(s) changed"));
        }
    }
    if let Some(files) = input.changes.files_changed
        && files > 50
    {
        add("files", 5, format!("{files} files changed"));
    }
    let failed: Vec<&str> = input
        .checks
        .iter()
        .filter(|c| !c.passed)
        .map(|c| c.name.as_str())
        .collect();
    if input.checks.is_empty() {
        add(
            "checks",
            10,
            "no checks (tests, lint, scans) reported".into(),
        );
    } else if !failed.is_empty() {
        add(
            "failed checks",
            (25 * failed.len() as u32).min(50),
            format!("failed: {}", failed.join(", ")),
        );
    }
    if !input.has_sbom {
        add(
            "sbom",
            5,
            "no software bill of materials: dependencies are not known".into(),
        );
    }
    if input.gates == 0 {
        add(
            "no health gates",
            15,
            "nothing watches the rollout: problems are only found by people".into(),
        );
    }
    if input.recent_finished > 0 && input.recent_rollbacks > 0 {
        let rate = f64::from(input.recent_rollbacks) / f64::from(input.recent_finished);
        add(
            "track record",
            (rate * 30.0).round() as u32,
            format!(
                "{} of the last {} release(s) were rolled back",
                input.recent_rollbacks, input.recent_finished
            ),
        );
    }
    if let Some(days) = input.days_since_last
        && days > 60
    {
        add(
            "time since last release",
            8,
            format!("{days} days of changes accumulated"),
        );
    }
    let weekday = input.at.weekday();
    if matches!(weekday, Weekday::Sat | Weekday::Sun)
        || (weekday == Weekday::Fri && input.at.hour() >= 12)
    {
        add(
            "timing",
            10,
            "proposed before or at the weekend, when fewer people watch".into(),
        );
    }
    match input.declared {
        Some(RiskLevel::High) => add("proposer's judgement", 15, "rated high risk".into()),
        Some(RiskLevel::Medium) => add("proposer's judgement", 5, "rated medium risk".into()),
        _ => {}
    }
    let score = factors.iter().map(|f| f.points).sum::<u32>().min(100);
    Risk {
        score,
        level: RiskLevel::of(score),
        factors,
        assessment: input.assessment.trim().to_string(),
    }
}

// ---- products and releases ----------------------------------------------------------

/// Something the organisation ships. Secrets (client token, webhook key)
/// are never part of this type; the hints show which are set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Product {
    pub id: Uuid,
    pub name: String,
    pub description: String,
    /// Departments whose agents may propose releases.
    pub departments: Vec<DepartmentId>,
    /// `owner/repo` the product is built from.
    pub repository: Option<String>,
    pub plan: Vec<Stage>,
    pub gates: Vec<HealthGate>,
    /// Distinct people who must approve a release.
    pub approvals_required: u32,
    /// Hosts artifacts may be downloaded from (and verified).
    pub artifact_hosts: Vec<String>,
    pub webhook_url: Option<String>,
    pub webhook_secret_hint: Option<String>,
    pub client_token_hint: Option<String>,
    /// A change freeze: nothing starts or advances while set.
    pub frozen: Option<String>,
    pub frozen_by: Option<String>,
    pub enabled: bool,
    pub updated_at: DateTime<Utc>,
    pub updated_by: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseStatus {
    /// Waiting for people.
    Proposed,
    ChangesRequested,
    /// Going out stage by stage.
    RollingOut,
    /// Held by a person or an agent; units keep what they were offered.
    Paused,
    /// Everyone has it.
    Released,
    /// Taken back (by a person, or a failing health gate); its version is
    /// never used again.
    RolledBack,
    Rejected,
}

impl ReleaseStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Proposed => "proposed",
            Self::ChangesRequested => "changes_requested",
            Self::RollingOut => "rolling_out",
            Self::Paused => "paused",
            Self::Released => "released",
            Self::RolledBack => "rolled_back",
            Self::Rejected => "rejected",
        }
    }

    pub fn is_pending(self) -> bool {
        matches!(self, Self::Proposed | Self::ChangesRequested)
    }

    /// Being distributed right now.
    pub fn is_active(self) -> bool {
        matches!(self, Self::RollingOut | Self::Paused)
    }
}

impl std::str::FromStr for ReleaseStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "proposed" => Ok(Self::Proposed),
            "changes_requested" => Ok(Self::ChangesRequested),
            "rolling_out" => Ok(Self::RollingOut),
            "paused" => Ok(Self::Paused),
            "released" => Ok(Self::Released),
            "rolled_back" => Ok(Self::RolledBack),
            "rejected" => Ok(Self::Rejected),
            other => Err(format!("unknown release status `{other}`")),
        }
    }
}

/// A file people download. The digest is what clients verify.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Artifact {
    pub name: String,
    /// `linux-x86_64`, `macos-arm64`, `windows-x86_64`, `any`, ...
    #[serde(default = "any_platform")]
    pub platform: String,
    pub url: String,
    pub sha256: String,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub media_type: Option<String>,
    /// When the server downloaded it and found the digest (and size) right.
    #[serde(default)]
    pub verified_at: Option<DateTime<Utc>>,
}

fn any_platform() -> String {
    "any".into()
}

pub const MAX_ARTIFACTS: usize = 50;

/// The host of an `http(s)://` URL, lowercase, without port or userinfo.
pub fn url_host(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let authority = rest.split(['/', '?', '#']).next()?;
    if authority.contains('@') {
        return None;
    }
    let host = if authority.starts_with('[') {
        authority.split(']').next()?.trim_start_matches('[')
    } else {
        authority.split(':').next()?
    };
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// Whether `host` is one of `allowed` (exact, or a subdomain of a
/// `.example.com` entry).
pub fn host_allowed(host: &str, allowed: &[String]) -> bool {
    allowed.iter().any(|a| {
        let a = a.trim().to_ascii_lowercase();
        match a.strip_prefix('.') {
            Some(suffix) => host == suffix || host.ends_with(&format!(".{suffix}")),
            None => host == a,
        }
    })
}

/// Normalise and check artifacts (lowercase digests, unique names).
pub fn check_artifacts(artifacts: &mut [Artifact], hosts: &[String]) -> Result<(), String> {
    if artifacts.is_empty() {
        return Err("a release has at least one artifact (with its sha256)".into());
    }
    if artifacts.len() > MAX_ARTIFACTS {
        return Err(format!("a release has at most {MAX_ARTIFACTS} artifacts"));
    }
    let mut names = std::collections::HashSet::new();
    for a in artifacts.iter_mut() {
        a.name = a.name.trim().to_string();
        a.platform = a.platform.trim().to_ascii_lowercase();
        a.sha256 = a
            .sha256
            .trim()
            .trim_start_matches("sha256:")
            .to_ascii_lowercase();
        a.url = a.url.trim().to_string();
        if a.name.is_empty() || a.name.chars().count() > 200 || a.name.contains(['/', '\\']) {
            return Err("an artifact has a file name (no slashes, up to 200 characters)".into());
        }
        if !names.insert(a.name.clone()) {
            return Err(format!("artifact `{}` is listed twice", a.name));
        }
        if a.platform.is_empty()
            || a.platform.len() > 40
            || !a
                .platform
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        {
            return Err(format!(
                "artifact `{}`: the platform uses a-z, 0-9, -, _ and . (e.g. linux-x86_64, any)",
                a.name
            ));
        }
        if a.sha256.len() != 64 || !a.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!(
                "artifact `{}`: `sha256` is the 64-character hex digest of the file",
                a.name
            ));
        }
        let host = url_host(&a.url)
            .ok_or_else(|| format!("artifact `{}`: the url is http(s)://host/path", a.name))?;
        if !host_allowed(&host, hosts) {
            return Err(format!(
                "artifact `{}`: `{host}` is not an allowed artifact host ({})",
                a.name,
                if hosts.is_empty() {
                    "an admin has not allowed any".to_string()
                } else {
                    hosts.join(", ")
                }
            ));
        }
        a.verified_at = None;
    }
    Ok(())
}

/// A person's approval of one revision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Approval {
    pub by: String,
    pub at: DateTime<Utc>,
    pub revision: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Release {
    pub id: Uuid,
    pub product_id: Uuid,
    pub version: String,
    /// Git commit the release is built from.
    pub commit: Option<String>,
    pub notes: String,
    pub artifacts: Vec<Artifact>,
    pub sbom: Option<Value>,
    pub checks: Vec<CheckOutcome>,
    pub changes: ChangeSize,
    pub risk: Risk,
    /// The rollout plan for this release (the product's, adapted to the risk).
    pub plan: Vec<Stage>,
    pub approvals_required: u32,
    pub approvals: Vec<Approval>,
    pub status: ReleaseStatus,
    /// Index into `plan` while rolling out.
    pub stage: u32,
    pub stage_started_at: Option<DateTime<Utc>>,
    pub gates: Vec<GateResult>,
    pub gates_checked_at: Option<DateTime<Utc>>,
    pub department_id: Option<DepartmentId>,
    pub agent_id: Option<OrgAgentId>,
    pub proposed_by: String,
    pub revision: u32,
    pub history: Vec<Value>,
    pub feedback: Option<String>,
    /// Why it was rejected, paused or rolled back, and by whom.
    pub reason: Option<String>,
    pub decided_by: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub released_at: Option<DateTime<Utc>>,
    /// Signed in-toto attestation (DSSE envelope), made when it starts.
    pub attestation: Option<Value>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Release {
    /// Share of units offered this release now.
    pub fn percent(&self) -> f64 {
        match self.status {
            ReleaseStatus::Released => 100.0,
            s if s.is_active() => self
                .plan
                .get(self.stage as usize)
                .map_or(0.0, |s| s.percent),
            _ => 0.0,
        }
    }

    pub fn current_stage(&self) -> Option<&Stage> {
        self.plan.get(self.stage as usize)
    }

    pub fn parsed_version(&self) -> Option<Version> {
        self.version.parse().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn v(s: &str) -> Version {
        s.parse().unwrap()
    }

    #[test]
    fn versions_parse_and_order_like_semver() {
        assert_eq!(v("v1.2.3").to_string(), "1.2.3");
        assert_eq!(v("1.0.0-rc.1+build.7").to_string(), "1.0.0-rc.1+build.7");
        for bad in [
            "1.2",
            "1.2.3.4",
            "01.2.3",
            "1.2.3-",
            "1.2.3-a..b",
            "a.b.c",
            "1.2.3-01",
        ] {
            assert!(bad.parse::<Version>().is_err(), "{bad}");
        }
        // The ordering example from semver.org.
        let ordered = [
            "1.0.0-alpha",
            "1.0.0-alpha.1",
            "1.0.0-alpha.beta",
            "1.0.0-beta",
            "1.0.0-beta.2",
            "1.0.0-beta.11",
            "1.0.0-rc.1",
            "1.0.0",
            "1.0.1",
            "1.1.0",
            "2.0.0",
        ];
        for pair in ordered.windows(2) {
            assert!(v(pair[0]) < v(pair[1]), "{} < {}", pair[0], pair[1]);
        }
        assert_eq!(v("1.0.0+a").cmp(&v("1.0.0+b")), Ordering::Equal);
    }

    #[test]
    fn bumps() {
        assert_eq!(Bump::between(None, &v("1.0.0")), Bump::Initial);
        assert_eq!(Bump::between(Some(&v("1.4.2")), &v("2.0.0")), Bump::Major);
        assert_eq!(Bump::between(Some(&v("0.4.2")), &v("0.5.0")), Bump::Major);
        assert_eq!(Bump::between(Some(&v("1.4.2")), &v("1.5.0")), Bump::Minor);
        assert_eq!(Bump::between(Some(&v("1.4.2")), &v("1.4.3")), Bump::Patch);
        assert_eq!(
            Bump::between(Some(&v("1.5.0-rc.1")), &v("1.5.0-rc.2")),
            Bump::Prerelease
        );
    }

    #[test]
    fn plans_grow_and_end_with_everyone() {
        assert!(check_plan(&default_plan()).is_ok());
        assert!(check_plan(&[]).is_err());
        assert!(check_plan(&[Stage::new("a", 50.0, 0)]).is_err());
        assert!(check_plan(&[Stage::new("a", 50.0, 0), Stage::new("b", 50.0, 0)]).is_err());
        assert!(check_plan(&[Stage::new("a", 10.0, 0), Stage::new("b", 100.0, 0)]).is_ok());
    }

    #[test]
    fn risky_releases_go_slower() {
        let plan = default_plan();
        assert_eq!(adapt_plan(&plan, RiskLevel::Low), plan);
        let medium = adapt_plan(&plan, RiskLevel::Medium);
        assert_eq!(medium.len(), 4);
        assert_eq!(medium[0].soak_minutes, 120);
        assert_eq!(medium[3].soak_minutes, 0);
        let high = adapt_plan(&plan, RiskLevel::High);
        assert_eq!(high[0].name, "pilot");
        assert_eq!(high[0].percent, 0.25);
        assert_eq!(high[1].soak_minutes, 180);
        assert!(check_plan(&high).is_ok());
        assert_eq!(extra_approvals(RiskLevel::High), 1);
    }

    #[test]
    fn cohorts_are_stable_and_proportional() {
        let release = Uuid::from_u128(42);
        let n = 20_000;
        let ten = (0..n)
            .filter(|i| in_rollout(release, &format!("unit-{i}"), 10.0))
            .count();
        assert!((1700..2300).contains(&ten), "{ten} of {n} at 10%");
        // Growing the rollout keeps everyone who already had it.
        for i in 0..1000 {
            let unit = format!("unit-{i}");
            if in_rollout(release, &unit, 10.0) {
                assert!(in_rollout(release, &unit, 50.0));
            }
            assert!(in_rollout(release, &unit, 100.0));
            assert!(!in_rollout(release, &unit, 0.0));
        }
        // Another release picks other units first.
        let other = Uuid::from_u128(43);
        let same = (0..1000)
            .filter(|i| {
                let u = format!("unit-{i}");
                in_rollout(release, &u, 10.0) && in_rollout(other, &u, 10.0)
            })
            .count();
        assert!(same < 40, "{same}");
    }

    fn input<'a>(changes: &'a ChangeSize, checks: &'a [CheckOutcome]) -> RiskInput<'a> {
        RiskInput {
            bump: Bump::Patch,
            changes,
            checks,
            has_sbom: true,
            gates: 1,
            recent_finished: 0,
            recent_rollbacks: 0,
            days_since_last: Some(3),
            // A Tuesday morning.
            at: Utc.with_ymd_and_hms(2026, 10, 6, 9, 0, 0).unwrap(),
            declared: None,
            assessment: " small fix ".into(),
        }
    }

    #[test]
    fn risk_is_explained_by_its_factors() {
        let small = ChangeSize {
            lines_added: Some(20),
            lines_removed: Some(5),
            ..Default::default()
        };
        let passed = [CheckOutcome {
            name: "tests".into(),
            passed: true,
            detail: String::new(),
        }];
        let routine = assess(&input(&small, &passed));
        assert_eq!(routine.level, RiskLevel::Low);
        assert_eq!(routine.score, 3);
        assert_eq!(routine.assessment, "small fix");

        let big = ChangeSize {
            lines_added: Some(5000),
            ..Default::default()
        };
        let failed = [CheckOutcome {
            name: "e2e".into(),
            passed: false,
            detail: String::new(),
        }];
        let mut risky = input(&big, &failed);
        risky.bump = Bump::Major;
        risky.gates = 0;
        risky.recent_finished = 4;
        risky.recent_rollbacks = 2;
        risky.at = Utc.with_ymd_and_hms(2026, 10, 9, 16, 0, 0).unwrap(); // Friday
        risky.declared = Some(RiskLevel::High);
        let r = assess(&risky);
        assert_eq!(r.level, RiskLevel::High);
        assert_eq!(r.score, 100);
        let names: Vec<&str> = r.factors.iter().map(|f| f.name.as_str()).collect();
        for n in [
            "version",
            "change size",
            "failed checks",
            "no health gates",
            "track record",
            "timing",
            "proposer's judgement",
        ] {
            assert!(names.contains(&n), "{n} in {names:?}");
        }
    }

    #[test]
    fn gates_judge_and_read_numbers() {
        let gate = HealthGate {
            name: "crash-free".into(),
            metric: GateSource::CrashFree,
            comparator: Comparator::Gte,
            threshold: 0.99,
            min_samples: 10,
        };
        assert_eq!(
            GateResult::judge(&gate, 1.0, Some(3)).status,
            GateStatus::Waiting
        );
        assert_eq!(
            GateResult::judge(&gate, 0.995, Some(10)).status,
            GateStatus::Pass
        );
        assert_eq!(
            GateResult::judge(&gate, 0.9, Some(10)).status,
            GateStatus::Fail
        );
        assert!(check_gates(&[gate]).is_ok());
        let parsed: HealthGate = serde_json::from_value(serde_json::json!({
            "name": "errors", "comparator": "<",  "threshold": 0.02,
            "metric": { "kind": "data", "source": "metrics", "sql": "SELECT 1 AS rate", "field": "rate" },
        }))
        .unwrap();
        assert_eq!(parsed.comparator, Comparator::Lt);
        assert!(check_gates(&[parsed]).is_ok());

        let rows = serde_json::json!({ "rows": [{ "rate": 0.5, "n": "7" }] });
        assert_eq!(extract_number(&rows, "rate").unwrap(), 0.5);
        assert_eq!(extract_number(&rows, "n").unwrap(), 7.0);
        assert_eq!(extract_number(&rows, "/rows/0/rate").unwrap(), 0.5);
        assert!(extract_number(&rows, "missing").is_err());
        let http = serde_json::json!({ "status": 200, "body": { "p99": 120 } });
        assert_eq!(extract_number(&http, "p99").unwrap(), 120.0);
    }

    #[test]
    fn artifacts_are_checked_and_normalised() {
        let hosts = vec!["downloads.acme.com".to_string(), ".github.com".to_string()];
        let artifact = |url: &str, sha: &str| Artifact {
            name: "app.tar.gz".into(),
            platform: "Linux-x86_64".into(),
            url: url.into(),
            sha256: sha.into(),
            size: None,
            media_type: None,
            verified_at: None,
        };
        let sha = format!("sha256:{}", "AB".repeat(32));
        let mut ok = vec![artifact("https://downloads.acme.com/app.tar.gz", &sha)];
        check_artifacts(&mut ok, &hosts).unwrap();
        assert_eq!(ok[0].sha256, "ab".repeat(32));
        assert_eq!(ok[0].platform, "linux-x86_64");
        let mut sub = vec![artifact("https://objects.github.com/x", &sha)];
        assert!(check_artifacts(&mut sub, &hosts).is_ok());
        for url in [
            "https://evil.com/app",
            "https://downloads.acme.com.evil.com/app",
            "https://user@downloads.acme.com/app",
            "ftp://downloads.acme.com/app",
        ] {
            let mut bad = vec![artifact(url, &sha)];
            assert!(check_artifacts(&mut bad, &hosts).is_err(), "{url}");
        }
        let mut short = vec![artifact("https://downloads.acme.com/a", "abc")];
        assert!(check_artifacts(&mut short, &hosts).is_err());
        assert!(check_artifacts(&mut [], &hosts).is_err());
        assert_eq!(
            url_host("http://127.0.0.1:8080/x").as_deref(),
            Some("127.0.0.1")
        );
    }
}
