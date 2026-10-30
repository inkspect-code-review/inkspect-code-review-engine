//! Inkspect Rust review engine.
//!
//! A single-call AI code reviewer:
//!   1. clone the PR repo into a sandbox,
//!   2. diff the merge-base against the PR head, minus paths excluded by the
//!      base revision's `.inkspectignore`,
//!   3. retrieve + rerank related code from the shared Qdrant index (RAG),
//!   4. one hardened, prompt-injection-resistant model call returns findings
//!      and a diagram together,
//!   5. (new vs. the Go engine) post findings as inline PR comments.
//!
//! CLI:
//!   inkspect-engine review --config host.json --job job.json [--out report.json]
//!
//! Credentials come from the environment, never from the config or job files:
//!   INKSPECT_GITHUB_READ_TOKEN   (required) installation token for reads/clone
//!   INKSPECT_GITHUB_WRITE_TOKEN  (optional) token with pull_requests:write
//!   INKSPECT_QDRANT_URL / INKSPECT_QDRANT_API_KEY  (optional) RAG index
//!   AWS credential chain          for Bedrock (model, embed, rerank).
//!   Set AWS_BEARER_TOKEN_BEDROCK to a Bedrock API key to use bearer-token
//!   auth; otherwise the standard AWS credential chain applies.

mod audit;
mod local;
mod byok;
mod fix;
mod config;
mod domain;
mod error;
mod git;
mod github;
mod github_app;
mod ignore;
mod model;
mod pipeline;
mod prompts;
mod provider_usage;
mod render;
mod retrieval;
mod review;
mod sandbox;
mod service;
mod telemetry;

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;

use config::{HostConfig, ReviewJob};

#[derive(Parser)]
#[command(name = "inkspect-engine", version = render::ENGINE_VERSION, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Review a local working tree or folder without GitHub access.
    Local {
        #[arg(long)] config: PathBuf,
        #[arg(long, default_value = ".")] repo: PathBuf,
        /// Compare the working tree to this local commit/branch (default HEAD).
        #[arg(long)] base: Option<String>,
        #[arg(long)] out: Option<PathBuf>,
        #[arg(long, default_value = "markdown", value_parser = ["json", "markdown", "md"])] format: String,
    },
    /// Authenticated API dispatch for indexing, chat retrieval and source reads.
    Service {
        #[arg(long)] config: PathBuf,
        #[arg(long)] job: PathBuf,
        #[arg(long)] request: PathBuf,
        #[arg(long, value_parser = ["index", "context", "source", "describe", "fix"])] mode: String,
        #[arg(long)] out: PathBuf,
    },
    /// Run a single PR review end to end.
    Review {
        /// Path to the trusted host config JSON.
        #[arg(long)]
        config: PathBuf,
        /// Path to the per-job review request JSON.
        #[arg(long)]
        job: PathBuf,
        /// Where to write the report (stdout if omitted).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Output format: "json" (default) or "markdown".
        #[arg(long, default_value = "json")]
        format: String,
        /// Reuse an already-prepared bare clone (must contain base + head
        /// commits) instead of cloning. For fast local iteration only.
        #[arg(long)]
        repo_dir: Option<PathBuf>,
    },
    /// Print the fixed review system prompt (for audit).
    Prompt,
    /// Render a stored report without fetching source or calling a model.
    Render {
        #[arg(long)] report: PathBuf,
        #[arg(long)] out: PathBuf,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // Stable, non-sensitive error line for the dispatching API.
            eprintln!("error: {e}");
            // The API discards stderr, so the usage journal carries the stable
            // code (never the reason) for failures it can explain. Best effort.
            let _ = telemetry::record(serde_json::json!({"kind": "engine.failed", "code": e.code.as_str()}));
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> error::Result<()> {
    match cli.command {
        Command::Local {config,repo,base,out,format} => {
            let host: HostConfig = load_json(&config, "host config")?;
            let report = tokio::time::timeout(std::time::Duration::from_secs(host.timeout_seconds), local::run(&host,&repo,base.as_deref())).await
                .map_err(|_| error::err(error::Code::Internal,"local review timeout"))??;
            let rendered = if format == "json" {serde_json::to_string_pretty(&report).map_err(|_|error::err(error::Code::Internal,"report serialization failed"))?}
                else {report.presentation.as_ref().ok_or_else(||error::err(error::Code::Internal,"missing presentation"))?.markdown.clone()};
            if let Some(path)=out {std::fs::write(path,rendered).map_err(|_|error::err(error::Code::Internal,"could not write report"))?;} else {println!("{rendered}");}
            Ok(())
        },
        Command::Render { report, out } => {
            let mut report: domain::Report = load_json(&report, "saved report")?;
            render::attach(&mut report);
            std::fs::write(out, serde_json::to_vec(&report.presentation).map_err(|_| error::err(error::Code::Internal, "presentation encoding failed"))?)
                .map_err(|_| error::err(error::Code::Internal, "presentation write failed"))?;
            Ok(())
        }
        Command::Service { config, job, request, mode, out } => {
            let host: HostConfig = load_json(&config, "host config")?;
            let job: ReviewJob = load_json(&job, "job")?;
            let request: serde_json::Value = load_json(&request, "request")?;
            let result = tokio::time::timeout(std::time::Duration::from_secs(host.timeout_seconds), service::operation(&host, &job, &mode, request)).await
                .map_err(|_| error::err(error::Code::Internal, "operation timeout"))??;
            std::fs::write(out, serde_json::to_vec(&result).unwrap()).map_err(|_| error::err(error::Code::Internal, "output write failed"))?;
            Ok(())
        },
        Command::Prompt => {
            // Show the prompt with a placeholder nonce for auditing.
            println!("{}", prompts::system_prompt("<per-call-nonce>"));
            Ok(())
        }
        Command::Review {
            config,
            job,
            out,
            format,
            repo_dir,
        } => {
            let host: HostConfig = load_json(&config, "host config")?;
            let job: ReviewJob = load_json(&job, "review job")?;

            let report = tokio::time::timeout(std::time::Duration::from_secs(host.timeout_seconds), pipeline::run(&host, &job, repo_dir)).await
                .map_err(|_| error::err(error::Code::Internal, "review timeout"))??;

            let rendered = match format.as_str() {
                "markdown" | "md" => report.presentation.as_ref().ok_or_else(|| error::err(error::Code::Internal, "missing engine presentation"))?.markdown.clone(),
                "json" => serde_json::to_string_pretty(&report).map_err(|_| {
                    error::err(error::Code::Internal, "report serialization failed")
                })?,
                _ => return Err(error::err(error::Code::QueueInvalid, "unknown output format")),
            };
            match out {
                Some(path) => std::fs::write(&path, rendered)
                    .map_err(|_| error::err(error::Code::Internal, "could not write report"))?,
                None => println!("{rendered}"),
            }
            Ok(())
        }
    }
}

fn load_json<T: serde::de::DeserializeOwned>(path: &PathBuf, what: &str) -> error::Result<T> {
    let bytes = std::fs::read(path)
        .map_err(|_| error::err(error::Code::QueueInvalid, format!("could not read {what}")))?;
    serde_json::from_slice(&bytes)
        .map_err(|_| error::err(error::Code::QueueInvalid, format!("invalid {what} JSON")))
}
