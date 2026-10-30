#!/usr/bin/env python3
"""Prepare an isolated local-review config; call a provider only with --run."""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

KEYS = {"openai": "OPENAI_API_KEY", "anthropic": "ANTHROPIC_API_KEY", "google": "GEMINI_API_KEY"}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", required=True, type=Path)
    parser.add_argument("--provider", required=True, choices=["bedrock", *KEYS])
    parser.add_argument("--model", required=True, help="Exact accessible model or Bedrock profile ID")
    parser.add_argument("--region", default="eu-west-2", help="AWS region for Bedrock")
    parser.add_argument("--max-output-tokens", type=int, default=8192)
    parser.add_argument("--base", help="Optional local Git comparison commit/branch")
    parser.add_argument("--run", action="store_true", help="Run the review; this can incur provider charges")
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    repo = args.repo.expanduser().resolve()
    if not repo.is_dir():
        parser.error("--repo must be an existing project directory")
    if not args.model.strip() or len(args.model) > 200:
        parser.error("Supply an exact model ID from your provider account")
    if not 1 <= args.max_output_tokens <= 16384:
        parser.error("--max-output-tokens must be between 1 and 16384")
    for tool in ["git", "cargo", "rustc"]:
        if not shutil.which(tool):
            parser.error(f"Missing {tool}; install the prerequisites from README.md first")
    rust_version = subprocess.check_output(["rustc", "--version"], text=True).split()[1]
    version = tuple(int(x) for x in rust_version.split("-")[0].split("."))
    if version < (1, 94, 1):
        parser.error("Rust 1.94.1 or newer is required")
    if args.run and args.provider in KEYS and not os.environ.get(KEYS[args.provider]):
        parser.error(f"Set {KEYS[args.provider]} privately before using --run; never paste it into logs")
    subprocess.run(["cargo", "build", "--locked", "--target-dir", str(root / "target")],
                   cwd=root, check=True, stdout=sys.stderr)
    private = root / ".local"
    private.mkdir(mode=0o700, exist_ok=True)
    output = Path(tempfile.mkdtemp(prefix="agent-review-", dir=private))
    config = json.loads((root / "examples/host.example.json").read_text())
    config.update(model_id=args.model, model_region=args.region, model_max_output_tokens=args.max_output_tokens)
    config["retrieval"].update(content_bytes=0, max_files=0)
    config_path = output / "host.json"
    config_path.write_text(json.dumps(config, indent=2) + "\n")
    config_path.chmod(0o600)
    binary = root / "target/debug/inkspect-engine"
    report = output / "report.json"
    command = [str(binary), "local", "--config", str(config_path), "--repo", str(repo), "--format", "json", "--out", str(report)]
    if args.base:
        command += ["--base", args.base]
    state = {"provider": args.provider, "model": args.model, "repo": str(repo), "config": str(config_path), "report": str(report), "ran_review": False}
    if args.run:
        environment = dict(os.environ)
        # Do not let an inherited pipe flag select an unintended provider.
        environment.pop("INKSPECT_BYOK_STDIN", None)
        payload = None
        if args.provider in KEYS:
            credential = {"provider": args.provider, "model": args.model,
                          "api_key": environment[KEYS[args.provider]], "max_output_tokens": config["model_max_output_tokens"]}
            payload = json.dumps(credential)
            environment["INKSPECT_BYOK_STDIN"] = "1"
        for name in KEYS.values():
            environment.pop(name, None)
        result = subprocess.run(command, input=payload, text=True, env=environment, stdout=sys.stderr)
        if result.returncode:
            raise SystemExit(result.returncode)
        state["ran_review"] = True
    print(json.dumps(state, indent=2))


if __name__ == "__main__":
    main()
