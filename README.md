<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/inkspect-symbol-white.svg">
    <img src="docs/assets/inkspect-symbol-black.svg" alt="Inkspect" width="64" height="80">
  </picture>
</p>

<h1 align="center">Inkspect Code Review Engine</h1>

<p align="center">Review the change. Understand the context.</p>

Run AI code reviews from your own computer, CI runner, or server. Bring your own
model credentials, review a local project or GitHub pull request, and save the
findings as Markdown or JSON. Posting comments to GitHub is optional.

You do not need an [Inkspect account](https://inkspect.si), dashboard, or application database to use
this CLI. Your selected AI provider bills your own account. Selected source is
sent to that provider; running the engine yourself does not make inference offline.

The repository is `inkspect-code-review-engine`; the executable is **`inkspect-engine`**.

[MIT license](LICENSE) · [Contributing](CONTRIBUTING.md) · [Security](SECURITY.md) ·
[Support](SUPPORT.md) · [Changelog](CHANGELOG.md) · [Community guidelines](CODE_OF_CONDUCT.md)

## Start here

1. [Install the engine](#1-install-the-engine).
2. [Create your configuration](#3-create-your-configuration).
3. [Review a local project](#review-a-local-project-no-github-required), or
   [connect GitHub](#2-connect-github) and [choose a PR](#4-choose-a-pull-request).
4. [Choose your AI provider](#5-choose-your-ai-provider-and-run).

Using a coding agent? Start with [AI-assisted setup](#ai-assisted-setup).
Prefer containers? See [Docker Compose](#docker-compose).
For unattended use, see [Self-hosting and CI](#self-hosting-and-ci).

## What you get

- Findings linked to changed lines, with suggested fixes and verification steps.
- A review summary and an assessment of how the change matches the PR's intent.
- Code quality and security scores, with explanations.
- A behavior diagram when it helps explain the change.
- Copyable fix prompts for your coding assistant.
- Optional related-code context from a self-hosted Qdrant index.

The engine checks finding locations against the actual diff. It does not execute
your project's code or tests. Treat findings and suggested fixes as review input;
continue to use your test suite and human judgment.

## AI-assisted setup

Give your coding agent [this setup guide](docs/AI_SETUP.md), or ask:

> Read the Inkspect AI setup guide at
> https://github.com/inkspect-code-review/inkspect-code-review-engine/blob/main/docs/AI_SETUP.md
> and prepare a local review for my project. Use my chosen provider and local
> credentials without displaying or saving keys. Ask for missing information,
> then run the review and summarize findings. Do not modify or publish my code.

The guide helps the agent check prerequisites, build the CLI, select a provider,
prepare exclusions/config and run the review. The shell helper builds the Rust
CLI and invokes its native `setup` command; no Python or other scripting runtime
is required. Rust/Cargo enforces the minimum version during the build:

```sh
sh scripts/ai-setup.sh \
  --repo /absolute/path/to/project --provider openai --model YOUR_MODEL_ID
```

Without `--run`, it only prepares the environment; no model call is made. Add
`--run` when you want a review and have set `OPENAI_API_KEY` privately. Anthropic
uses `ANTHROPIC_API_KEY`, Google uses `GEMINI_API_KEY`, and Bedrock uses your AWS
credentials. Use `--max-output-tokens` if your model needs a lower output cap.
See the guide for the full workflow and a setup-only prompt. With an already
built binary, use `inkspect-engine setup` with the same options; `--output-dir`
selects the private parent directory (default `.local` in the current directory).
The shell helper uses `.local` in the engine checkout. Existing parent directories
must have mode 0700; setup never changes an existing directory's permissions.

## 1. Install the engine

These examples use a macOS or Linux terminal. Windows users can follow them in
WSL with a Linux toolchain.

Install these tools first:

| Tool | Why you need it |
| --- | --- |
| Rust **1.94.1 or newer**, including Cargo | Builds the engine. |
| Git | Downloads repository contents for review. |
| GitHub CLI (`gh`), optional | Only needed for the GitHub PR examples. Local reviews do not need it. |
| A native C/C++ build toolchain and CMake | Builds native dependencies if required by your platform. |

Check your installations:

```sh
rustc --version
cargo --version
git --version
gh --version
```

Download the repository and build the executable:

```sh
git clone https://github.com/inkspect-code-review/inkspect-code-review-engine.git
cd inkspect-code-review-engine
cargo build --release --locked
./target/release/inkspect-engine --help
```

The first build downloads and compiles dependencies, so it can take a while.
Run the remaining examples from this repository's root directory.

## 2. Connect GitHub

Skip this section for local reviews.

Sign in with an account that can access the repository you want to review:

```sh
gh auth login --hostname github.com
export INKSPECT_GITHUB_READ_TOKEN="$(gh auth token --hostname github.com)"
```

The engine uses this token to read PR details and fetch source files. If you use
a fine-grained token instead, grant access to the selected repository with
**Contents: read** and **Pull requests: read** permissions. Supply it through your
shell or secret manager as `INKSPECT_GITHUB_READ_TOKEN`.

For your first run, leave `publish_inline_comments` set to `false`. This saves a
local report without posting a review to GitHub.

## 3. Create your configuration

```sh
mkdir -p .local
chmod 700 .local
cp examples/host.example.json .local/host.json
```

Open `.local/host.json` in your editor. It controls the model and resource limits;
it is **not** where you put API keys.

| Setting | What to put here |
| --- | --- |
| `model_region` | The AWS region for Bedrock reviews. Ignored for direct-provider reviews below. |
| `model_id` | A Bedrock model or inference profile ID your AWS account can invoke. Ignored for direct-provider reviews. |
| `model_max_output_tokens` | Maximum response length. The example starts at `8192`; your model must support it. |
| `timeout_seconds` | Maximum duration of the whole review. The example allows 30 minutes. |
| `retrieval.content_bytes` | `0` in the example: start without a vector database or embedding calls. |
| `retrieval.max_files` | Also `0` in the example. Enable both retrieval limits when adding repository context. |
| `sandbox.clone_mib` | Maximum temporary clone size, in MiB. The example uses `500`. |
| `sandbox.containerize` | Keep `false`. The built-in container clone mode is not implemented in this release. |

The other retrieval fields can stay as supplied while retrieval is disabled.
The `sandbox.runtime`, `image`, CPU, memory and PID fields do not enforce container
limits in host mode. Use your own runner/container limits for server deployments.

## Review a local project (no GitHub required)

After copying the host config, choose your provider as described below. For
Bedrock, use:

```sh
./target/release/inkspect-engine local \
  --config .local/host.json \
  --repo /absolute/path/to/your-project \
  --out .local/report.md
```

For OpenAI, Anthropic or Google, use the native `with-provider` command in
Option B. You do not need a GitHub token, PR, job JSON, Qdrant
or PostgreSQL for local reviews.

| Folder | What gets reviewed |
| --- | --- |
| Git working-tree root with commits | Current on-disk changes against `HEAD`, including staged changes as they exist on disk, unstaged changes, untracked files and deletions. |
| Git working tree with no commits | Eligible tracked/untracked source files as a first snapshot. |
| Ordinary folder without `.git` | Eligible source files, treated as an initial snapshot. |

To include committed changes on a branch, specify a local comparison point:

```sh
./target/release/inkspect-engine local \
  --config .local/host.json --repo /absolute/path/to/your-project \
  --base main --out .local/report.md
```

`--base` compares the **current working-tree contents directly against that
local commit or branch**; it does not fetch a remote or compute a merge base.
If you want merge-base semantics, resolve that commit yourself and pass its SHA.
Use the Git working-tree root for Git-aware mode. A nested directory without
its own `.git` is treated as an ordinary folder.

Local mode never stages, commits, checks out, fetches, or publishes files. It
reads a snapshot; later edits are not part of the saved report. A clean working
tree produces a report without a model call. It does not review intermediate
staged content that you have subsequently overwritten in the working tree.

Local `.inkspectignore` rules come from the current folder. Git mode also omits
Git-ignored untracked files. Plain-folder mode uses `.inkspectignore` and built-in
source filters, **not `.gitignore`**. Hidden paths, symlinks, common generated and
sensitive paths are skipped. Binary, non-UTF-8, oversized selected files and
obvious secret markers stop the review before a model call. These checks are not
a complete secret scanner.

Each review is limited to 200 changed source files, 128 KB per source file and
512 KB of combined before/after source. Narrow the folder or add exclusions when
a limit is reached. Local mode currently uses the diff/snapshot only; Qdrant
retrieval is available for GitHub PR reviews, not local snapshots.

## 4. Choose a pull request

Replace `YOUR_ORG/YOUR_REPO` and `123` with your repository and PR number:

```sh
export REVIEW_REPOSITORY="YOUR_ORG/YOUR_REPO"
export REVIEW_PR="123"
```

Run this block to create `.local/job.json`. It asks GitHub for the repository ID
and current commit IDs, so you do not need to copy them by hand:

```sh
./target/release/inkspect-engine prepare-job \
  --repository "$REVIEW_REPOSITORY" --pull "$REVIEW_PR" \
  --out .local/job.json
```

This uses your authenticated `gh` CLI against github.com. The output is private
(mode 0600); an existing job is never overwritten. To refresh a job, choose a
new `--out` path or remove the old job first. The parent directory must be private
(mode 0700). `--tenant` defaults to `my-local-workspace`.

A *merge base* is the common commit from which the two branches diverged. The
engine reviews changes between that commit and the PR's current head.

`tenant_id` is your own workspace label, used to separate indexed context. It
does not need to come from an Inkspect account. Keep it stable for the same
workspace, and use separate labels for unrelated tenants.

To add review focus, edit `review_notes`, for example:

```json
[{ "author": "reviewer", "body": "Focus on error handling and missing tests." }]
```

Generate a fresh job after new commits are pushed. The engine checks the PR again
before reviewing and stops if the pinned commits no longer match.

## 5. Choose your AI provider and run

| Provider | Available now? | Credentials |
| --- | --- | --- |
| AWS Bedrock | Yes | Your AWS credentials or Bedrock API key. |
| OpenAI | Yes | Your OpenAI API key and an accessible model ID. |
| Anthropic | Yes | Your Anthropic API key and an accessible model ID. |
| Google Gemini | Yes | Your Google API key and an accessible model ID. |
| Inkspect AI | Planned | An Inkspect API key using your workspace's credits. Not available in this CLI yet. |

### Option A: AWS Bedrock

In `.local/host.json`, replace `YOUR_BEDROCK_MODEL_ID` with a model or inference
profile ID available in your account, and set its `model_region`.

The engine uses the standard AWS credential chain. For example, if you already
have an AWS profile configured:

```sh
export AWS_PROFILE="your-profile-name"
```

An IAM role on your runner/server also works. Alternatively, supply a Bedrock API
key as `AWS_BEARER_TOKEN_BEDROCK` through your secret manager. Your AWS identity
needs permission to invoke the chosen model; a model ID from another account or
region is not automatically usable.

Then run:

```sh
./target/release/inkspect-engine review \
  --config .local/host.json \
  --job .local/job.json \
  --format markdown \
  --out .local/report.md
```

### Option B: OpenAI, Anthropic, or Google

The native `with-provider` launcher reads the selected key from your environment,
then starts a review subprocess with credential JSON on a private stdin pipe.
It removes all three direct-provider key variables from the child environment;
keys never enter config files or command-line arguments. Existing integrations
can continue to provide credential JSON on stdin with `INKSPECT_BYOK_STDIN=1`.

Set `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, or `GEMINI_API_KEY` privately through
your secret manager or terminal. Never paste a key into chat or a shell command
that will be saved in history. For an interactive Bash prompt, for example:

```bash
read -r -s -p 'OpenAI API key: ' OPENAI_API_KEY
printf '\n'
export OPENAI_API_KEY
```

Keep retrieval disabled for this first run; a direct-provider review without
retrieval does not need AWS credentials. Run a local review:

```sh
./target/release/inkspect-engine with-provider \
  --provider openai --model YOUR_MODEL_ID -- \
  local --config .local/host.json --repo /absolute/path/to/your-project \
  --format markdown --out .local/report.md
```

For a GitHub PR, replace `local --repo /absolute/path/to/your-project` with
`review --job .local/job.json`, keeping the config and output options. The launcher
also accepts `snapshot`. It rejects other subcommands and never retries a failed
review. Use `--provider anthropic` or `--provider google` for the other keys.
Bedrock uses the ordinary review commands and the model/region in the host config.

Use a model ID your own provider account can access. If its output limit is
below `8192`, add `--max-output-tokens` before `--` and lower
`model_max_output_tokens` in the host config. The smaller limit is used.
There is no automatic provider or model fallback. Unset the key variable after
use if it is no longer needed. The parent launcher's environment is still sensitive;
do not dump it to logs.

The engine uses OpenAI Responses, Anthropic Messages, or Google GenerateContent.
API keys are passed only to the selected provider's fixed HTTPS endpoint.

### Read the result

Open `.local/report.md` in a Markdown viewer. For structured output, change
`--format markdown --out .local/report.md` to
`--format json --out .local/report.json` in your chosen command or launcher.
Each review invocation makes a new model request and may incur a new charge.

You can turn an existing JSON report into presentation data **without another
model call**:

```sh
./target/release/inkspect-engine render \
  --report .local/report.json --out .local/presentation.json
```

The result includes a `markdown` field and the generated fix prompts.

## Post the review to GitHub

Once you are happy with local reports:

1. Give your GitHub token **Pull requests: read and write** permission.
2. Supply it as `INKSPECT_GITHUB_WRITE_TOKEN`.
3. Set `"publish_inline_comments": true` in `.local/job.json`.
4. Run the review command again with your chosen provider.

If the token from `gh auth login` already has those permissions:

```sh
export INKSPECT_GITHUB_WRITE_TOKEN="$INKSPECT_GITHUB_READ_TOKEN"
```

Publishing creates a review summary and inline comments on eligible changed
lines. It does not approve, merge, or commit code. A publishing failure can leave
a successful local report; check the terminal warning and PR before rerunning,
because another review run can incur another model charge.

For a shared installation, you can use your own GitHub App instead of user tokens:

```sh
export INKSPECT_GITHUB_APP_ID="YOUR_NUMERIC_APP_ID"
export INKSPECT_GITHUB_APP_PRIVATE_KEY_FILE="/absolute/path/to/github-app.pem"
```

Install that App on the repository and grant Contents read access, plus Pull
requests read access (write access when publishing). App credentials take
precedence over explicit read/write tokens for the `review` command.

## Exclude files with `.inkspectignore`

Start with the [example `.inkspectignore`](.inkspectignore) included in this repo.
Copy or merge it into the root of the project **being reviewed**, keeping any
existing project-specific rules. For example:

```gitignore
# Skip generated files and build output.
dist/
coverage/
**/*.generated.ts

# Keep this file in scope even if an earlier pattern excluded it.
!src/important.generated.ts
```

For GitHub reviews, the engine reads exclusions from the comparison base, not the PR's new version,
so a PR cannot hide its own changes. Add the policy to your base branch before
relying on it. Nested `.inkspectignore` files are ignored.

Supported patterns include `*`, `?`, `**`, directory patterns and `!` exceptions.
Invalid policies stop the review before the model call. If all changes are
excluded, the model call is skipped.

## Optional: add repository context

Start with the diff-only setup above. For related-code retrieval, you also need:

- A Qdrant instance you operate.
- AWS access to the configured Cohere Embed and Rerank models.
- Nonzero `retrieval.content_bytes` and `retrieval.max_files` in your host config.

For example, use `content_bytes: 192000` and `max_files: 40`, and configure the
embedding/reranking model IDs and regions available in your AWS account. Then set:

```sh
export INKSPECT_QDRANT_URL="http://127.0.0.1:6333"
# If your Qdrant instance requires authentication, supply
# INKSPECT_QDRANT_API_KEY through your secret manager.
```

Set `"auto_review": true` in the job to build an index when the required snapshot
has no index. **For this CLI, that flag enables initial indexing; it does not
watch GitHub or schedule future reviews.** Embedding and reranking have their own
provider costs, even when the review model uses a direct OpenAI or Anthropic key.

Retrieved chunks are checked against the pinned repository revision before they
are used. If retrieval is unavailable, the review continues without that context
and prints a warning. Keep the embedding model and dimensions consistent with
your index; changing them requires a compatible new index.

## Docker Compose

Compose includes the CLI and **optional Qdrant**. There is no PostgreSQL service:
the standalone engine does not use PostgreSQL.

```sh
mkdir -p .local/reports
cp examples/host.example.json .local/host.json
export INKSPECT_CONFIG=./.local/host.json
export INKSPECT_REPO_PATH=/absolute/path/to/your-project
export INKSPECT_UID="$(id -u)"
export INKSPECT_GID="$(id -g)"
docker compose --profile review build engine
```

Edit `.local/host.json` with your Bedrock region/model first. Supply AWS credentials
through your shell or secret manager, then run:

```sh
docker compose run --rm engine local \
  --config /config/host.json --repo /workspace --out /reports/report.md
```

Your project is mounted read-only. Reports are written to `.local/reports` on your
host. The image runs as a non-root user; UID/GID above allow it to write to that
directory on Linux. The process has a read-only root filesystem and bounded
writable temporary storage. AWS profiles from your host are not automatically
mounted: pass session credentials or a Bedrock API key to the container.

For direct providers, use the same native launcher inside the container. Supply
`OPENAI_API_KEY` privately in your host environment first; `-e OPENAI_API_KEY`
forwards its value without placing it in the command arguments:

```sh
docker compose run --rm -T -e OPENAI_API_KEY engine with-provider \
  --provider openai --model YOUR_MODEL_ID -- \
  local --config /config/host.json --repo /workspace --out /reports/report.md
```

For Anthropic or Google, change both the environment variable and `--provider`.
The launcher removes direct-provider keys from the review subprocess environment
and supplies the selected key on stdin. The container configuration still contains
the forwarded environment value and must be accessible only to trusted operators.
No host Python, Node.js, jq or Rust installation is needed for the Docker workflow.

To add Qdrant for **GitHub PR context**, start the context profile:

```sh
docker compose --profile context up -d qdrant
```

The database persists in the `qdrant-data` volume; its HTTP port is bound to
`127.0.0.1:6333`. Set `QDRANT_API_KEY` before starting services if you want
authentication; the engine and Qdrant receive the same key. Do not expose an
unauthenticated Qdrant instance to a public network.

Prepare `.local/job.json` using the GitHub steps above, set
`INKSPECT_JOB=./.local/job.json`, enable retrieval in the config, and set the job's
`auto_review` to `true`. Then run the container's `review` command with
`--job /config/job.json` instead of `local --repo /workspace`. Supply a GitHub read
token and AWS embedding/reranking credentials as well as your review provider.

```sh
docker compose --profile context down
```

Stopping services preserves Qdrant data. Only add `--volumes` if you deliberately
want to delete that stored index. See [.env.example](.env.example) for path and
port settings; real keys should stay out of version control.

## Self-hosting and CI

The CLI is a one-shot process: give it a config and a local folder or PR job, run it, and collect
the report. It does not start an HTTP server, a webhook listener, or a dashboard.

For an unattended runner:

1. Build the binary and install Git in the runtime environment.
2. Supply GitHub and provider credentials from the runner's secret store. For
   direct providers, use `with-provider`, or pass credential JSON on stdin with
   `INKSPECT_BYOK_STDIN=1` from an existing integration.
3. For GitHub reviews, generate a fresh job from the PR event or current metadata.
   For local reviews, mount the project read-only and pass its path with `--repo`.
4. Keep the host config under operator control, outside the reviewed PR's control.
5. Run one review process per job and retain the report as a private CI artifact.
6. Configure your own concurrency, timeouts, disk/memory limits and artifact retention.

Host-mode cloning uses a temporary bare repository and disables hooks, submodules
and Git LFS smudging. It does not execute project scripts. For stronger process
isolation, run the entire CLI inside your own container or isolated runner; the
`sandbox.containerize` switch itself is not available yet.

The standalone CLI does not include Inkspect's hosted billing, automatic-fix
scheduler, or commit publisher. Advanced `service` commands are building blocks
for integrations, not a self-hosted application server. In particular,
`service --mode fix` produces proposed file changes; it does not push them.

### Optional diagnostics

```sh
export INKSPECT_USAGE_FILE="$PWD/.local/usage.jsonl"
export INKSPECT_REVIEW_CONTEXT_FILE="$PWD/.local/model-context.json"
```

The usage journal records provider-call events and available token measurements;
it is not a monetary spending cap. The context file records the exact prompt,
including source snippets, immediately before sending it. Keep both private.
Use a new context filename for each run: an existing capture is not overwritten,
and a capture failure stops the model call.

## Planned: Inkspect AI as a provider

The intended flow is to create an API key in Inkspect Settings, choose
**Inkspect AI** in the CLI, and use your workspace's credits for local review
requests. **API-key creation and this CLI provider are not implemented yet.**
There is no Inkspect key or endpoint to configure in this release. Use one of the
available providers above for now.

## Troubleshooting

| Problem | What to check |
| --- | --- |
| Cargo says the Rust version is too old | Install Rust 1.94.1 or newer, then build again with `--locked`. |
| `missing read token` or `pull not accessible` | GitHub credentials, selected repository access, and PR number. |
| `github.pull_request_changed` | The PR changed. Regenerate `.local/job.json` before retrying. |
| The model request is rejected | Provider key/permissions, exact model ID, account access, and output-token limit. For Bedrock, also check the region. |
| `source.invalid_ignore` | Fix the `.inkspectignore` policy in the comparison base. |
| `retrieval skipped` | Check Qdrant and AWS model access, or leave retrieval disabled for a first run. |
| `container clone mode is not yet implemented` | Set `sandbox.containerize` to `false`; use your own isolated runner if needed. |
| The report exists but GitHub has no comments | Check `publish_inline_comments`, write-token permissions, and publishing warnings. |
| You only have a local folder | Use `local --repo /path/to/project`. `review --repo-dir` is a separate advanced option that still requires GitHub. |

## Development

```sh
cargo test --locked
cargo run -- prompt
cargo run -- review --help
```

`prompt` prints the fixed review instructions without accessing a repository or
calling a model. Source modules are grouped by responsibility:

| Module | Purpose |
| --- | --- |
| `config.rs`, `main.rs` | Configuration and CLI commands. |
| `pipeline.rs`, `local.rs` | GitHub PR and local-folder review workflows. |
| `github.rs`, `github_app.rs` | PR verification, credentials and optional publishing. |
| `sandbox.rs`, `git.rs`, `ignore.rs` | Temporary clone, diff and exclusions. |
| `model.rs`, `byok.rs`, `prompts.rs` | Provider calls and separation of instructions from untrusted source. |
| `retrieval/` | Repository indexing, search, reranking and context selection. |
| `review.rs`, `render.rs` | Finding verification and report presentation. |
| `fix.rs`, `service.rs` | Repair proposals and integration operations. |

## License

Licensed under the [MIT License](LICENSE). The Inkspect name and logo identify
the project; using them must not imply endorsement of your own product.

## Inline snapshot reviews for integrations

Integrations can supply before/after source text without mounting a repository:

```sh
./target/release/inkspect-engine snapshot \
  --config .local/host.json --request examples/snapshot.example.json \
  --out .local/snapshot-report.json
```

Use the existing operator-owned model configuration and credential transport.
The snapshot command never reads paths from the supplied source list, accesses
GitHub, or executes project code. It computes changed lines from the inline
contents and uses the existing single-call reviewer and changed-line checks.

The request contains the original task, source or execution acceptance criteria,
a comparison baseline label, full before/after files, client-reported tests and
explicit omissions. See `examples/snapshot.example.json`. Added/deleted files
use null before/after values; context files have equal contents. Renames can be
represented as deletion plus addition. The snapshot ID is SHA-256 of the UTF-8
compact JSON file array sorted by path, with each file's fields ordered as
`path`, `before`, `after`, `role`. Prefix it with `sha256:`. Test records contain
`id`, `command`, `exit_code`, `output`, and that exact `snapshot_id`.

Limits are 750,000 request bytes, 80 files, 64,000 bytes per before/after value,
400,000 source bytes, 1-30 criteria, 10 test records with at most 8,000 output
bytes each, and 200 omissions. Sensitive paths, symlinks represented by callers,
binary contents and invalid digests must not be submitted. The engine validates
inline paths and bounded text and never resolves them on the filesystem.

The output wraps the ordinary review in `review` and includes `assessments`,
`evidence` and `snapshot_id`. Source citations are checked against the supplied
contents. They do not prove that a remote repository matches the snapshot.
Client test references are labeled `client_supplied`; execution criteria cannot
be independently satisfied by those claims. Missing/duplicate criteria or
unsupported citations become unverified. Declared omissions make coverage
incomplete. The integration owns source transport, authorization, storage and
cleanup; the engine performs no automatic retry of a paid model call.
