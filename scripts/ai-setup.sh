#!/bin/sh
# Build the trusted engine checkout, then use its native setup command.
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
for tool in git cargo rustc; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        printf 'Missing %s; install the prerequisites from README.md first.\n' "$tool" >&2
        exit 1
    fi
done
(
    cd "$root"
    # Build scripts do not need direct-provider keys or the review pipe flag.
    unset OPENAI_API_KEY ANTHROPIC_API_KEY GEMINI_API_KEY INKSPECT_BYOK_STDIN
    cargo build --locked --bin inkspect-engine --target-dir "$root/target" >&2
)
exec "$root/target/debug/inkspect-engine" setup --output-dir "$root/.local" "$@"
