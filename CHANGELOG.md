# Changelog

Every change shipped from this repository must be recorded here, including code,
documentation, configuration, tests and repository automation. Add an entry in
the same commit or pull request as the change. Use the actual change date and
link related commits where available; do not invent released version numbers.

This repository begins with a single clean public snapshot. The entries below
summarize development before that snapshot; earlier private commit history is not
included. The package's `0.1.0` version does not imply a published tagged release.

## Unreleased

### 2026-10-30

#### Added

- An AI-agent setup guide and helper that prepares private local configuration,
  checks/builds prerequisites, and runs a review only with `--run`. Added setup
  and key-handling regression tests and linked the workflow from the README.
- A starter `.inkspectignore` policy for build outputs, local credentials and
  key material, with parser coverage and instructions for adapting it without
  overwriting existing rules. Excluded Python bytecode caches from Git.
- Direct OpenAI, Anthropic and Google provider support, with credentials delivered
  through stdin and measured token usage. Added bounded repair proposals through
  `service --mode fix`; applying or publishing them remains an integration concern.
- Standalone installation, provider configuration and first-review instructions,
  generic example files, and light/dark Inkspect logo assets.
- Local reviews for Git working trees and ordinary source folders, with no GitHub
  account required. Includes uncommitted/untracked changes, deletions and an
  optional local comparison revision via `--base`; local source remains unchanged.
- Local-review regression tests for exclusions, line locations, staged/untracked
  changes, clean and unborn repositories, symlinks, secrets and size limits.
- A non-root Docker image and Compose services for the engine and optional Qdrant.
  No PostgreSQL dependency or service is included.
- MIT licensing, contribution/security/support/community guides, issue forms and
  a pull-request checklist. Added Linux/macOS CI, pull-request title validation,
  and Dependabot configuration for Cargo, Actions and Docker.

#### Fixed

- Local Git review in containers now trusts only the explicitly selected project
  root when the bind mount's ownership differs from the runtime user; global Git
  trust settings are not changed.

#### Changed

- Prepared a clean public repository with a single initial commit, generic
  examples and a GitHub noreply commit identity. Removed obsolete commit links.
- Renamed the package and executable from `inkspect-rust-engine` to
  `inkspect-engine`; updated commands and container entrypoints. Local reports
  identify a snapshot and omit the hosted dashboard link.
- Established `type(scope): short imperative description` for commit subjects
  and pull-request titles, with matching squash-merge settings.
- Rebuilt this changelog from actual history and established a mandatory changelog
  entry for every future shipped change, including documentation and maintenance.

### 2026-10-29

#### Changed

- Improved related-code selection with file diversity, cross-query deduplication,
  relevance filtering, source/test relationships and bounded context allocation.
- Expanded provider-usage auditing and preserved the exact pre-provider review
  context, including the selected retrieved chunks. Added retrieval and audit
  regression tests.

### 2026-10-28

#### Added

- Initial Rust review engine: GitHub PR snapshot verification, temporary cloning,
  structured diffs, repository context retrieval, one review-model call, finding
  validation, Markdown/JSON reports and optional inline PR comments.
- Integration operations for indexing, context retrieval, source reads and
  repository descriptions, with engine-authored report presentation.
- Documentation of the review pipeline, indexing, prompt boundaries, configuration
  and module responsibilities.

#### Changed

- Removed repository-specific test configuration and kept generic examples for
  users to adapt to their own repositories.

#### Fixed

- Corrected `.inkspectignore` line counting when the file has no trailing newline.
