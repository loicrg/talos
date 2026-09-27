# Talos

Talos manages self-hosted GitHub Actions runners on Linux machines from your workstation. It uses your existing OpenSSH configuration, SSH agent and `known_hosts`; nothing needs to be installed on a managed host.

This initial release covers host and fleet inventory, remote host inspection, local runner discovery, repository-scoped GitHub reconciliation, systemd lifecycle operations, sequential runner provisioning, desired-count scaling, a read-only doctor command and an interactive terminal manager.

## Requirements

On the workstation:

- Rust and Cargo (edition 2024)
- OpenSSH client configured for the target hosts
- GitHub CLI authenticated to `github.com` (`gh auth login`)
- Repository administrator access to list, register and unregister repository runners

On each managed host:

- Linux, Python 3, and OpenSSH access for the configured account
- systemd and non-interactive `sudo -n` for service operations and provisioning
- `curl`, `tar`, and `sha256sum` for provisioning
- A supported Linux runner architecture: x64, ARM64 or ARM

Talos does not accept SSH keys or GitHub tokens as configuration values. SSH credentials stay in OpenSSH and registration tokens are short lived.

## Build and run

```sh
cargo build --release
./target/release/talos --help
```

To install the executable into Cargo's user binary directory:

```sh
cargo install --path .
```

Useful checks while developing:

```sh
cargo fmt --check
cargo check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all
```

Python 3 is needed on the development machine for the systemd fixture tests. Managed hosts also need Python 3 for discovery and provisioning.

## Configuration

Talos stores host inventory and fleet membership as TOML at:

```text
$XDG_CONFIG_HOME/talos/config.toml
```

When `XDG_CONFIG_HOME` is unset, it uses `~/.config/talos/config.toml`. The configuration directory is mode `0700` and the file is mode `0600` on Unix. You can point a command at another file with `--config PATH`.

Only Talos metadata is stored locally. Talos does not keep a database of remote observations or make local state authoritative for runner existence. Each managed installation carries a small `.talos-managed` ownership marker in its own directory.

Add the first host:

```sh
talos host add builder-01 \
  --ssh runner@builder-01.example.com
talos host list
talos host show builder-01
```

`--ssh` accepts an OpenSSH host alias or `[user@]host`. Use your normal `~/.ssh/config` for keys, agent forwarding, `ProxyJump`, and aliases. Talos keeps OpenSSH host verification enabled and invokes SSH in batch mode, so an unavailable password prompt fails with an actionable error.

Runner and archive paths default to `~/.local/share/talos/runners` and `~/.cache/talos/github-runner` on the remote host. Set `runner_root` and `cache_root` in the host's TOML entry to choose other absolute directories below the SSH account's home; `talos host add` also accepts `--runner-root` and `--cache-root`.

## Inspect and discover

Start with read-only operations:

```sh
talos inspect builder-01
talos inspect builder-01 --json
talos runners --host builder-01
talos runner list --host builder-01 --repo loicrg/lemnos --json
talos doctor --host builder-01
```

Inspection reads OS, kernel, CPU, memory, load, disk and systemd data, then searches conventional Linux installation roots, runner service definitions and running `Runner.Listener` processes. It reads runner configuration; it does not execute scripts found during discovery.

When a runner's local configuration identifies a repository, `runners` reconciles it against the GitHub repository runner API using `gh auth token` on the workstation. Pass `--repo OWNER/NAME` to include GitHub registrations that have no corresponding local installation. Without a known repository, Talos cannot enumerate every repository where a machine might have a registration.

Talos reports separate local and GitHub state, including local-only installations, GitHub-only registrations, service state, busy state, ownership, and health. Existing runners are external until they carry Talos's ownership marker; discovery never adopts them.

## Add and manage runners

Provision repository runners on a host:

```sh
talos runner add --host builder-01 --repo loicrg/lemnos
talos runner add --host builder-01 --repo loicrg/lemnos --count 3 \
  --label gpu --label linux-large
```

Provisioning is sequential. Talos validates SSH, GitHub access and non-interactive sudo; downloads the current official Linux runner asset; verifies the asset digest published by GitHub's release API; creates an isolated directory; configures the runner with a short-lived registration token; installs the official `svc.sh` service; enables and starts it; then checks both the local process and GitHub's online status. If a later step fails, Talos attempts to unregister partial state and removes the directory only when the registration is gone and the Talos marker matches. Incomplete rollback is reported with the retained state.

Default custom labels are `talos`, `host-<host-name>` and `repo-<owner>-<repo>`. The runner distribution keeps its standard labels. Additional labels can be supplied with repeated `--label` options.

Talos-managed files use this per-runner layout:

```text
~/.local/share/talos/runners/<owner>/<repo>/<slot>/
~/.cache/talos/github-runner/<version>/<official-runner-archive>
```

Commands include `runner start`, `runner stop`, `runner restart`, `runner logs`, and `service install|remove|enable|disable`. Stopping or restarting checks GitHub's busy state; unknown state fails closed unless `--force` is provided. `--adopt` is required before service commands execute `svc.sh` from a runner installed outside Talos. Talos validates that a discovered unit points back into that runner installation before modifying an existing service.

## Desired-count scaling

`--count` is the total desired runner count on the selected host, not an additive quantity:

```sh
talos scale --host builder-01 --repo loicrg/lemnos --count 4 --dry-run
talos scale --host builder-01 --repo loicrg/lemnos --count 4
```

The planner counts distinct names known locally or registered with GitHub. Scale-up creates only the difference. Scale-down is permitted only when enough runners are Talos-managed, have an active service, and have a known idle state at GitHub. Manual, busy, unknown, process-only, or GitHub-only registrations are not removal candidates. If no safe plan can reach the desired count, Talos explains the blocker and makes no changes. Applying a downscale confirms the GitHub busy state, removes each selected service, verifies the service is gone, unregisters the runner, then deletes its marked installation. It requires confirmation (or explicit `--yes` in a non-interactive session).

The scale planner reports CPU, memory and disk. If requested runner count exceeds logical CPU count it warns that actual workflow load determines whether the host is appropriate; it does not invent a universal safe runner limit.

## Removal and safety

These operations have separate commands and explicit semantics:

- `runner stop` stops a service but leaves it enabled and registered.
- `service disable` disables startup but leaves the service installed.
- `service remove` stops, disables and removes the runner's systemd service.
- `runner remove --unregister` removes the GitHub registration after confirming the runner is idle; unless `--force` is supplied, its service must already be inactive and disabled.
- `runner remove --delete-files` removes a runner installation only after GitHub no longer lists it.

`runner remove` with no action flags does nothing and returns an error. For a GitHub-only registration with no local installation, provide its repository scope explicitly, for example `talos runner remove orphaned-runner --host builder-01 --repo OWNER/NAME --unregister --yes`; Talos re-checks the GitHub busy state before deleting the registration. File deletion requires `--yes` or an interactive confirmation and is restricted remotely to a canonical path below Talos runner storage with a matching regular `.talos-managed` marker. External runner directories cannot be deleted through Talos.

## Fleets and TUI

Fleets are local groups of host names:

```sh
talos fleet create builders
talos fleet add builders builder-01
talos fleet list
```

Fleet-wide scheduling is not implemented. Scaling requires one selected `--host`.

Run `talos tui` for the Ratatui manager. Use Up/Down or `j`/`k` to select a host, Enter to open its runner list, and then select an individual runner. Runner actions include `s` start, `x` stop, `R` restart, `l` logs, `i` service install, `e` service enable, `E` service disable, `v` service remove, `u` unregister, `X` unregister plus Talos-owned file deletion, and `g` desired-count scaling for a repository. Destructive operations require confirmation and reuse the same lifecycle and scale safeguards as the CLI. `r` refreshes state, `d` opens the read-only doctor view, Escape goes back, and `q` quits.

## Troubleshooting

- **SSH fails:** connect with `ssh <configured-target>` first and verify host-key trust, the SSH agent, and any `ProxyJump` in your OpenSSH configuration.
- **GitHub API returns 403:** the authenticated `gh` account needs repository administrator access. Fine-grained tokens need repository Administration permissions; creating runner tokens requires write access.
- **Provisioning says sudo is unavailable:** configure passwordless `sudo -n` for service management or use a suitable administrative account. Talos will not wait for a password prompt.
- **No runner directories found:** ensure the host has Python 3. Talos searches common system and home roots, service `ExecStart` paths, and live runner processes. It never executes a discovered script to identify a runner version.
- **Scale-down is blocked:** inspect runner ownership and GitHub busy state. Manually installed runners are intentionally excluded; Talos will not stop a busy or state-unknown runner by default.
- **Runner archive checksum is unavailable:** Talos requires the SHA-256 digest returned by GitHub's release API and fails closed if it is missing or malformed.

## Architecture

See [docs/architecture.md](docs/architecture.md) for module boundaries, reconciliation behavior, and operational limits. Use [docs/live-validation.md](docs/live-validation.md) for the repeatable real-host validation procedure.
