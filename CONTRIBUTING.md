# Contributing to Inkspect

Bug reports, documentation improvements, tests and focused code changes are welcome.
For a large change, open an issue first so maintainers can discuss the scope.

## Set up

Follow the README installation steps, then run:

```sh
cargo build --locked
cargo test --locked
```

Tests use local fixtures and loopback servers. They do not require provider keys,
paid model calls or a real GitHub repository. Some restricted environments need
permission to bind a loopback port. Never put credentials in test fixtures.

## Make a change

1. Create a branch for one focused change.
2. Follow the surrounding Rust style. Keep instructions separate from untrusted source.
3. Add regression tests for behavior changes, including failure cases.
4. Update relevant docs and examples when changing commands or configuration.
5. Run the checks above and describe exactly what you validated in your pull request.

Do not run code, hooks or filters from reviewed repositories. Provider keys must
not appear in logs, command arguments, reports or saved configuration. Changes to
review scope, provider requests, publishing or billing need explicit test coverage.

Model-generated findings are untrusted: keep location validation and strict JSON
parsing. Do not add automatic retries of ambiguous paid requests or GitHub writes.

## Pull requests

Explain the problem, the change, and verification. Screenshots are useful for
visual documentation changes, but optional. Do not include private source,
customer data, credentials or local configuration. Maintainers may request a
smaller change or a regression test before merging.

Contributions are provided under this repository's MIT license. By submitting a
contribution, you confirm that you have the right to contribute it under that license.
For security issues, follow [SECURITY.md](SECURITY.md) instead of a public issue.

## Commit and pull-request titles

Use `type(scope): short imperative description`, without a trailing period.
Keep the subject at most 72 characters. Use a specific scope such as `cli`,
`review`, `retrieval`, `docs`, `ci`, or `security`.

Examples:

- `feat(cli): add local project reviews`
- `fix(review): preserve changed-line locations`
- `docs(setup): explain provider credentials`

Types: `feat`, `fix`, `docs`, `test`, `refactor`, `perf`, `build`, `ci`, `chore`,
and `revert`. Use the same format for pull-request titles so squash commits
retain a consistent history. Preserve accurate authorship and describe the final
change rather than the conversation that produced it.

## Changelog required

Every shipped change needs an entry in `CHANGELOG.md` in the same commit or PR:
features, fixes, docs, configuration, tests and repository maintenance all count.
Write a concise user-facing description under `Unreleased` with the actual date.
Use Added/Changed/Fixed/Removed as appropriate. Link relevant history when useful;
never claim that a version was released until it actually was.
