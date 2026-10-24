# Hermes Agent

Open **Hermes Agent** in the POM sidebar. On the first visit, enter an optional POM model ID (from a model already deployed on this node's network), then select **Install and open Hermes**. Without a POM model, choose your provider in Hermes after the dashboard opens. The installation runs in the background and can be retried without losing settings. It downloads several gigabytes and needs access to GitHub and the upstream dependency registries.

The POM screen hosts the official Hermes dashboard at the POM origin; its menus, channels, model settings and credentials are managed by Hermes. The dashboard is local to this node. An authenticated POM operator who can open this plugin can manage all of this Hermes installation, including its stored keys. Do not grant POM UI access to untrusted operators.

The POM model connection saves `model.provider: custom` and the POM's loopback `/v1` endpoint in Hermes's `config.yaml`, and saves the gateway credential in its owner-only `.env`. It applies only during the first setup; later edits to Hermes's configuration stay under Hermes's control. A rotated POM gateway key must be refreshed in Hermes's Keys page.

The plugin skips the upstream installer’s optional browser and computer-use components. To enable them later, follow the upstream Hermes PM instructions. Consult the Hermes project's dashboard documentation for configuration of messaging platforms, other providers and tools.
