# Security policy

## Report a vulnerability privately

Do not put credentials, private source, exploit details or customer information
in public issues. Contact **privacy@inkspect.si** with the subject
“Inkspect engine security report”. Include the affected version/commit, a minimal
reproduction using synthetic data, expected impact, and a way to contact you.
Do not send live API keys. If a key was exposed, revoke it through its provider.

Maintainers will investigate and coordinate disclosure. There is no guaranteed
response time or bug bounty. Security fixes target the latest release/current main;
older versions have no guaranteed backport support.

## Security boundaries

The engine reads source and sends selected context to your configured model
provider. Self-hosting the CLI does not make provider inference local. Optional
Qdrant indexing stores source chunks and uses AWS embedding/reranking services.

The engine does not execute project tests, hooks or model-proposed commands.
Host-mode temporary cloning is not an OS security sandbox. Use an isolated runner
for untrusted projects, scoped credentials, resource limits and private artifacts.
Local review skips symlinks and common sensitive/generated paths, but is not a
comprehensive secret scanner. Inspect exclusions before sending source externally.

Model output is untrusted. Findings must be anchored to the supplied review scope.
Prompt captures contain source code and should have restricted access and retention.
