//! Native setup and credential transport; no interpreter or project execution.
use crate::error::{err, Code, Result};
use clap::{Args, ValueEnum};
use serde_json::{json, Value};
use std::{
    ffi::OsString,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

const KEYS: [&str; 3] = ["OPENAI_API_KEY", "ANTHROPIC_API_KEY", "GEMINI_API_KEY"];

#[derive(Clone, Copy, ValueEnum)]
pub enum Provider {
    Bedrock,
    Openai,
    Anthropic,
    Google,
}
impl Provider {
    fn name(self) -> &'static str {
        match self {
            Self::Bedrock => "bedrock",
            Self::Openai => "openai",
            Self::Anthropic => "anthropic",
            Self::Google => "google",
        }
    }
    fn key(self) -> Option<&'static str> {
        match self {
            Self::Bedrock => None,
            Self::Openai => Some(KEYS[0]),
            Self::Anthropic => Some(KEYS[1]),
            Self::Google => Some(KEYS[2]),
        }
    }
}

#[derive(Args)]
pub struct ProviderOptions {
    #[arg(long, value_enum)]
    pub provider: Provider,
    /// Exact model/profile ID accessible in your provider account.
    #[arg(long)]
    pub model: String,
    #[arg(long, default_value_t = 8192, value_parser = clap::value_parser!(i32).range(1..=16384))]
    pub max_output_tokens: i32,
}
impl ProviderOptions {
    fn validate(&self) -> Result<()> {
        let direct = self.provider.key().is_some();
        if self.model.trim().is_empty()
            || self.model.len() > if direct { 100 } else { 200 }
            || self.model.chars().any(char::is_control)
            || (direct
                && !self
                    .model
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-._".contains(&b)))
            || !(1..=16384).contains(&self.max_output_tokens)
        {
            return Err(err(
                Code::QueueInvalid,
                "invalid model ID or output-token limit",
            ));
        }
        Ok(())
    }
    fn credential(&self) -> Result<Option<Vec<u8>>> {
        self.validate()?;
        let Some(name) = self.provider.key() else {
            return Ok(None);
        };
        let key = std::env::var(name).map_err(|_| {
            err(
                Code::QueueInvalid,
                format!("set {name} privately before running a review"),
            )
        })?;
        let mut bytes =
            serde_json::to_vec(&json!({"provider":self.provider.name(), "model":self.model,
            "api_key":key, "max_output_tokens":self.max_output_tokens}))
            .map_err(|_| err(Code::Internal, "credential encoding failed"))?;
        if crate::byok::Provider::decode(&bytes).is_err() {
            bytes.fill(0);
            return Err(err(Code::QueueInvalid, "invalid provider credentials"));
        }
        Ok(Some(bytes))
    }
}

#[derive(Args)]
pub struct SetupOptions {
    #[arg(long)]
    pub repo: PathBuf,
    #[command(flatten)]
    pub provider: ProviderOptions,
    #[arg(long, default_value = "eu-west-2")]
    pub region: String,
    #[arg(long)]
    pub base: Option<String>,
    /// Parent directory for a new private config/report directory.
    #[arg(long, default_value = ".local")]
    pub output_dir: PathBuf,
    /// Run a review; may incur provider charges. Setup alone never calls a model.
    #[arg(long)]
    pub run: bool,
}

fn private_parent(path: &Path) -> Result<PathBuf> {
    #[cfg(not(unix))]
    return Err(err(
        Code::QueueInvalid,
        "private setup directories require Unix permissions; use WSL",
    ));
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        match fs::symlink_metadata(path) {
            Ok(meta)
                if meta.is_dir()
                    && !meta.file_type().is_symlink()
                    && meta.permissions().mode() & 0o077 == 0 => {}
            Ok(_) => {
                return Err(err(
                    Code::QueueInvalid,
                    "output directory must be a private directory (chmod 700) and not a symlink",
                ))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                fs::DirBuilder::new()
                    .mode(0o700)
                    .create(path)
                    .map_err(|_| {
                        err(
                            Code::QueueInvalid,
                            "could not create private output directory",
                        )
                    })?;
            }
            Err(_) => {
                return Err(err(
                    Code::QueueInvalid,
                    "could not inspect output directory",
                ))
            }
        }
        fs::canonicalize(path)
            .map_err(|_| err(Code::QueueInvalid, "could not resolve output directory"))
    }
}

fn private_json(path: &Path, value: &Value) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|_| {
        err(
            Code::QueueInvalid,
            "output file already exists or cannot be created",
        )
    })?;
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|_| err(Code::Internal, "setup JSON encoding failed"))?;
    file.write_all(&bytes)
        .and_then(|_| file.write_all(b"\n"))
        .map_err(|_| err(Code::Internal, "setup file write failed"))
}

pub fn prepare(options: SetupOptions) -> Result<()> {
    options.provider.validate()?;
    if options.region.trim().is_empty()
        || options.region.len() > 100
        || options.region.chars().any(char::is_control)
    {
        return Err(err(Code::QueueInvalid, "invalid AWS region"));
    }
    let repo = fs::canonicalize(&options.repo).map_err(|_| {
        err(
            Code::QueueInvalid,
            "--repo must be an existing project directory",
        )
    })?;
    if !repo.is_dir() {
        return Err(err(
            Code::QueueInvalid,
            "--repo must be an existing project directory",
        ));
    }
    // Fail before creating files when an explicitly requested review lacks a key.
    let credential = if options.run {
        options.provider.credential()?
    } else {
        None
    };
    let parent = private_parent(&options.output_dir)?;
    let mut builder = tempfile::Builder::new();
    builder.prefix("agent-review-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(fs::Permissions::from_mode(0o700));
    }
    let directory = builder
        .tempdir_in(parent)
        .map_err(|_| err(Code::Internal, "could not create setup directory"))?;
    let config_path = directory.path().join("host.json");
    let report_path = directory.path().join("report.json");
    let mut config: Value = serde_json::from_str(include_str!("../examples/host.example.json"))
        .map_err(|_| err(Code::Internal, "invalid built-in config"))?;
    config["model_id"] = json!(options.provider.model);
    config["model_region"] = json!(options.region);
    config["model_max_output_tokens"] = json!(options.provider.max_output_tokens);
    config["retrieval"]["content_bytes"] = json!(0);
    config["retrieval"]["max_files"] = json!(0);
    private_json(&config_path, &config)?;
    if options.run {
        let mut args = vec![
            OsString::from("local"),
            "--config".into(),
            config_path.as_os_str().into(),
            "--repo".into(),
            repo.as_os_str().into(),
            "--format".into(),
            "json".into(),
            "--out".into(),
            report_path.as_os_str().into(),
        ];
        if let Some(base) = options.base {
            args.extend(["--base".into(), base.into()]);
        }
        let mut command = engine_command(&args)?;
        command.stdout(Stdio::inherit());
        run_child(command, credential)?;
    }
    // Keep successful setup artifacts, but clean up a failed preparation/review.
    let _ = directory.keep();
    println!(
        "{}",
        json!({"provider":options.provider.provider.name(), "model":options.provider.model,
        "repo":repo, "config":config_path, "report":report_path, "ran_review":options.run})
    );
    Ok(())
}

fn engine_command(args: &[OsString]) -> Result<Command> {
    let executable = std::env::current_exe()
        .map_err(|_| err(Code::Internal, "engine executable unavailable"))?;
    let mut command = Command::new(executable);
    command.args(args);
    Ok(command)
}

pub fn launch(provider: &ProviderOptions, args: &[OsString]) -> Result<()> {
    if matches!(provider.provider, Provider::Bedrock) {
        return Err(err(
            Code::QueueInvalid,
            "Bedrock uses the ordinary review commands and host config",
        ));
    }
    run_child(engine_command(args)?, provider.credential()?)
}

fn run_child(mut command: Command, mut credential: Option<Vec<u8>>) -> Result<()> {
    for key in KEYS {
        command.env_remove(key);
    }
    command.env_remove("INKSPECT_BYOK_STDIN");
    command.stdin(if credential.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    if credential.is_some() {
        command.env("INKSPECT_BYOK_STDIN", "1");
    }
    let result = (|| {
        let mut child = command
            .spawn()
            .map_err(|_| err(Code::Internal, "could not start review"))?;
        if let Some(bytes) = credential.as_mut() {
            let sent = child
                .stdin
                .take()
                .ok_or_else(|| err(Code::Internal, "credential pipe unavailable"))?
                .write_all(bytes);
            bytes.fill(0);
            if sent.is_err() {
                let _ = child.kill();
                let _ = child.wait();
                return Err(err(Code::Internal, "credential pipe unavailable"));
            }
        }
        let status = child
            .wait()
            .map_err(|_| err(Code::Internal, "could not wait for review"))?;
        if !status.success() {
            return Err(err(
                Code::Internal,
                "review process failed; see preceding engine diagnostic",
            ));
        }
        Ok(())
    })();
    if let Some(bytes) = credential.as_mut() {
        bytes.fill(0);
    }
    result
}

#[derive(Args)]
pub struct JobOptions {
    #[arg(long)]
    pub repository: String,
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    pub pull: u64,
    #[arg(long, default_value = "my-local-workspace")]
    pub tenant: String,
    #[arg(long, default_value = ".local/job.json")]
    pub out: PathBuf,
}

fn github(path: &str) -> Result<Value> {
    let mut command = Command::new("gh");
    command
        .args(["api", "--hostname", "github.com", path])
        .stdin(Stdio::null());
    for key in KEYS {
        command.env_remove(key);
    }
    command.env_remove("INKSPECT_BYOK_STDIN");
    let result = command
        .output()
        .map_err(|_| err(Code::GitHub, "install and authenticate GitHub CLI first"))?;
    if !result.status.success() {
        return Err(err(
            Code::GitHub,
            "GitHub CLI could not read the PR; check authentication and repository access",
        ));
    }
    serde_json::from_slice(&result.stdout)
        .map_err(|_| err(Code::GitHub, "invalid GitHub CLI response"))
}

fn sha(value: &Value) -> Result<&str> {
    value
        .as_str()
        .filter(|s| s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| err(Code::GitHub, "invalid GitHub commit ID"))
}

pub fn prepare_job(options: JobOptions) -> Result<()> {
    let (owner, repo) = options
        .repository
        .split_once('/')
        .filter(|(o, r)| {
            !o.is_empty()
                && !r.is_empty()
                && [*o, *r].iter().all(|s| {
                    s.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-._".contains(&b))
                })
        })
        .ok_or_else(|| err(Code::QueueInvalid, "--repository must be OWNER/REPO"))?;
    let repository = github(&format!("repos/{owner}/{repo}"))?;
    let pull = github(&format!("repos/{owner}/{repo}/pulls/{}", options.pull))?;
    let base = sha(&pull["base"]["sha"])?;
    let head = sha(&pull["head"]["sha"])?;
    let comparison = github(&format!(
        "repos/{owner}/{repo}/compare/{base}...{head}?per_page=1"
    ))?;
    let job = job_value(&options, &repository, &pull, &comparison)?;
    let parent = options
        .out
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let parent = private_parent(parent)?;
    let name = options
        .out
        .file_name()
        .ok_or_else(|| err(Code::QueueInvalid, "invalid job output path"))?;
    private_json(&parent.join(name), &job)?;
    println!("Created pinned review job. Publishing and automatic review are disabled.");
    Ok(())
}

fn job_value(
    options: &JobOptions,
    repository: &Value,
    pull: &Value,
    comparison: &Value,
) -> Result<Value> {
    let (owner, repo) = options
        .repository
        .split_once('/')
        .ok_or_else(|| err(Code::QueueInvalid, "invalid repository"))?;
    let repository_id = repository["id"]
        .as_u64()
        .filter(|id| *id > 0)
        .ok_or_else(|| err(Code::GitHub, "invalid repository ID"))?;
    let head_ref = pull["head"]["ref"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| err(Code::GitHub, "missing head reference"))?;
    let job = json!({"run_id":format!("J-{:032x}",rand::random::<u128>()), "owner":owner, "repo":repo,
        "repository_id":repository_id.to_string(), "tenant_id":options.tenant, "pull_number":options.pull,
        "base_sha":sha(&comparison["merge_base_commit"]["sha"])?, "head_sha":sha(&pull["head"]["sha"])?,
        "head_ref":head_ref, "review_notes":[], "publish_inline_comments":false, "auto_review":false});
    let parsed: crate::config::ReviewJob =
        serde_json::from_value(job.clone()).map_err(|_| err(Code::GitHub, "invalid review job"))?;
    parsed.validate()?;
    Ok(job)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn child_receives_only_pipe_credentials_and_preserves_aws_identity() {
        for direct in [false, true] {
            let mut command = Command::new("/bin/sh");
            command.args(["-c", if direct {
                "test -z \"${OPENAI_API_KEY+x}${ANTHROPIC_API_KEY+x}${GEMINI_API_KEY+x}\" && test \"$INKSPECT_BYOK_STDIN\" = 1 && test \"$AWS_PROFILE\" = fixture && read -r line && test \"$line\" = pipe-only"
            } else {
                "test -z \"${OPENAI_API_KEY+x}${ANTHROPIC_API_KEY+x}${GEMINI_API_KEY+x}${INKSPECT_BYOK_STDIN+x}\" && test \"$AWS_PROFILE\" = fixture && ! read -r line"
            }]);
            for key in KEYS {
                command.env(key, "must-not-reach-child");
            }
            command
                .env("INKSPECT_BYOK_STDIN", "inherited")
                .env("AWS_PROFILE", "fixture");
            run_child(command, direct.then(|| b"pipe-only\n".to_vec())).unwrap();
        }
    }

    #[test]
    fn failed_child_is_not_retried() {
        let dir = tempfile::tempdir().unwrap();
        let count = dir.path().join("attempts");
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "printf x >> \"$1\"; exit 7", "test"])
            .arg(&count);
        assert!(run_child(command, None).is_err());
        assert_eq!(fs::read(count).unwrap(), b"x");
    }

    #[test]
    fn job_uses_merge_base_and_never_enables_publication() {
        let options = JobOptions {
            repository: "owner/project".into(),
            pull: 12,
            tenant: "workspace".into(),
            out: "job.json".into(),
        };
        let pull = json!({"head":{"sha":"b".repeat(40),"ref":"branch/quoted-\"name"},"base":{"sha":"c".repeat(40)}});
        let comparison = json!({"merge_base_commit":{"sha":"a".repeat(40)}});
        let job = job_value(&options, &json!({"id":123}), &pull, &comparison).unwrap();
        assert_eq!(job["base_sha"], "a".repeat(40));
        assert_eq!(job["head_sha"], "b".repeat(40));
        assert_eq!(job["repository_id"], "123");
        assert_eq!(job["publish_inline_comments"], false);
        assert_eq!(job["auto_review"], false);
        assert!(job_value(
            &options,
            &json!({"id":123}),
            &pull,
            &json!({"merge_base_commit":{"sha":"bad"}})
        )
        .is_err());
    }

    #[test]
    fn private_json_never_overwrites_a_file_or_follows_a_link() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        private_json(&path, &json!({"keep":true})).unwrap();
        assert!(private_json(&path, &json!({"keep":false})).is_err());
        let link = dir.path().join("link.json");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(private_json(&link, &json!({"keep":false})).is_err());
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(path).unwrap()).unwrap()["keep"],
            true
        );
    }
}
