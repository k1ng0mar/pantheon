# Bundled plugins: content contract

This directory is the content side of Pantheon's bundled-plugin
mechanism. **Content workers add plugins by adding directories - no
code changes, no per-plugin hand edits.** The machinery (embedding,
seeding, enable/disable) lives in `build.rs`,
`build-support/gen_plugins.rs`, and `src/plugins.rs`.

## One directory per plugin

```
bundled-plugins/<name>/
```

- `<name>` is the plugin's registry key: ASCII alphanumerics plus
  `-`/`_`, 1..=64 chars. It should match the manifest's `name`.
- Dot-directories (`.foo`) are ignored.

## Exactly one manifest per directory

Each directory holds **either**:

- `manifest.yaml` - a **tool plugin** (Pantheon tool-plugin schema:
  `name`, `description`, `version`, `capabilities`, `env_vars`,
  `runner` default `run.sh`, `enabled`). Its tools are projected into
  the agent's tool registry behind the capability gate. Seeds to
  `<data_dir>/plugins/bundled/<name>/`.
- `plugin.yaml` - a **hook plugin** (extensions shape: `name`,
  `version`, `description`, `provides_hooks` and/or `hooks`). Fired
  per hook point as a child process with a scrubbed environment
  (PATH only). Seeds to `<data_dir>/extensions/bundled/<name>/`.

A directory with both manifests is skipped with a build warning; a
directory with neither is ignored.

## Text files only

Every file under the directory is embedded into the binary.
Only UTF-8 text files are embedded - anything else (binaries,
images) is skipped with a build warning. Keep plugins source-only.

A sha256 over the (path, content) pairs is embedded per plugin.
Seeding re-hashes the materialized directory and fails closed on
mismatch, so what ships in the binary is exactly what lands on disk.

## Enablement

Seeding is inert: an existing target directory is **never**
overwritten, and every bundled plugin ships disabled - with one
exception: `noisegate` ships with `enabled: true` in its manifest,
per Umar's direct directive (it is the one bundled plugin on by
default). Do not "normalize" it back to `false`. The config
file's `[plugins.<name>]` table is the single enablement state
(`pantheon plugins enable <name>`, the dashboard, the TUI toggle
screen, the agent's `enable_plugin` tool). For bundled plugins it
wins over the manifest's own `enabled` flag.

## Trust

A plugin is code Pantheon runs on the operator's machine with the
operator's user privileges - Pantheon does **not** sandbox plugins.
Bundled plugins are first-party (shipped in this repo, reviewed
in-tree), so they skip the third-party approval store, but they do
not skip enablement: enabling one is consent to run its code.
