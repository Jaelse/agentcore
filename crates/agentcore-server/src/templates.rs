//! Building an organisation from templates.
//!
//! * **Department templates** (`templates/departments/*.toml`): a ready-made
//!   department for a business function (engineering, support, finance, ...)
//!   with its mission, tools and agents. Agents marked `core` make up a lean
//!   start; the others are added for a full start or suggested later.
//! * **Blueprints** (`templates/blueprints/*.toml`): growth paths. Each is an
//!   ordered list of stages that add a few departments at a time, from a
//!   single engineering department to a complete company.
//!
//! [`plan`] turns a choice of templates into the departments and agents to
//! create (checking the limits); [`suggest`] looks at the organisation as it
//! is and proposes what to add next.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use agentcore_core::org::TOOL_GROUPS;
use agentcore_core::{Department, OrgAgent, OrgProfile, OrgSettings};
use anyhow::Context;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Categories of business functions, in the order they are shown.
pub const CATEGORIES: [(&str, &str, &str); 5] = [
    (
        "build",
        "Product & engineering",
        "Decide what to make and make it.",
    ),
    ("grow", "Growth", "Get known, win customers, grow."),
    ("serve", "Customers", "Help customers and keep them."),
    (
        "run",
        "Operations",
        "Money, contracts, people and processes.",
    ),
    ("lead", "Leadership", "Direction, coordination and insight."),
];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TemplateAgent {
    pub name: String,
    pub title: String,
    #[serde(default)]
    pub core: bool,
    pub instructions: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DepartmentTemplate {
    pub id: String,
    pub name: String,
    pub category: String,
    pub summary: String,
    /// When a business usually needs this department.
    pub when_to_add: String,
    pub mission: String,
    #[serde(default)]
    pub tools: Vec<String>,
    /// Policy for its workers; the server default for departments if unset.
    #[serde(default)]
    pub policy: Option<String>,
    /// Departments it usually works with (template ids).
    #[serde(default)]
    pub pairs_with: Vec<String>,
    pub agents: Vec<TemplateAgent>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    /// One department to start with, growing from there.
    Starter,
    /// A small team first, more in later stages.
    Growing,
    /// Everything, for those who want the whole organisation at once.
    Complete,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stage {
    pub title: String,
    #[serde(default)]
    pub description: String,
    pub departments: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Blueprint {
    pub id: String,
    pub title: String,
    pub level: Level,
    /// `software`, `business`, `services`, `marketing`, ...
    pub focus: String,
    pub audience: String,
    pub description: String,
    pub stages: Vec<Stage>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DepartmentFile {
    departments: Vec<DepartmentTemplate>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Catalog {
    pub departments: Vec<DepartmentTemplate>,
    pub blueprints: Vec<Blueprint>,
}

fn toml_files(dir: &Path) -> anyhow::Result<Vec<std::path::PathBuf>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
        .collect();
    files.sort();
    Ok(files)
}

impl Catalog {
    /// Load `dir/departments/*.toml` and `dir/blueprints/*.toml`. A missing
    /// directory gives an empty catalogue.
    pub fn load_dir(dir: &Path) -> anyhow::Result<Self> {
        let mut catalog = Self::default();
        for file in toml_files(&dir.join("departments"))? {
            let src = std::fs::read_to_string(&file)?;
            let parsed: DepartmentFile =
                toml::from_str(&src).with_context(|| format!("parsing {}", file.display()))?;
            catalog.departments.extend(parsed.departments);
        }
        for file in toml_files(&dir.join("blueprints"))? {
            let src = std::fs::read_to_string(&file)?;
            catalog
                .blueprints
                .push(toml::from_str(&src).with_context(|| format!("parsing {}", file.display()))?);
        }
        catalog.validate()?;
        // Smallest first: starters, then growing paths, then complete.
        catalog.blueprints.sort_by_key(|b| {
            (
                b.level as u8,
                b.stages.iter().map(|s| s.departments.len()).sum::<usize>(),
            )
        });
        Ok(catalog)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        let mut ids = HashSet::new();
        for t in &self.departments {
            anyhow::ensure!(
                ids.insert(t.id.as_str()),
                "duplicate department template `{}`",
                t.id
            );
            anyhow::ensure!(
                CATEGORIES.iter().any(|(c, _, _)| *c == t.category),
                "template `{}`: unknown category `{}`",
                t.id,
                t.category
            );
            for tool in &t.tools {
                anyhow::ensure!(
                    TOOL_GROUPS.contains(&tool.as_str()),
                    "template `{}`: unknown tool group `{tool}`",
                    t.id
                );
            }
            anyhow::ensure!(
                t.agents.iter().any(|a| a.core),
                "template `{}`: at least one agent must be `core`",
                t.id
            );
            let mut names = HashSet::new();
            for a in &t.agents {
                anyhow::ensure!(
                    agentcore_store::valid_agent_name(&a.name)
                        && !agentcore_store::RESERVED_NAMES.contains(&a.name.as_str()),
                    "template `{}`: invalid agent name `{}`",
                    t.id,
                    a.name
                );
                anyhow::ensure!(
                    names.insert(a.name.as_str()),
                    "template `{}`: duplicate agent `{}`",
                    t.id,
                    a.name
                );
            }
        }
        for t in &self.departments {
            for other in &t.pairs_with {
                anyhow::ensure!(
                    ids.contains(other.as_str()),
                    "template `{}` pairs with unknown template `{other}`",
                    t.id
                );
            }
        }
        let mut blueprints = HashSet::new();
        for b in &self.blueprints {
            anyhow::ensure!(
                blueprints.insert(b.id.as_str()),
                "duplicate blueprint `{}`",
                b.id
            );
            anyhow::ensure!(!b.stages.is_empty(), "blueprint `{}` has no stages", b.id);
            let mut seen = HashSet::new();
            for stage in &b.stages {
                for d in &stage.departments {
                    anyhow::ensure!(
                        ids.contains(d.as_str()),
                        "blueprint `{}` uses unknown template `{d}`",
                        b.id
                    );
                    anyhow::ensure!(
                        seen.insert(d.as_str()),
                        "blueprint `{}` lists `{d}` twice",
                        b.id
                    );
                }
            }
        }
        Ok(())
    }

    pub fn department(&self, id: &str) -> Option<&DepartmentTemplate> {
        self.departments.iter().find(|t| t.id == id)
    }

    pub fn blueprint(&self, id: &str) -> Option<&Blueprint> {
        self.blueprints.iter().find(|b| b.id == id)
    }

    /// Policies the templates refer to (checked against the policy set).
    pub fn policies(&self) -> impl Iterator<Item = &str> {
        self.departments.iter().filter_map(|t| t.policy.as_deref())
    }
}

/// How many agents each new department starts with.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Size {
    /// Only the core agents: start small, add more later.
    #[default]
    Lean,
    /// Every agent of the template.
    Full,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlannedAgent {
    pub name: String,
    pub title: String,
    pub instructions: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlannedDepartment {
    pub template: String,
    pub name: String,
    pub description: String,
    pub mission: String,
    pub tools: Vec<String>,
    pub policy: Option<String>,
    pub agents: Vec<PlannedAgent>,
    /// Already in the organisation: it is left as it is.
    pub exists: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Plan {
    pub departments: Vec<PlannedDepartment>,
    /// Departments that would be created.
    pub new_departments: usize,
    /// Worker agents that would be created (each department also gets a
    /// communicator).
    pub new_agents: usize,
    /// Current limits.
    pub limits: OrgSettings,
    /// Limits this plan needs.
    pub needs: OrgSettings,
    pub fits: bool,
}

/// `{company}` → the company name.
pub fn fill(text: &str, profile: &OrgProfile) -> String {
    let company = profile.company_name.trim();
    text.replace(
        "{company}",
        if company.is_empty() {
            "the company"
        } else {
            company
        },
    )
    .trim()
    .to_string()
}

fn same_department(template: &DepartmentTemplate, dept: &Department) -> bool {
    dept.template.as_deref() == Some(template.id.as_str())
        || dept.name.eq_ignore_ascii_case(&template.name)
}

fn agents_for(template: &DepartmentTemplate, size: Size) -> Vec<&TemplateAgent> {
    template
        .agents
        .iter()
        .filter(|a| size == Size::Full || a.core)
        .collect()
}

/// What creating these templates would add, and whether the limits allow it.
pub fn plan(
    catalog: &Catalog,
    ids: &[String],
    size: Size,
    profile: &OrgProfile,
    existing: &[Department],
    limits: OrgSettings,
) -> Result<Plan, String> {
    let mut seen = HashSet::new();
    let mut departments = Vec::new();
    for id in ids {
        if !seen.insert(id.as_str()) {
            continue;
        }
        let template = catalog
            .department(id)
            .ok_or_else(|| format!("unknown department template `{id}`"))?;
        departments.push(PlannedDepartment {
            template: template.id.clone(),
            name: template.name.clone(),
            description: template.summary.clone(),
            mission: fill(&template.mission, profile),
            tools: template.tools.clone(),
            policy: template.policy.clone(),
            agents: agents_for(template, size)
                .into_iter()
                .map(|a| PlannedAgent {
                    name: a.name.clone(),
                    title: a.title.clone(),
                    instructions: fill(&a.instructions, profile),
                })
                .collect(),
            exists: existing.iter().any(|d| same_department(template, d)),
        });
    }
    let new: Vec<&PlannedDepartment> = departments.iter().filter(|d| !d.exists).collect();
    let needs = OrgSettings {
        max_departments: (existing.len() + new.len()) as u32,
        max_agents_per_department: new.iter().map(|d| d.agents.len()).max().unwrap_or(0) as u32,
    };
    let fits = needs.max_departments <= limits.max_departments
        && needs.max_agents_per_department <= limits.max_agents_per_department;
    Ok(Plan {
        new_departments: new.len(),
        new_agents: new.iter().map(|d| d.agents.len()).sum(),
        departments,
        limits,
        needs: OrgSettings {
            max_departments: needs.max_departments.max(limits.max_departments),
            max_agents_per_department: needs
                .max_agents_per_department
                .max(limits.max_agents_per_department),
        },
        fits,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SuggestionKind {
    /// The next stage of the chosen blueprint.
    Stage,
    /// A department that works with ones the organisation already has.
    Department,
    /// An agent a department's template has and the department does not.
    Agent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Suggestion {
    pub kind: SuggestionKind,
    pub title: String,
    pub reason: String,
    /// Templates to create (stage, department).
    pub templates: Vec<String>,
    /// The existing department to add an agent to.
    pub department_id: Option<Uuid>,
    pub agent: Option<PlannedAgent>,
    /// Why it cannot be done right now (a limit), if so.
    pub blocked: Option<String>,
}

/// What to add next: the blueprint's next stage, departments that work with
/// the existing ones, and agents the existing departments do not have yet.
pub fn suggest(
    catalog: &Catalog,
    profile: &OrgProfile,
    departments: &[Department],
    agents: &[OrgAgent],
    limits: OrgSettings,
) -> Vec<Suggestion> {
    let have = |id: &str| {
        catalog
            .department(id)
            .is_some_and(|t| departments.iter().any(|d| same_department(t, d)))
    };
    let room = limits.max_departments as usize > departments.len();
    let dept_limit = |n: usize| {
        (departments.len() + n > limits.max_departments as usize).then(|| {
            format!(
                "the organisation is limited to {} departments; an admin can raise the limit",
                limits.max_departments
            )
        })
    };
    let mut out = Vec::new();
    let mut offered: HashSet<String> = HashSet::new();

    // 1. The next stage of the chosen growth path.
    if let Some(bp) = profile
        .blueprint
        .as_deref()
        .and_then(|b| catalog.blueprint(b))
    {
        let total = bp.stages.len();
        if let Some((i, stage)) = bp
            .stages
            .iter()
            .enumerate()
            .find(|(_, s)| s.departments.iter().any(|d| !have(d)))
        {
            let missing: Vec<String> = stage
                .departments
                .iter()
                .filter(|d| !have(d))
                .cloned()
                .collect();
            let names: Vec<&str> = missing
                .iter()
                .filter_map(|d| catalog.department(d).map(|t| t.name.as_str()))
                .collect();
            offered.extend(missing.iter().cloned());
            out.push(Suggestion {
                kind: SuggestionKind::Stage,
                title: format!(
                    "Stage {} of {total} · {}: {}",
                    i + 1,
                    stage.title,
                    names.join(", ")
                ),
                reason: if stage.description.is_empty() {
                    format!("Next step of the {} path.", bp.title)
                } else {
                    format!("{} (next step of the {} path)", stage.description, bp.title)
                },
                blocked: dept_limit(missing.len()),
                templates: missing,
                department_id: None,
                agent: None,
            });
        }
    }

    // 2. Departments the existing ones usually work with, most wanted first.
    let mut wanted: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for dept in departments {
        let Some(template) = dept.template.as_deref().and_then(|t| catalog.department(t)) else {
            continue;
        };
        for other in &template.pairs_with {
            if !have(other) && !offered.contains(other) {
                wanted.entry(other).or_default().push(dept.name.as_str());
            }
        }
    }
    let mut wanted: Vec<_> = wanted.into_iter().collect();
    wanted.sort_by_key(|(id, by)| (std::cmp::Reverse(by.len()), *id));
    for (id, by) in wanted.into_iter().take(4) {
        let Some(t) = catalog.department(id) else {
            continue;
        };
        out.push(Suggestion {
            kind: SuggestionKind::Department,
            title: format!("Add {}", t.name),
            reason: format!(
                "{} {} usually work with it. {}",
                fill(&t.when_to_add, profile),
                by.join(" and "),
                t.summary
            ),
            templates: vec![t.id.clone()],
            department_id: None,
            agent: None,
            blocked: (!room).then(|| dept_limit(1)).flatten(),
        });
    }

    // 3. Grow existing departments with the template's other agents.
    for dept in departments {
        let Some(template) = dept.template.as_deref().and_then(|t| catalog.department(t)) else {
            continue;
        };
        let members: Vec<&OrgAgent> = agents
            .iter()
            .filter(|a| a.department_id == dept.id && a.kind == agentcore_core::AgentKind::Worker)
            .collect();
        let full = members.len() >= limits.max_agents_per_department as usize;
        for a in &template.agents {
            if members.iter().any(|m| m.name == a.name) {
                continue;
            }
            out.push(Suggestion {
                kind: SuggestionKind::Agent,
                title: format!("Add a {} to {}", a.title.to_lowercase(), dept.name),
                reason: fill(&a.instructions, profile)
                    .split(". ")
                    .next()
                    .unwrap_or_default()
                    .trim_end_matches('.')
                    .to_string()
                    + ".",
                templates: Vec::new(),
                department_id: Some(dept.id),
                agent: Some(PlannedAgent {
                    name: a.name.clone(),
                    title: a.title.clone(),
                    instructions: fill(&a.instructions, profile),
                }),
                blocked: full.then(|| {
                    format!(
                        "{} already has the maximum of {} agents",
                        dept.name, limits.max_agents_per_department
                    )
                }),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> Catalog {
        Catalog::load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../templates")).unwrap()
    }

    fn dept(template: &str, name: &str) -> Department {
        Department {
            id: Uuid::now_v7(),
            name: name.into(),
            description: String::new(),
            mission: String::new(),
            policy: "department".into(),
            tools: vec![],
            communicator_agent: "x".into(),
            state: agentcore_core::DepartmentState::Active,
            template: Some(template.into()),
            created_at: chrono::Utc::now(),
            updated_by: "a".into(),
        }
    }

    const LIMITS: OrgSettings = OrgSettings {
        max_departments: 10,
        max_agents_per_department: 10,
    };

    #[test]
    fn bundled_templates_are_valid_and_cover_a_business() {
        let c = catalog();
        assert!(c.departments.len() >= 25, "{}", c.departments.len());
        for (category, _, _) in CATEGORIES {
            assert!(
                c.departments.iter().any(|d| d.category == category),
                "{category}"
            );
        }
        assert_eq!(c.blueprints[0].level, Level::Starter);
        assert_eq!(c.blueprints.last().unwrap().id, "complete-company");
        // Every department but a niche one is somewhere on the complete path.
        let complete = c.blueprint("complete-company").unwrap();
        let covered: usize = complete.stages.iter().map(|s| s.departments.len()).sum();
        assert!(covered + 1 >= c.departments.len());
    }

    #[test]
    fn validation_rejects_broken_templates() {
        let mut c = catalog();
        c.blueprints[0].stages[0].departments.push("nope".into());
        assert!(c.validate().is_err());
        let mut c = catalog();
        c.departments[0]
            .agents
            .iter_mut()
            .for_each(|a| a.core = false);
        assert!(c.validate().is_err());
        let mut c = catalog();
        c.departments[0].agents[0].name = "everyone".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn lean_and_full_plans_and_limits() {
        let c = catalog();
        let profile = OrgProfile {
            company_name: "Acme".into(),
            ..Default::default()
        };
        let lean = plan(
            &c,
            &["engineering".into()],
            Size::Lean,
            &profile,
            &[],
            LIMITS,
        )
        .unwrap();
        assert_eq!(lean.new_departments, 1);
        assert_eq!(lean.departments[0].agents.len(), 2);
        assert!(lean.departments[0].mission.contains("Acme"));
        assert!(lean.fits);
        let full = plan(
            &c,
            &["engineering".into()],
            Size::Full,
            &profile,
            &[],
            LIMITS,
        )
        .unwrap();
        assert_eq!(full.new_agents, 3);

        // Existing departments are skipped.
        let existing = [dept("engineering", "Engineering")];
        let again = plan(
            &c,
            &["engineering".into(), "qa".into()],
            Size::Lean,
            &profile,
            &existing,
            LIMITS,
        )
        .unwrap();
        assert!(again.departments[0].exists);
        assert_eq!(again.new_departments, 1);

        // The complete company does not fit the default limits.
        let all: Vec<String> = c
            .blueprint("complete-company")
            .unwrap()
            .stages
            .iter()
            .flat_map(|s| s.departments.clone())
            .collect();
        let big = plan(&c, &all, Size::Full, &profile, &[], LIMITS).unwrap();
        assert!(!big.fits);
        assert_eq!(big.needs.max_departments as usize, all.len());
        assert!(plan(&c, &["nope".into()], Size::Lean, &profile, &[], LIMITS).is_err());
    }

    #[test]
    fn suggestions_follow_the_blueprint_then_neighbours_then_agents() {
        let c = catalog();
        let profile = OrgProfile {
            blueprint: Some("solo-developer".into()),
            ..Default::default()
        };
        let eng = dept("engineering", "Engineering");
        let lead = OrgAgent {
            id: Uuid::now_v7(),
            department_id: eng.id,
            name: "lead".into(),
            kind: agentcore_core::AgentKind::Worker,
            agent: "x".into(),
            instructions: String::new(),
            desired: agentcore_core::Desired::Stopped,
            node: None,
            session_id: None,
            status: None,
            note: None,
            changed_by: "a".into(),
            created_at: chrono::Utc::now(),
        };
        let s = suggest(
            &c,
            &profile,
            std::slice::from_ref(&eng),
            std::slice::from_ref(&lead),
            LIMITS,
        );
        assert_eq!(s[0].kind, SuggestionKind::Stage);
        assert_eq!(s[0].templates, ["qa"]);
        assert!(s[0].title.starts_with("Stage 2 of 3"));
        // QA is offered by the stage, so not again as a neighbour.
        assert!(
            s.iter()
                .filter(|x| x.kind == SuggestionKind::Department)
                .all(|x| x.templates != ["qa"])
        );
        assert!(s.iter().any(|x| x.templates == ["product"]));
        let agents: Vec<_> = s
            .iter()
            .filter_map(|x| x.agent.as_ref().map(|a| a.name.as_str()))
            .collect();
        assert_eq!(agents, ["developer", "reviewer"]);

        // At the limits, suggestions say why they cannot be done.
        let tight = OrgSettings {
            max_departments: 1,
            max_agents_per_department: 1,
        };
        let s = suggest(&c, &profile, &[eng], &[lead], tight);
        assert!(s.iter().all(|x| x.blocked.is_some()));
    }
}
