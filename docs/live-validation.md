# Live validation

The automated test suite covers parsing, reconciliation, planning, API behavior and safety checks without requiring a real GitHub Actions runner host. Before a release that changes discovery, provisioning, lifecycle or scaling, run this checklist against a disposable Linux host and a repository where the operator has runner-administration permission.

Use generic local configuration; do not commit real hostnames, usernames, SSH aliases or infrastructure addresses to the repository.

## Preconditions

- The target uses Linux, systemd and Python 3.
- OpenSSH access works from the workstation.
- `sudo -n true` succeeds on the target.
- `gh auth status --hostname github.com` succeeds locally.
- The test repository may safely receive temporary self-hosted runners.
- Any pre-existing runner on the target has been identified and must not be modified by cleanup.

Example shell variables:

```sh
export TALOS_TEST_HOST=builder-01
export TALOS_TEST_REPO=OWNER/REPOSITORY
```

## 1. Establish the baseline

```sh
talos inspect "$TALOS_TEST_HOST" --json
talos runner list --host "$TALOS_TEST_HOST" --repo "$TALOS_TEST_REPO" --json
talos doctor --host "$TALOS_TEST_HOST"
```

Record the existing runner names. Confirm that external runners are reported as external and that their repository scope is recovered from the runner configuration.

## 2. Preview scale-up

Choose a desired count larger than the current distinct runner count.

```sh
talos scale \
  --host "$TALOS_TEST_HOST" \
  --repo "$TALOS_TEST_REPO" \
  --count <DESIRED> \
  --dry-run
```

Confirm that the plan creates only the difference and does not propose removal of existing external runners.

## 3. Provision and reconcile

Apply the scale-up:

```sh
talos scale \
  --host "$TALOS_TEST_HOST" \
  --repo "$TALOS_TEST_REPO" \
  --count <DESIRED>
```

Then immediately re-run discovery and reconciliation:

```sh
talos runner list --host "$TALOS_TEST_HOST" --repo "$TALOS_TEST_REPO" --json
talos doctor --host "$TALOS_TEST_HOST"
```

For every newly created runner, verify all of the following:

- one local installation and one GitHub registration reconcile into a single runner record;
- the repository is the expected `OWNER/REPOSITORY`;
- ownership is `talos`;
- the systemd service is active and enabled;
- the local `Runner.Listener` process is running;
- GitHub reports the runner online;
- the runner has the expected Talos labels.

## 4. Exercise lifecycle operations

Select one Talos-created idle runner and run:

```sh
talos runner restart <RUNNER> --host "$TALOS_TEST_HOST"
talos runner logs <RUNNER> --host "$TALOS_TEST_HOST" --lines 50
```

If a disposable job is available, also confirm that stop/restart refuses a known-busy runner without `--force`.

## 5. Scale back to the baseline

Use the original desired count and inspect the dry-run first:

```sh
talos scale \
  --host "$TALOS_TEST_HOST" \
  --repo "$TALOS_TEST_REPO" \
  --count <BASELINE> \
  --dry-run
```

The removal plan must contain only known-idle, Talos-managed runners. Apply it only after verifying that no pre-existing external runner is selected.

```sh
talos scale \
  --host "$TALOS_TEST_HOST" \
  --repo "$TALOS_TEST_REPO" \
  --count <BASELINE> \
  --yes
```

## 6. Verify cleanup

```sh
talos runner list --host "$TALOS_TEST_HOST" --repo "$TALOS_TEST_REPO" --json
talos doctor --host "$TALOS_TEST_HOST"
```

Confirm that:

- all temporary Talos runner services are gone;
- their GitHub registrations are gone;
- their marked installation directories are gone;
- any pre-existing runner and service remain unchanged;
- no GitHub-only or local-only drift was introduced.

## 7. GitHub-only orphan cleanup

If a deliberately disposable GitHub registration exists without a local installation, verify explicit repository-scoped cleanup:

```sh
talos runner remove <ORPHAN_NAME> \
  --host "$TALOS_TEST_HOST" \
  --repo "$TALOS_TEST_REPO" \
  --unregister \
  --yes
```

Talos must re-read the GitHub busy state and refuse deletion when the state is busy or unknown unless the operator explicitly uses `--force`.
