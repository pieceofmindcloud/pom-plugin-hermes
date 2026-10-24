"""Register POM as a named Hermes provider without changing the chosen model.

Runs under Hermes's managed Python, not the host's. Never prints credentials.
Only the `providers.pom` entry owned by this integration is updated.
"""
import json
import os
from pathlib import Path
import re
import sys
import tempfile
import urllib.request

from pm.environments import activate_dependencies

root = Path(__file__).resolve().parent / "source"
activate_dependencies(root)
from hermes_cli.config import load_config, save_config


def model_ids(url, key):
    try:
        request = urllib.request.Request(url + "/models", headers={"Authorization": "Bearer " + key})
        with urllib.request.urlopen(request, timeout=3) as response:
            payload = json.load(response)
        data = payload.get("data", [])
        if not isinstance(data, list):
            return None
        return sorted({item["id"] for item in data if isinstance(item, dict)
                       and isinstance(item.get("id"), str) and 0 < len(item["id"]) <= 200})[:512]
    except (OSError, ValueError, TypeError, KeyError):
        # An offline deployment does not disable the provider. Hermes's model
        # picker can still discover models from /v1/models on explicit refresh.
        return None


def set_secret(home, key):
    path = home / ".env"
    if path.is_symlink():
        raise RuntimeError("Hermes credentials path is a symlink")
    old = path.read_text(encoding="utf-8") if path.exists() else ""
    lines = [line for line in old.splitlines() if not re.match(r"^\s*POM_API_KEY\s*=", line)]
    lines.append("POM_API_KEY=" + key)
    updated = "\n".join(lines) + "\n"
    if updated == old:
        return
    fd, name = tempfile.mkstemp(prefix=".pom-env-", dir=home)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as output:
            output.write(updated)
        os.chmod(name, 0o600)
        os.replace(name, path)
    finally:
        if os.path.exists(name):
            os.unlink(name)


def connect(url, key):
    if not re.fullmatch(r"http://(?:127\.0\.0\.1|localhost|\[::1\]):[0-9]+/v1", url):
        raise ValueError("POM gateway must be a loopback /v1 endpoint")
    if not re.fullmatch(r"[A-Za-z0-9._~-]+", key):
        raise ValueError("Invalid POM API key")
    home = Path(os.environ["HERMES_HOME"])
    config = load_config()
    providers = config.get("providers") or {}
    if not isinstance(providers, dict):
        raise ValueError("Hermes providers configuration is not a mapping")
    existing = providers.get("pom")
    if existing is not None and (not isinstance(existing, dict) or
                                 existing.get("base_url") != url or
                                 existing.get("key_env") != "POM_API_KEY"):
        # Never hijack a provider created by a Hermes user.
        raise ValueError("Hermes provider 'pom' already belongs to a different endpoint")
    ids = model_ids(url, key)
    provider = dict(existing or {})
    provider.update({"name": "POM", "base_url": url, "key_env": "POM_API_KEY"})
    if ids is not None:
        provider["models"] = {model: {} for model in ids}
    set_secret(home, key)
    if provider != existing:
        providers["pom"] = provider
        config["providers"] = providers
        save_config(config)


if __name__ == "__main__":
    connect(sys.argv[1], os.environ["POM_GATEWAY_KEY"])
