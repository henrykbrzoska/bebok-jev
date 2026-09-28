# bebok-jev

Jev smart permission auto-approver for [Bebok](https://github.com/henrykbrzoska/bebok):
a structured decision model (TypeSafe `jev-1.13` via the OpenRouter Decisions
API) that auto-allows/denies tool calls before the user popup appears.

The engine sends each pending permission ask to the plugin's `decide`
action over JSON-lines stdio; the plugin returns
`{"decision": "allow" | "deny" | null}`. `null` (uncertain, or any error:
no API key, network failure, timeout, malformed response) falls through to
the user popup — the plugin never blocks the user.

## Setup

1. Install the plugin (Settings → General → Plugins → Install).
2. `export OPENROUTER_API_KEY=sk-or-v1-…` where the Bebok server runs
   (or point `JEV_API_KEY_ENV` at a differently-named variable).
3. `export JEV_ENABLED=1` before starting the server.

## Install as a Bebok plugin

The engine installs bebok-jev into a plugin slot
(`<project>/.bebok/plugins/bebok-jev/`, declaration
`<project>/.bebok/plugins/bebok-jev.json`) and runs it as a subprocess:
JSON-lines over the plugin's stdin/stdout, working directory = the slot dir.

- **Settings → General → Plugins → Install** (`POST /plugins/bebok-jev/install`).
  The engine resolves the release through its plugin registry: when the registry
  entry carries `asset_url` + `asset_sha256` it downloads the matching archive,
  verifies the SHA-256 and unpacks it; otherwise it falls back to
  `git clone` of the repository at its latest tag.
- **Update** (`POST /plugins/bebok-jev/update`) deletes the slot and installs
  it again, so a wiped or half-unpacked **binary is repaired**.

The slot manifest (`bebok-plugin.json`) carries the entrypoints:

| field | value |
| --- | --- |
| `entrypoint` | `bebok-jev --plugin-server` |
| `entrypoint_windows` | `bebok-jev.exe --plugin-server` |
| `entrypoint_unix` | `sh -c "if [ $(uname) = Darwin ]; then B=./bebok-jev-macos; else B=./bebok-jev; fi; chmod +x $B && exec $B --plugin-server"` |
| `prompt_file` | `AGENT_JEV.md` |

`prompt_file` names the prompt file in the slot: the engine reads
`AGENT_JEV.md` out of the slot dir and injects it as the plugin's agent
prompt. When the file (or the field) is missing the engine falls back to its
built-in prompt, so old slots keep working.

## Release assets

Every `vX.Y.Z` tag is built and published by
[`.github/workflows/release.yml`](./.github/workflows/release.yml):
three build legs (`windows-latest`, `ubuntu-latest`, `macos-latest`)
compile the binary and upload it as a workflow artifact; the `assemble` job
(`ubuntu-latest`) merges them into ONE universal asset plus a `checksums.txt`
and publishes the GitHub release.

| asset | contents |
| --- | --- |
| `bebok-jev-X.Y.Z.zip` | `bebok-jev.exe` (Windows), `bebok-jev` (Linux), `bebok-jev-macos` (macOS), `bebok-plugin.json`, `AGENT_JEV.md` — all at the **archive root** (no wrapping folder) |
| `checksums.txt` | `<sha256>  bebok-jev-X.Y.Z.zip`; the digest goes into the registry as `asset_sha256` |

One universal `.zip` because the engine registry carries a single `asset_url`
per plugin (no per-OS templating — the installer downloads the URL verbatim),
so every platform binary must ship in one package. The layout rule is the
engine's, which detects the format from the URL suffix only: a `.zip` must
keep every file at the archive root (a wrapping directory would bury the
manifest). The downloader requires `https`, verifies SHA-256 (64 hex chars),
caps the transfer at 256 MiB / 60 s, and rejects symlinks and absolute or `..`
entries.

Release prerequisites:

- The tag version must equal `version` in `Cargo.toml` **and** in
  `bebok-plugin.json`; the workflow fails before building otherwise.
- Each leg packages `target/release/bebok-jev[.exe]` and fails with a clear
  message when `cargo build --release --bin bebok-jev` produced no
  executable — the crate must keep the `bebok-jev` binary target.
- Publishing uses the preinstalled `gh` CLI with the workflow `GITHUB_TOKEN`;
  the `assemble` job creates the release, re-runs upload into it with
  `--clobber`. Only first-party actions are used (`checkout`,
  `upload-artifact`, `download-artifact`) plus `dtolnay/rust-toolchain@stable`.

### Why the Unix entrypoint is a `sh -c` shim

The engine unpacks archives **without preserving file modes** (zip external
attributes are ignored, tar entries are extracted with default permissions), so
after an asset install the Unix slot has a binary with no exec bit. A bare
`./bebok-jev` entrypoint would therefore fail with a permission error, even
though the same manifest works after a `git clone` in an environment where the
file kept its mode.

The shim sidesteps that: `/bin/sh -c "if [ $(uname) = Darwin ]; then
B=./bebok-jev-macos; else B=./bebok-jev; fi; chmod +x $B && exec $B
--plugin-server"` runs in the slot dir (the engine's cwd), picks the binary
matching the OS (the universal zip carries `bebok-jev` for Linux and
`bebok-jev-macos` for macOS side by side), makes it executable first, and
`exec` replaces the shell so the JSON-lines stdio talks to the plugin
directly and no wrapper process lingers. The engine's entrypoint parser
honours quotes, so the whole `-c` argument stays a single argv entry.

Once the engine preserves exec bits on unpack, `entrypoint_unix` can be dropped
(or simplified to `bebok-jev --plugin-server`, which is exactly the generic
`entrypoint` already in the manifest).

## License

GNU Affero General Public License v3.0 or later — see [LICENSE](./LICENSE).
