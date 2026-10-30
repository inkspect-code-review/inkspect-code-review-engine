"""Exercise setup and credential boundaries without builds or provider calls."""
import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location("ai_setup", Path(__file__).with_name("ai-setup.py"))
setup = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(setup)


class SetupTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name) / "engine"
        (self.root / "examples").mkdir(parents=True)
        (self.root / "examples/host.example.json").write_text(json.dumps({
            "retrieval": {"content_bytes": 1234, "max_files": 20},
        }))
        self.project = Path(self.directory.name) / "project"
        self.project.mkdir()
        (self.project / ".inkspectignore").write_text("keep-existing-policy/\n")

    def invoke(self, provider="openai", run=False, environment=None, extra=()):
        args = ["ai-setup.py", "--repo", str(self.project), "--provider", provider,
                "--model", "test-model", *extra]
        if run:
            args.append("--run")
        output = io.StringIO()
        with patch.object(setup, "__file__", str(self.root / "scripts/ai-setup.py")), \
             patch.object(setup.sys, "argv", args), \
             patch.object(setup.shutil, "which", return_value="/tools/tool"), \
             patch.object(setup.subprocess, "check_output", return_value="rustc 1.94.1 (test)"), \
             patch.object(setup.subprocess, "run") as command, \
             patch.dict(os.environ, environment or {}, clear=True), \
             contextlib.redirect_stdout(output), contextlib.redirect_stderr(io.StringIO()):
            command.return_value.returncode = 0
            try:
                setup.main()
            except SystemExit:
                self.failed_calls = command.call_args_list
                raise
        return json.loads(output.getvalue()), command.call_args_list

    def test_setup_only_preserves_project_and_uses_private_config(self):
        state, calls = self.invoke()
        self.assertFalse(state["ran_review"])
        self.assertEqual(len(calls), 1)
        config_path = Path(state["config"])
        config = json.loads(config_path.read_text())
        self.assertEqual(config["retrieval"], {"content_bytes": 0, "max_files": 0})
        self.assertEqual(config_path.stat().st_mode & 0o777, 0o600)
        self.assertEqual(config_path.parent.stat().st_mode & 0o777, 0o700)
        self.assertEqual((self.project / ".inkspectignore").read_text(), "keep-existing-policy/\n")
        self.assertEqual(len(list(self.project.iterdir())), 1)

    def test_direct_keys_only_reach_review_stdin(self):
        for provider, variable in setup.KEYS.items():
            with self.subTest(provider=provider):
                environment = {name: "private-test-credential" for name in setup.KEYS.values()}
                state, calls = self.invoke(provider, True, environment, ("--base", "main"))
                review = calls[-1]
                credential = json.loads(review.kwargs["input"])
                self.assertEqual(credential["provider"], provider)
                self.assertEqual(credential["api_key"], environment[variable])
                self.assertEqual(review.kwargs["env"]["INKSPECT_BYOK_STDIN"], "1")
                for name in setup.KEYS.values():
                    self.assertNotIn(name, review.kwargs["env"])
                self.assertEqual(review.args[0][-2:], ["--base", "main"])
                self.assertNotIn("private-test-credential", json.dumps(state))
                self.assertNotIn("private-test-credential", Path(state["config"]).read_text())
                self.assertNotIn("private-test-credential", " ".join(review.args[0]))
                self.assertTrue(state["ran_review"])

    def test_missing_key_stops_before_build_or_config(self):
        with self.assertRaises(SystemExit) as error:
            self.invoke(run=True)
        self.assertEqual(error.exception.code, 2)
        self.assertEqual(self.failed_calls, [])
        self.assertFalse((self.root / ".local").exists())

    def test_bedrock_keeps_aws_credentials_without_inherited_byok(self):
        state, calls = self.invoke("bedrock", True, {
            "AWS_PROFILE": "test-profile", "INKSPECT_BYOK_STDIN": "1",
            "OPENAI_API_KEY": "unused-key",
        })
        review = calls[-1]
        self.assertIsNone(review.kwargs["input"])
        self.assertEqual(review.kwargs["env"]["AWS_PROFILE"], "test-profile")
        self.assertNotIn("INKSPECT_BYOK_STDIN", review.kwargs["env"])
        self.assertNotIn("OPENAI_API_KEY", review.kwargs["env"])
        self.assertTrue(state["ran_review"])


if __name__ == "__main__":
    unittest.main()
