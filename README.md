# Hermes Agent for POM

A POM plugin (`plugin_code: hermes`) that installs the official [Hermes Agent](https://github.com/NousResearch/hermes-agent) on the local node and embeds its web dashboard in a POM screen. The plugin's setup and status use POM panels, colors and English/Portuguese translations. Hermes itself retains its own dashboard layout.

## Use

1. Install this plugin from **Plugins > Add from GitHub**, or rebuild the sibling `pom-plugin-hermes` checkout in POM development mode.
2. Open **Hermes Agent** in the menu. Enter the ID of a model already deployed in POM to preconfigure Hermes's custom OpenAI endpoint, or leave the field blank to configure a different provider in Hermes. Click **Install and open Hermes**.
3. Wait for the background installation. On success the Hermes dashboard opens inside POM. Its settings, sessions and keys remain on this node. Installation may take several minutes and multiple gigabytes.

The plugin downloads and verifies the SHA-256 pinned official `NousResearch/hermes-agent` source at commit `5894e750c3dba980d43b25aa29e3958149ba2c61` and a SHA-256 pinned uv executable for the host platform. It uses uv to install managed Python 3.14.9 and invokes Hermes's own PM to provision its hash-verified Python dependencies and Node toolchain. The official source-completion entry point builds the TUI and dashboard, syncs bundled skills and migrates settings. No third-party executable or source is bundled in the plugin; all downloads, including the upstream PM's tools, happen on first install and require network access. The optional POM model configuration is saved before the upstream configuration stage; existing settings are never overwritten. Credentials stay in Hermes's local data home and are never returned by RPC.

Runtime, source checkout and logs: `<directory of POM_PLUGIN_DB>/hermes/` (`install.log`, `dashboard.log`, `bridge.log`). User settings, keys and sessions: `<workspace_root>/.hermes/` (or runtime `data/` without a configured workspace). Only the loopback bridge is published through POM's authenticated `/api/ui/plugins/hermes/proxy/`; it checks a per-launch proxy token and relays to a loopback-only Hermes dashboard. The plugin reopens an installed dashboard after a host restart. A failed installation shows an error in the setup screen with its log location; **Retry installation** resumes without deleting existing user data. Upstream browser/computer-use extras are intentionally skipped; use the Hermes dashboard/PM to add them as needed.

**Requirements:** A supported POM host (Linux x86_64/aarch64, macOS x86_64/arm64, Windows x86_64/arm64) with outbound HTTPS access to GitHub, Python and npm registries and enough disk space for the managed runtime (several gigabytes). No preinstalled Python, bash, Git, curl or Node, and no special POM Docker image, is needed for installation. Linux installation and the live dashboard were verified end-to-end without those system tools; native macOS and Windows runner tests also installed Hermes and served the authenticated dashboard, and Windows built the release-mode plugin library. The plugin has not yet been field-tested inside a Windows POM node. Source tarball installs do not have a Git checkout, so upstream Git-based self-update requires a separate checkout. The Hermes dashboard is powerful: anyone authenticated to this node's POM plugin UI can control Hermes and its stored secrets. Do not expose the POM administration UI to untrusted operators. Model credentials do not automatically rotate in Hermes when POM's gateway key changes; update them in Hermes's Keys page.

## Build and verify

```sh
scripts/build-ui.sh
POM_UI_REQUIRE_DIST=1 cargo test --locked
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
python3 -m unittest discover -s tests -p 'test_runtime.py'
tests/test_ci_plan.sh
scripts/package.sh --platform linux-x86_64 --version 0.1.0
```

For a full network-backed standalone install and authenticated dashboard test (downloads several gigabytes), run `cargo test --locked installer::tests::installs_without_system_python_git_or_curl -- --ignored --nocapture`; the manual **Standalone Hermes install smoke** GitHub Actions workflow offers the same test on Windows, macOS and Linux. The local dev rebuild uses `scripts/dev-ui.sh` and a read-only source mount. The built UI, manifest and docs ship inside the verified cdylib; `scripts/package.sh` produces the GitHub release asset and per-platform manifest. The library is loaded only by `pom-plugin-host`.
