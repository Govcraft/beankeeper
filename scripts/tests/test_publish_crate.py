"""Exercise publishing decisions with mocked registry and Cargo calls; never upload."""

from contextlib import redirect_stderr, redirect_stdout
import importlib.util
import io
import json
from pathlib import Path
import subprocess
import unittest
from unittest.mock import patch
from urllib.error import HTTPError, URLError


SCRIPT = Path(__file__).resolve().parents[1] / "publish_crate.py"
SPEC = importlib.util.spec_from_file_location("publish_crate", SCRIPT)
publisher = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(publisher)


class RegistryVersionTests(unittest.TestCase):
    def test_exact_version_is_present(self):
        response = io.BytesIO(json.dumps({"version": {"num": "0.7.0"}}).encode())
        with patch.object(publisher, "urlopen", return_value=response) as registry:
            self.assertTrue(publisher.is_published("beankeeper", "0.7.0"))
        request = registry.call_args.args[0]
        self.assertEqual(request.full_url, "https://crates.io/api/v1/crates/beankeeper/0.7.0")
        self.assertEqual(registry.call_args.kwargs["timeout"], 30)

    def test_missing_crate_or_version_is_not_published(self):
        error = HTTPError("https://crates.io", 404, "Not Found", {}, None)
        with patch.object(publisher, "urlopen", side_effect=error):
            self.assertFalse(publisher.is_published("beankeeper-cli", "0.21.0"))

    def test_registry_errors_do_not_mean_missing_version(self):
        for error in (
            HTTPError("https://crates.io", 403, "Forbidden", {}, None),
            HTTPError("https://crates.io", 429, "Too Many Requests", {}, None),
            HTTPError("https://crates.io", 500, "Server Error", {}, None),
            URLError("connection failed"),
        ):
            with self.subTest(error=error), patch.object(publisher, "urlopen", side_effect=error):
                with self.assertRaises(URLError):
                    publisher.is_published("beankeeper", "0.7.0")

    def test_mismatched_response_fails(self):
        response = io.BytesIO(b'{"version": {"num": "0.6.0"}}')
        with patch.object(publisher, "urlopen", return_value=response):
            with self.assertRaises(ValueError):
                publisher.is_published("beankeeper", "0.7.0")


class PublishCrateTests(unittest.TestCase):
    def setUp(self):
        registry = patch.object(publisher, "is_published", return_value=False)
        self.registry = registry.start()
        self.addCleanup(registry.stop)
        cargo = patch.object(
            publisher.subprocess, "run",
            return_value=subprocess.CompletedProcess([], 0, "Published successfully\n"),
        )
        self.cargo = cargo.start()
        self.addCleanup(cargo.stop)
        self.output = io.StringIO()
        stdout = redirect_stdout(self.output)
        stdout.__enter__()
        self.addCleanup(stdout.__exit__, None, None, None)

    def test_existing_library_version_skips_cargo(self):
        self.registry.return_value = True
        self.assertEqual(publisher.publish("beankeeper"), 0)
        self.cargo.assert_not_called()
        self.assertIn("already published", self.output.getvalue())

    def test_missing_library_version_publishes_with_locked_dependencies(self):
        self.assertEqual(publisher.publish("beankeeper"), 0)
        self.cargo.assert_called_once_with(
            ["cargo", "publish", "-p", "beankeeper", "--locked"],
            cwd=publisher.ROOT, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
            text=True, check=False,
        )

    def test_cli_is_never_published(self):
        with self.assertRaises(ValueError):
            publisher.publish("beankeeper-cli")
        self.registry.assert_not_called()
        self.cargo.assert_not_called()
        with patch("sys.argv", ["publish_crate.py", "beankeeper-cli"]), \
                redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as exit_:
            publisher.main()
        self.assertNotEqual(exit_.exception.code, 0)
        self.cargo.assert_not_called()

    def test_already_uploaded_race_succeeds_once_registry_confirms(self):
        self.registry.side_effect = [False, True]
        self.cargo.return_value = subprocess.CompletedProcess(
            [], 101, "error: crate version `0.7.0` is already uploaded",
        )
        self.assertEqual(publisher.publish("beankeeper"), 0)
        self.assertIn("already published", self.output.getvalue())

    def test_already_uploaded_message_without_registry_confirmation_fails(self):
        self.registry.side_effect = [False, False]
        self.cargo.return_value = subprocess.CompletedProcess([], 101, "already exists")
        self.assertEqual(publisher.publish("beankeeper"), 101)

    def test_library_publish_failures_fail(self):
        for message in (
            "the remote server responded with an error (status 403 Forbidden)",
            "401 Unauthorized", "only trusted publishing is allowed",
            "error: could not compile", "network timeout", "500 Internal Server Error",
        ):
            with self.subTest(message=message):
                self.cargo.return_value = subprocess.CompletedProcess([], 101, message)
                self.assertEqual(publisher.publish("beankeeper"), 101)
        self.assertNotIn("::warning::", self.output.getvalue())

    def test_registry_failure_never_attempts_upload(self):
        self.registry.side_effect = URLError("connection failed")
        with self.assertRaises(URLError):
            publisher.publish("beankeeper")
        self.cargo.assert_not_called()


if __name__ == "__main__":
    unittest.main()
