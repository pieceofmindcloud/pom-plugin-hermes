"""pom_provider tests with stubbed Hermes modules; no install, no network."""
import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import types
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]


def load_pom_provider():
    state = {"config": {}}

    def save_config(config):
        state["config"] = config

    config_module = types.ModuleType("hermes_cli.config")
    config_module.load_config = lambda: dict(state["config"])
    config_module.save_config = save_config
    hermes_cli = types.ModuleType("hermes_cli")
    sys.modules["hermes_cli"] = hermes_cli
    sys.modules["hermes_cli.config"] = config_module
    pm = types.ModuleType("pm")
    pm_environments = types.ModuleType("pm.environments")
    pm_environments.activate_dependencies = lambda root: None
    pm.environments = pm_environments
    sys.modules["pm"] = pm
    sys.modules["pm.environments"] = pm_environments
    spec = importlib.util.spec_from_file_location("pom_provider", ROOT / "pom_provider.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module, state


class PomProviderTests(unittest.TestCase):
    def setUp(self):
        self.module, self.state = load_pom_provider()
        self.home = Path(tempfile.mkdtemp(prefix="hermes-pom-provider-test-"))
        self.previous_home = self.module.os.environ.get("HERMES_HOME")
        self.module.os.environ["HERMES_HOME"] = str(self.home)

    def tearDown(self):
        if self.previous_home is None:
            self.module.os.environ.pop("HERMES_HOME", None)
        else:
            self.module.os.environ["HERMES_HOME"] = self.previous_home

    def fake_gateway(self, payload):
        response = mock.Mock()
        response.read.return_value = json.dumps(payload).encode()
        response.__enter__ = mock.Mock(return_value=response)
        response.__exit__ = mock.Mock(return_value=False)
        return mock.patch("urllib.request.urlopen", return_value=response)

    def test_requires_a_loopback_gateway_and_a_plain_key(self):
        with self.assertRaises(ValueError):
            self.module.connect("https://evil.example/v1", "sk-good")
        with self.assertRaises(ValueError):
            self.module.connect("http://127.0.0.1:8080/v1", "sk bad key")
        self.assertFalse((self.home / ".env").exists())
        self.assertEqual(self.state["config"], {})

    def test_registers_pom_with_discovered_models_and_a_private_secret(self):
        url = "http://127.0.0.1:8080/v1"
        with self.fake_gateway({"data": [{"id": "m1"}, {"id": "m2"}, {"id": 7}, {"id": ""}]}):
            self.module.connect(url, "sk-pom")
        entry = self.state["config"]["providers"]["pom"]
        self.assertEqual(
            entry,
            {"name": "POM", "base_url": url, "key_env": "POM_API_KEY",
             "models": {"m1": {}, "m2": {}}},
        )
        env_text = (self.home / ".env").read_text()
        self.assertIn("POM_API_KEY=sk-pom\n", env_text)
        self.assertTrue(len(env_text.splitlines()) >= 1)

    def test_an_offline_gateway_still_registers_the_provider(self):
        url = "http://127.0.0.1:1/v1"
        with mock.patch("urllib.request.urlopen", side_effect=OSError("down")):
            self.module.connect(url, "sk-pom")
        entry = self.state["config"]["providers"]["pom"]
        self.assertEqual(entry["base_url"], url)
        self.assertNotIn("models", entry)
        self.assertIn("POM_API_KEY=sk-pom\n", (self.home / ".env").read_text())

    def test_never_hijacks_a_user_owned_pom_provider(self):
        self.state["config"] = {"providers": {"pom": {"base_url": "http://127.0.0.1:9999/v1",
                                                      "key_env": "OTHER_KEY"}}}
        (self.home / ".env").write_text("OTHER_KEY=user-secret\n")
        with self.assertRaises(ValueError):
            self.module.connect("http://127.0.0.1:8080/v1", "sk-pom")
        self.assertEqual(
            self.state["config"]["providers"]["pom"]["base_url"],
            "http://127.0.0.1:9999/v1",
        )
        self.assertNotIn("POM_API_KEY", (self.home / ".env").read_text())

    def test_reruns_are_idempotent_and_preserve_other_secrets(self):
        url = "http://127.0.0.1:8080/v1"
        (self.home / ".env").write_text("OTHER=keep\n")
        self.module.connect(url, "sk-pom")
        first = (self.home / ".env").read_text()
        self.module.connect(url, "sk-pom")
        self.assertEqual((self.home / ".env").read_text(), first)
        self.assertIn("OTHER=keep", first)
        self.assertIn("POM_API_KEY=sk-pom\n", first)


if __name__ == "__main__":
    unittest.main()
