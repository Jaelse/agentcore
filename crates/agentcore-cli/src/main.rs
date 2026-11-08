use std::path::PathBuf;

use agentcore_core::Action;
use agentcore_policy::{PolicySet, normalize_action};
use anyhow::Context;
use clap::{Parser, Subcommand, ValueEnum};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

#[derive(Parser)]
#[command(
    name = "agentcore",
    version,
    about = "Secure, observable runtime for AI agents"
)]
struct Cli {
    /// Log format for operational logs.
    #[arg(long, global = true, value_enum, default_value_t = LogFormat::Pretty, env = "AGENTCORE_LOG_FORMAT")]
    log_format: LogFormat,
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
enum LogFormat {
    Pretty,
    Json,
}

#[derive(Subcommand)]
enum Command {
    /// Run the server (API, MCP gateway and web UI).
    Serve {
        #[arg(
            short,
            long,
            default_value = "agentcore.toml",
            env = "AGENTCORE_CONFIG"
        )]
        config: PathBuf,
    },
    /// Policy tooling.
    Policy {
        #[command(subcommand)]
        command: PolicyCommand,
    },
    /// Audit log tooling.
    Audit {
        #[command(subcommand)]
        command: AuditCommand,
    },
    /// Print the SHA-256 of an operator token for `[[server.operators]]`.
    HashToken { token: String },
}

#[derive(Subcommand)]
enum PolicyCommand {
    /// Validate every policy in a directory.
    Check {
        #[arg(default_value = "policies")]
        dir: PathBuf,
    },
    /// Show the verdict a policy gives an action, e.g.
    /// `agentcore policy eval default '{"type":"exec","command":"git","args":["push"]}'`.
    Eval {
        policy: String,
        action: String,
        #[arg(long, default_value = "policies")]
        dir: PathBuf,
    },
}

#[derive(Subcommand)]
enum AuditCommand {
    /// Verify the hash chain of one or more audit files.
    Verify { files: Vec<PathBuf> },
    /// Print the events of an audit file (verifying it first).
    Show { file: PathBuf },
}

fn init_tracing(format: LogFormat) {
    let filter = EnvFilter::try_from_env("AGENTCORE_LOG")
        .unwrap_or_else(|_| EnvFilter::new("info,tower_http=info"));
    // Besides printing, each node keeps its log for the log export.
    let registry = tracing_subscriber::registry()
        .with(filter)
        .with(agentcore_server::logs::capture());
    match format {
        LogFormat::Pretty => registry.with(tracing_subscriber::fmt::layer()).init(),
        LogFormat::Json => registry
            .with(tracing_subscriber::fmt::layer().json().flatten_event(true))
            .init(),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.log_format);
    match cli.command {
        Command::Serve { config } => {
            let config = agentcore_server::Config::load(&config)?;
            agentcore_server::serve(config).await
        }
        Command::Policy {
            command: PolicyCommand::Check { dir },
        } => {
            let set = PolicySet::load_dir(&dir)?;
            for policy in set.iter() {
                println!(
                    "ok  {:<16} {} rules  sha256:{}",
                    policy.name(),
                    policy.policy().rules.len(),
                    policy.digest()
                );
            }
            Ok(())
        }
        Command::Policy {
            command:
                PolicyCommand::Eval {
                    policy,
                    action,
                    dir,
                },
        } => {
            let set = PolicySet::load_dir(&dir)?;
            let policy = set
                .get(&policy)
                .with_context(|| format!("unknown policy `{policy}`"))?;
            let action: Action = serde_json::from_str(&action).context("parsing action JSON")?;
            let action = normalize_action(&action, "/workspace");
            println!("action:  {}", action.summary());
            println!(
                "verdict: {}",
                serde_json::to_string_pretty(&policy.evaluate(&action))?
            );
            Ok(())
        }
        Command::Audit {
            command: AuditCommand::Verify { files },
        } => {
            let mut failed = false;
            for file in files {
                match agentcore_audit::verify_file(&file) {
                    Ok(head) => println!(
                        "ok    {} ({} records, head {})",
                        file.display(),
                        head.records,
                        head.hash
                    ),
                    Err(err) => {
                        failed = true;
                        println!("FAIL  {err}");
                    }
                }
            }
            if failed {
                anyhow::bail!("audit verification failed");
            }
            Ok(())
        }
        Command::Audit {
            command: AuditCommand::Show { file },
        } => {
            for record in agentcore_audit::read_events(&file)? {
                println!("{}", serde_json::to_string(&record.event)?);
            }
            Ok(())
        }
        Command::HashToken { token } => {
            println!("{}", agentcore_server::auth::hash_token(&token));
            Ok(())
        }
    }
}
