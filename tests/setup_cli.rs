#![cfg(unix)]
use serde_json::Value;
use std::{
    fs,
    os::unix::fs::{symlink, PermissionsExt},
    path::Path,
    process::{Command, Output},
};

const KEYS: [&str; 3] = ["OPENAI_API_KEY", "ANTHROPIC_API_KEY", "GEMINI_API_KEY"];
fn engine() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_inkspect-engine"));
    for key in KEYS {
        command.env_remove(key);
    }
    command.env_remove("INKSPECT_BYOK_STDIN");
    command
}
fn setup(root: &Path, provider: &str) -> Command {
    let mut command = engine();
    command
        .args([
            "setup",
            "--provider",
            provider,
            "--model",
            "test-model",
            "--repo",
        ])
        .arg(root.join("project"))
        .arg("--output-dir")
        .arg(root.join("output"));
    command
}
fn success(output: Output) -> Value {
    assert!(
        output.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}
fn fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("project")).unwrap();
    fs::write(
        root.path().join("project/.inkspectignore"),
        "keep-existing/\n",
    )
    .unwrap();
    root
}

#[test]
fn setup_without_a_runtime_or_keys_is_private_and_preserves_the_project() {
    let root = fixture();
    for provider in ["bedrock", "openai", "anthropic", "google"] {
        let state = success(
            setup(root.path(), provider)
                .env("PATH", "")
                .output()
                .unwrap(),
        );
        assert_eq!(state["ran_review"], false);
        let config = Path::new(state["config"].as_str().unwrap());
        let value: Value = serde_json::from_slice(&fs::read(config).unwrap()).unwrap();
        assert_eq!(value["model_id"], "test-model");
        assert_eq!(value["retrieval"]["content_bytes"], 0);
        assert_eq!(value["retrieval"]["max_files"], 0);
        assert_eq!(
            fs::metadata(config).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(config.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert!(!Path::new(state["report"].as_str().unwrap()).exists());
    }
    assert_eq!(
        fs::read_to_string(root.path().join("project/.inkspectignore")).unwrap(),
        "keep-existing/\n"
    );
    assert_eq!(
        fs::read_dir(root.path().join("project")).unwrap().count(),
        1
    );
    assert_eq!(fs::read_dir(root.path().join("output")).unwrap().count(), 4);
}

#[test]
fn missing_or_invalid_credentials_fail_before_creating_files() {
    for (provider, key) in [
        ("openai", KEYS[0]),
        ("anthropic", KEYS[1]),
        ("google", KEYS[2]),
    ] {
        let root = fixture();
        assert!(!setup(root.path(), provider)
            .arg("--run")
            .output()
            .unwrap()
            .status
            .success());
        let secret = "invalid\ncredential";
        let output = setup(root.path(), provider)
            .arg("--run")
            .env(key, secret)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stderr).contains(secret));
        assert!(!root.path().join("output").exists());
    }
}

#[test]
fn invalid_model_and_limits_fail_without_artifacts() {
    let root = fixture();
    for args in [
        ["--model", "bad/model"],
        ["--max-output-tokens", "0"],
        ["--max-output-tokens", "16385"],
    ] {
        let mut command = engine();
        command
            .args(["setup", "--provider", "openai", "--repo"])
            .arg(root.path().join("project"))
            .arg("--output-dir")
            .arg(root.path().join("output"));
        if args[0] != "--model" {
            command.args(["--model", "test-model"]);
        }
        assert!(!command.args(args).output().unwrap().status.success());
        assert!(!root.path().join("output").exists());
    }
}

#[test]
fn private_parent_rejects_symlinks_and_public_directories() {
    let root = fixture();
    let output = root.path().join("output");
    fs::create_dir(&output).unwrap();
    fs::set_permissions(&output, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(!setup(root.path(), "bedrock")
        .output()
        .unwrap()
        .status
        .success());
    assert_eq!(fs::read_dir(&output).unwrap().count(), 0);
    fs::remove_dir(&output).unwrap();
    symlink(root.path().join("project"), &output).unwrap();
    assert!(!setup(root.path(), "bedrock")
        .output()
        .unwrap()
        .status
        .success());
}

#[test]
fn explicit_empty_reviews_need_no_network_and_do_not_save_keys() {
    for (provider, key) in [
        ("bedrock", None),
        ("openai", Some(KEYS[0])),
        ("anthropic", Some(KEYS[1])),
        ("google", Some(KEYS[2])),
    ] {
        let root = fixture();
        let secret = format!("fixture-{:032x}", rand::random::<u128>());
        let mut command = setup(root.path(), provider);
        command
            .arg("--run")
            .env("INKSPECT_BYOK_STDIN", "1")
            .env("PATH", "");
        for variable in KEYS {
            command.env(variable, &secret);
        }
        if let Some(variable) = key {
            command.env(variable, &secret);
        }
        let output = command.output().unwrap();
        assert!(!String::from_utf8_lossy(&output.stdout).contains(&secret));
        assert!(!String::from_utf8_lossy(&output.stderr).contains(&secret));
        let state = success(output);
        assert_eq!(state["ran_review"], true);
        for field in ["config", "report"] {
            let data = fs::read_to_string(state[field].as_str().unwrap()).unwrap();
            assert!(!data.contains(&secret));
        }
    }
}

#[test]
fn launcher_rejects_non_review_commands_and_propagates_failure() {
    for args in [
        vec!["prompt"],
        vec!["setup", "--repo", "."],
        vec!["local", "--config", "missing"],
    ] {
        let output = engine()
            .args([
                "with-provider",
                "--provider",
                "openai",
                "--model",
                "test-model",
                "--",
            ])
            .args(args)
            .env(KEYS[0], format!("fixture-{:032x}", rand::random::<u128>()))
            .output()
            .unwrap();
        assert!(!output.status.success());
    }
}

fn script(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn shell_wrapper_builds_without_keys_and_runs_native_setup() {
    let root = fixture();
    let checkout = root.path().join("engine with spaces");
    let tools = root.path().join("tools");
    fs::create_dir_all(checkout.join("scripts")).unwrap();
    fs::create_dir_all(checkout.join("target/debug")).unwrap();
    fs::create_dir(&tools).unwrap();
    fs::write(
        checkout.join("scripts/ai-setup.sh"),
        include_str!("../scripts/ai-setup.sh"),
    )
    .unwrap();
    fs::copy(
        env!("CARGO_BIN_EXE_inkspect-engine"),
        checkout.join("target/debug/inkspect-engine"),
    )
    .unwrap();
    symlink("/usr/bin/dirname", tools.join("dirname")).unwrap();
    for tool in ["git", "rustc"] {
        script(&tools.join(tool), "#!/bin/sh\nexit 0\n");
    }
    script(&tools.join("cargo"), "#!/bin/sh\nset -eu\ntest -z \"${OPENAI_API_KEY+x}${ANTHROPIC_API_KEY+x}${GEMINI_API_KEY+x}${INKSPECT_BYOK_STDIN+x}\"\ntest \"$1 $2 $3 $4 $5\" = 'build --locked --bin inkspect-engine --target-dir'\ntest \"$6\" = \"$PWD/target\"\n");
    let mut command = Command::new("/bin/sh");
    command
        .arg(checkout.join("scripts/ai-setup.sh"))
        .args(["--provider", "openai", "--model", "test-model", "--repo"])
        .arg(root.path().join("project"))
        .env("PATH", &tools)
        .env("INKSPECT_BYOK_STDIN", "1");
    for key in KEYS {
        command.env(key, format!("fixture-{:032x}", rand::random::<u128>()));
    }
    let state = success(command.output().unwrap());
    assert_eq!(state["ran_review"], false);
    assert!(Path::new(state["config"].as_str().unwrap())
        .starts_with(fs::canonicalize(checkout.join(".local")).unwrap()));
    script(&tools.join("cargo"), "#!/bin/sh\nexit 7\n");
    assert!(!command.output().unwrap().status.success());
    assert_eq!(fs::read_dir(checkout.join(".local")).unwrap().count(), 1);
}

#[test]
fn prepare_job_uses_authenticated_cli_without_runtime_dependencies_or_overwrite() {
    let root = fixture();
    let tools = root.path().join("tools");
    fs::create_dir(&tools).unwrap();
    script(
        &tools.join("gh"),
        &format!(
            r#"#!/bin/sh
set -eu
test -z "${{OPENAI_API_KEY+x}}${{ANTHROPIC_API_KEY+x}}${{GEMINI_API_KEY+x}}${{INKSPECT_BYOK_STDIN+x}}"
test "$1 $2 $3" = 'api --hostname github.com'
case "$4" in
  repos/owner/project) printf '%s\n' '{{"id":123}}' ;;
  repos/owner/project/pulls/12) printf '%s\n' '{{"base":{{"sha":"{base}"}},"head":{{"sha":"{head}","ref":"branch"}}}}' ;;
  'repos/owner/project/compare/{base}...{head}?per_page=1') printf '%s\n' '{{"merge_base_commit":{{"sha":"{merge}"}}}}' ;;
  *) exit 9 ;;
esac
"#,
            base = "a".repeat(40),
            head = "b".repeat(40),
            merge = "c".repeat(40)
        ),
    );
    let file = root.path().join("output/job.json");
    let mut command = engine();
    command
        .args([
            "prepare-job",
            "--repository",
            "owner/project",
            "--pull",
            "12",
            "--out",
        ])
        .arg(&file)
        .env("PATH", &tools)
        .env("INKSPECT_BYOK_STDIN", "1");
    for key in KEYS {
        command.env(key, format!("fixture-{:032x}", rand::random::<u128>()));
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let original = fs::read(&file).unwrap();
    let job: Value = serde_json::from_slice(&original).unwrap();
    assert_eq!(job["base_sha"], "c".repeat(40));
    assert_eq!(job["publish_inline_comments"], false);
    assert_eq!(
        fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(!command.output().unwrap().status.success());
    assert_eq!(fs::read(&file).unwrap(), original);
}

#[test]
fn native_launcher_runs_a_local_review_without_python_or_cargo() {
    let root = fixture();
    let config = root.path().join("host.json");
    fs::write(&config, include_str!("../examples/host.example.json")).unwrap();
    let report = root.path().join("report.json");
    let secret = format!("fixture-{:032x}", rand::random::<u128>());
    let output = engine()
        .args([
            "with-provider",
            "--provider",
            "openai",
            "--model",
            "test-model",
            "--",
            "local",
            "--config",
        ])
        .arg(config)
        .arg("--repo")
        .arg(root.path().join("project"))
        .args(["--format", "json", "--out"])
        .arg(&report)
        .env("PATH", "")
        .env(KEYS[0], &secret)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!fs::read_to_string(report).unwrap().contains(&secret));
}
