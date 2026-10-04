use super::*;

const SAMPLE: &str = r#"
name = "test"
default = "deny"

[[rules]]
id = "read-workspace"
effect = "allow"
kinds = ["file_read"]
paths = ["/workspace/**"]

[[rules]]
id = "no-secrets"
description = "secrets are off limits"
effect = "deny"
kinds = ["file_read", "file_write"]
paths = ["**/.env", "**/*.pem"]

[[rules]]
id = "cargo"
effect = "allow"
kinds = ["exec"]
commands = ["cargo build*", "cargo test*"]

[[rules]]
id = "push-needs-human"
effect = "require_approval"
kinds = ["exec"]
commands = ["git push*"]

[[rules]]
id = "git"
effect = "allow"
kinds = ["exec"]
commands = ["git *"]

[[rules]]
id = "github"
effect = "allow"
kinds = ["network"]
hosts = ["github.com", "*.github.com"]
"#;

fn policy() -> CompiledPolicy {
    Policy::from_toml(SAMPLE, "test")
        .unwrap()
        .compile()
        .unwrap()
}

fn read(path: &str) -> Action {
    normalize_action(&Action::FileRead { path: path.into() }, "/workspace")
}

fn exec(cmd: &str) -> Action {
    let mut parts = cmd.split(' ').map(String::from);
    Action::Exec {
        command: parts.next().unwrap(),
        args: parts.collect(),
        cwd: None,
    }
}

#[test]
fn allows_workspace_reads() {
    assert!(matches!(
        policy().evaluate(&read("src/main.rs")),
        Verdict::Allow { .. }
    ));
}

#[test]
fn deny_overrides_allow() {
    let verdict = policy().evaluate(&read("config/.env"));
    assert_eq!(
        verdict,
        Verdict::Deny {
            rule: Some("no-secrets".into()),
            reason: "secrets are off limits".into()
        }
    );
}

#[test]
fn parent_dir_traversal_is_normalised() {
    // Without normalisation `/workspace/../etc/passwd` would match `/workspace/**`.
    let action = read("../etc/passwd");
    assert_eq!(
        action,
        Action::FileRead {
            path: "/etc/passwd".into()
        }
    );
    assert!(matches!(
        policy().evaluate(&action),
        Verdict::Deny { rule: None, .. }
    ));
}

#[test]
fn approval_overrides_allow() {
    let verdict = policy().evaluate(&exec("git push origin main"));
    assert_eq!(verdict.rule(), Some("push-needs-human"));
    assert!(matches!(verdict, Verdict::RequireApproval { .. }));
    assert_eq!(policy().evaluate(&exec("git status")).rule(), Some("git"));
}

#[test]
fn falls_back_to_default() {
    assert!(matches!(
        policy().evaluate(&exec("curl evil.example")),
        Verdict::Deny { .. }
    ));
}

#[test]
fn hosts_are_case_insensitive() {
    let action = normalize_action(
        &Action::Network {
            host: "API.GitHub.com.".into(),
            port: Some(443),
        },
        "/workspace",
    );
    assert_eq!(policy().evaluate(&action).rule(), Some("github"));
}

#[test]
fn rejects_duplicate_rule_ids() {
    let src = r#"
name = "dup"
[[rules]]
id = "a"
effect = "allow"
[[rules]]
id = "a"
effect = "deny"
"#;
    let err = Policy::from_toml(src, "dup")
        .unwrap()
        .compile()
        .unwrap_err();
    assert!(err.to_string().contains("duplicate rule id"));
}

#[test]
fn rejects_unknown_fields() {
    let src = "name = \"x\"\n[[rules]]\nid = \"a\"\neffect = \"allow\"\npathz = [\"/\"]\n";
    assert!(Policy::from_toml(src, "x").is_err());
}

#[test]
fn digest_is_stable() {
    assert_eq!(policy().digest(), policy().digest());
    assert_eq!(policy().digest().len(), 64);
}

#[test]
fn bundled_policies_compile() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../policies");
    let set = PolicySet::load_dir(&dir).unwrap();
    assert!(set.get("default").is_some());
}
