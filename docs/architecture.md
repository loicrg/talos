# Architecture and operational model

Talos has a local CLI/TUI application boundary and an agentless remote execution boundary. Both frontends call the same `Application` operations and domain types.

```text
CLI / Ratatui
      │
      ▼
Application ───── ConfigStore (XDG TOML inventory)
      │
      ├── Remote discovery ── structured OpenSSH command execution ── Linux host
      ├── GitHub client ────── gh token + GitHub REST API ───────────── github.com
      ├── Reconciliation ───── local installations + GitHub registrations
      └── Scale planner ───── desired count → create/remove plan
```

## Module responsibilities

- `domain` owns host, repository, installation, ownership, health, runner and scale plan models.
- `app::config` validates host data and stores local metadata atomically with restrictive Unix permissions.
- `app::context` resolves hosts and composes host inspection with repository-scoped GitHub observations.
- `ssh` builds remote commands from a program and argument list, quotes every remote shell word, captures output/status/duration, times out operations, and redacts explicitly marked secrets.
- `remote` executes one fixed, read-only Python inventory script. It reads systemd unit details, common filesystem roots, runner configuration and `/proc`; it never evaluates data from a discovered installation.
- `github` obtains the user's existing `gh` CLI token in a local subprocess and uses it only in local HTTPS API requests. Runner registration and removal tokens are separately typed and redact their debug representation.
- `runner` provisions from the official Actions runner release, delegates service installation to the release's own helper, owns lifecycle operations, and applies desired-count scale plans through one shared executor used by both CLI and TUI.
- `reconcile` merges state by repository and runner name. The scale planner is pure: it calculates additions or selects eligible managed idle runners for removal without performing I/O.
- `tui` renders the same observations as the CLI and invokes the shared runner lifecycle and scale application APIs. It does not implement a second infrastructure control path.

## Sources of truth

Remote files, processes and systemd describe local installation state. The GitHub Actions runner API describes registration, online status, busy state and labels. Talos does not treat its local inventory as proof that either source still exists. GitHub-only and local-only states stay visible. Repository identity is taken from the runner's `gitHubUrl` when present; Talos ownership metadata is a fallback and the Actions service `serverUrl` is not treated as the repository URL.

Ownership is local to each runner installation. Existing installations remain external unless a matching `.talos-managed` marker is present. `--adopt` only authorizes a service command to invoke the runner distribution's helper; it does not rewrite ownership or make the installation eligible for scale-down or Talos file deletion.

Fleets currently record host membership only. There is no cross-host placement or scheduling algorithm.

## Provisioning and rollback

Creation is sequential to keep state transitions reviewable:

1. Validate remote access, unprivileged account, non-interactive sudo, repository access and host architecture.
2. Select the matching Linux asset from GitHub's latest official `actions/runner` release and require its release API SHA-256 digest.
3. Create a per-runner directory and a shared version-keyed cache. Verify the archive before extraction.
4. Pass a one-hour registration token through SSH stdin to a fixed Python wrapper; the official `config.sh` receives it as its required `--token` argument.
5. Install and start the generated systemd service, then require both a local listener process and GitHub's online state.
6. On failure, check GitHub state, attempt service cleanup and unregister a partial registration. Remove the directory only if the Talos marker matches and GitHub confirms the registration is absent. Otherwise preserve it and report incomplete rollback.

The registration token is not stored in Talos config or included in local SSH arguments, command records, or traces. The official runner configuration process necessarily receives it as an argument on the remote host for the duration of setup.

## Known limits

- Repository-scoped GitHub API permissions are needed to enumerate and manage repo runners. Talos cannot infer GitHub-only registrations for repositories it has not been told about. Supplying `--repo OWNER/NAME` allows an orphaned GitHub-only registration to be removed explicitly.
- Remote discovery depends on Python 3 and searches `/opt`, `/home`, `/srv`, `/usr/local`, `/var/lib`, systemd unit paths, and live `Runner.Listener` executables. Additional arbitrary root filesystem traversal is intentionally avoided.
- Local runner version is shown only when configuration or Talos metadata exposes it. Talos does not execute unknown `Runner.Listener` binaries to query a version.
- `runner stop` and scale-down read GitHub's busy flag immediately before stopping the service. Scale-down then confirms the service is gone and re-reads the GitHub registration before unregistering. The GitHub API and service stop are separate systems, so a job could be assigned between the initial check and stop; Talos refuses known-busy or unknown-busy state but cannot make this distributed transition atomic.
- Doctor is diagnostic only. It does not fix state or change services.
- Live provisioning against a real host is intentionally outside the automated test suite; the repository includes a repeatable validation checklist for exercising discovery, provisioning, reconciliation, scaling and cleanup against a disposable test target.
