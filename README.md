<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/inkspect-symbol-white.svg">
    <img src="docs/assets/inkspect-symbol-black.svg" alt="Inkspect" width="64" height="80">
  </picture>
</p>

<h1 align="center">Inkspect Code Review Engine</h1>

Run AI reviews on a local project or GitHub pull request. The engine checks finding
locations against the diff and saves a Markdown or JSON report. Your chosen AI
provider receives the selected source and bills your account. Posting comments to
GitHub is optional.

## Review a local project

On macOS, Linux or WSL, install Git, Rust 1.94.1 or newer with Cargo, CMake and
a native C/C++ toolchain. Clone the engine and prepare a review from its checkout.

```sh
git clone https://github.com/inkspect-code-review/inkspect-code-review-engine.git
cd inkspect-code-review-engine
sh scripts/ai-setup.sh --repo /absolute/path/to/project --provider openai --model YOUR_MODEL_ID
```

Replace the path and model ID with your own. This builds the engine and prepares a
private configuration. To run the review, set `OPENAI_API_KEY` in your environment
and repeat the last command with `--run`. The receipt includes the report path.
For a Git working tree, the default comparison is the current files against `HEAD`.

## Inside a local review

[The local command](src/local.rs) builds a snapshot of the eligible files and sends the diff to the selected model. A clean working tree produces an empty review without a model call. [The review module](src/review.rs) checks proposed locations against the diff and assigns finding IDs. [The renderer](src/render.rs) turns the resulting report into Markdown and GitHub comments.

When changing how findings are identified or validated, run the review module’s unit tests from the engine checkout:

```sh
cargo test --locked --bin inkspect-engine review::tests::
```

## Other workflows

- [Review a GitHub pull request](docs/USAGE.md#2-connect-github) and [publish findings](docs/USAGE.md#post-the-review-to-github).
- [Choose a provider](docs/USAGE.md#5-choose-your-ai-provider-and-run) or [set up with a coding agent](docs/AI_SETUP.md).
- [Configure file exclusions](docs/USAGE.md#exclude-files-with-inkspectignore) and [add repository context](docs/USAGE.md#optional-add-repository-context).
- [Run with Docker Compose](docs/USAGE.md#docker-compose) or [use the engine in CI](docs/USAGE.md#self-hosting-and-ci).
- [Integrate inline snapshot reviews](docs/USAGE.md#inline-snapshot-reviews-for-integrations) or [troubleshoot a run](docs/USAGE.md#troubleshooting).

[MIT license](LICENSE) · [Contributing](CONTRIBUTING.md) · [Security](SECURITY.md) ·
[Support](SUPPORT.md) · [Changelog](CHANGELOG.md) · [Community guidelines](CODE_OF_CONDUCT.md)
