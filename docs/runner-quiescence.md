# Runner quiescence (T04 spike)

This document records what an unprivileged BaukFS runner can guarantee about the lifetime of its command and that command's descendants, and when a workspace may be captured or sealed. The evidence comes from the isolated, non-published crate `crates/baukfs-runner-spike`. That crate is not part of the `baukfs` library API. It does no filesystem capture, checkpointing, confinement, or database work, and it never calls `Workspace::record_quiescence`.

Storage is out of scope. The accepted design (one database per workspace copy, opened with multiprocess WAL) is unchanged. Database write locks are not used as evidence that writers have stopped.

## Threat model

**Managed workload.** The command the runner starts and every process descended from it through `fork`/`exec` (including `posix_spawn` and `std::process::Command`). This is true even if a descendant:

- outlives its parent or every ancestor,
- closes or redirects stdout and stderr,
- changes process group (`setpgid`) or session (`setsid`), or double-forks to daemonize,
- ignores or handles `SIGTERM`,
- outlives the controller.

**Assumed cooperative, not adversarial.** The workload runs as the same user as the controller. No unprivileged mechanism here stops a deliberately hostile same-user process from leaving supervision. Examples:

- asking the systemd user manager, launchd, D-Bus activation, `at`, cron, or ssh to start a process that is not a descendant,
- moving itself to another cgroup the user can write to,
- calling `flock(LOCK_UN)` on the inherited token,
- writing to the controller's state directory.

These are recorded as escapes, not as guarantees.

**Out of scope.** Unrelated host processes that modify the working directory. BaukFS makes no claim to detect them.

**Insufficient signals.** The direct child exiting (`wait()` returning), stdout and stderr reaching EOF, and the database write lock becoming free all happen while a managed writer can still be alive. Fixtures demonstrate each of these.

## Two separate conditions

1. **Controller ownership.** At most one controller acts for a workspace.
2. **Workload quiescence.** No process of the managed workload remains.

Neither implies the other. A dead controller releases ownership while its workload keeps running. A successor controller can own the workspace while an abandoned workload is still alive. Admission requires both conditions, established independently. Neither one uses a lease timeout or a database lock.

## Controller ownership

**Mechanism.** An exclusive `flock` (Rust `File::try_lock`, stable since 1.89) on `<state-dir>/controller.lock`.

- The state directory is per workspace and controller-private. It is supplied as a path and must not be inside the agent-writable working directory. Workspace identifiers are never turned into paths.
- The descriptor is close-on-exec, which is the std default, so the workload never inherits ownership.
- The kernel releases the lock when the controller exits or is killed. No stale-lock cleanup or lease is needed.
- `flock` locks the inode, not the path. If the file is unlinked or replaced, a second controller can lock a fresh inode. `ControllerLock::verify` compares the held descriptor's `(dev, ino)` with the path's, and admission refuses with `Ownership(Lost)` if they differ.

Behaviour is the same on Linux and macOS. Both implement `flock` natively; this is not the `fcntl` record-lock emulation. NFS and other network filesystems are not supported for the state directory.

## Candidate supervision mechanisms

| Mechanism | Tracks | Escapes | Controller death | Cleanup / quiescence | Host requirements |
| --- | --- | --- | --- | --- | --- |
| Direct child `waitpid` | Root process only | Every descendant | Root reparented; status lost | Neither. Necessary, not sufficient | None |
| Stdout/stderr EOF | Processes holding the pipe write end | Redirecting or closing stdio | Pipes lost | Neither (fixture: closed stdio) | None |
| Process group (`process_group(0)`, `kill(-pgid, 0)`, `killpg`) | Members of the root's group, including zombies | `setsid`, `setpgid` (fixture: detached) | Group survives; no record of who left | Signals reach members only. Empty-group probe is stable but incomplete | None |
| **Workload token** (inherited exclusive `flock` description) | Every process still holding the inherited descriptor | Closing inherited fds (`close_range`, daemon idioms), `flock(LOCK_UN)` | **Survives**: a new controller can tell whether any holder is alive | Detects liveness only; cannot list or signal holders | None, Linux and macOS |
| **Child subreaper** (`PR_SET_CHILD_SUBREAPER`, Linux) | Every descendant, including ones in another session or group or with closed fds; orphans reparent to the controller | Only non-descendants (service-manager spawns) | Lost: orphans reparent to the next subreaper or init | **Yes** while the controller lives. `waitpid(-1)` returning `ECHILD` is authoritative and stable. Descendants are found by `/proc` scan for signalling | Linux ≥ 3.4 |
| **cgroup v2 leaf** (Linux, delegated) | Every process in the subtree, inherited across fork regardless of session or fds | Moving to another writable cgroup; non-descendant spawns | **Survives**: `cgroup.events populated` is readable by any later invocation | **Yes**: `populated 0` is exact; `cgroup.kill` kills the subtree atomically | cgroup v2, kernel ≥ 5.14 for `cgroup.kill`, a cgroup delegated to the user (e.g. `systemd-run --user --scope -p Delegate=yes`) |
| PID + user namespace (Linux) | Everything in the namespace; init death kills all | None by fork | Init death kills all (with `PR_SET_PDEATHSIG` on the init) | Yes | Unprivileged user namespaces enabled (disabled or AppArmor-restricted on some distributions). Not implemented: container-style isolation is excluded from the MVP |
| `PR_SET_PDEATHSIG` (Linux) | Direct child only | Any descendant; child can reset it | Kills the direct child | No | None. Not implemented |
| macOS `kqueue` `EVFILT_PROC` | Specific known pids (`NOTE_EXIT`) | Unknown pids; `NOTE_TRACK` is unsupported on current macOS | Lost | Exit notification for already-known pids only | None. Not implemented |
| macOS ppid enumeration (`proc_listchildpids`, `sysctl KERN_PROC`) | Live descendants whose ancestry is intact | Orphans: reparented to launchd, lineage lost | Lost | Incomplete | None. Not implemented |

On macOS there is no unprivileged equivalent of a subreaper or cgroup. No investigated mechanism tracks a descendant that both leaves the process group and closes its inherited descriptors.

## Fixtures

The spike has three parts:

- `spike-controller`: a line-oriented controller process. It acquires ownership, classifies prior runs, becomes a subreaper on Linux, spawns the workload, and answers `spawn`, `wait-root`, `observe`, `settle`, `admit`, `terminate`, `diagnostics`, `reclassify`, and `quit`.
- `spike-fixture`: a workload process. It connects to a Unix socket owned by the test, announces `pid/ppid/pgid/sid`, and acknowledges each command: `write`, `spawn`, `setsid`, `close-stdio`, `close-fds`, `ignore-term`, `exit`.
- Integration tests: they drive both over handshakes. No sleep is used to decide whether a writer exists. Bounded waits re-check OS state (`waitpid`, `kill(-pgid, 0)`, token probe, `cgroup.events`) until a deadline. Writer liveness is shown by the writer acknowledging a `write` after the event under test.

Each test uses its own scratch tree: a working directory, a separate state directory, and the socket. On drop, every fixture is told to exit, and any whose socket stays open is sent `SIGKILL`. A fixture also exits when its control socket closes. After 30 consecutive full runs, no fixture or controller processes, scratch directories, or systemd scopes remained.

A fixture's socket EOF is not proof that its other descriptors are released, because the kernel releases files in deferred `fput` work during exit. Only a reaped (or zombie) process has released its token lock and left its cgroup. Supervised checks therefore use `wait-root` or `settle`, and recovery checks re-poll `reclassify` within a bound.

### Results by platform

Legend: **pass** = executed and passed. **cfg-checked** = compiled and Clippy-clean for the target, not executed. **not run** = not exercised.

| Scenario (test) | x86_64-linux | aarch64-linux | aarch64-darwin |
| --- | --- | --- | --- |
| Normal exit admitted after root reaped (`normal_exit_is_admitted_after_root_is_reaped`) | pass | not run | cfg-checked |
| Failing command admitted once quiescent (`failing_command_is_admitted_once_quiescent`) | pass | not run | cfg-checked |
| Child writes after parent exit; admission refused until it exits (`child_writing_after_parent_exit_blocks_admission`) | pass | not run | cfg-checked |
| Grandchild outlives ancestors; reparented to subreaper controller (`grandchild_outliving_ancestors_blocks_admission`) | pass | not run | cfg-checked |
| Descendant closes stdio, pipes reach EOF, keeps writing (`descendant_with_closed_stdio_keeps_writing_and_blocks_admission`) | pass | not run | cfg-checked |
| Descendant `setsid`: escapes process group; then `close-fds` escapes token (`detached_descendant_escapes_process_group_and_token`) | pass (subreaper still refuses) | not run | cfg-checked (asserts the false admission described below) |
| Descendant ignores `SIGTERM`; escalation to `SIGKILL` (`termination_escalates_for_descendant_ignoring_sigterm`) | pass | not run | cfg-checked |
| Termination reaches detached descendant (`termination_reach_for_detached_descendant`) | pass (individual `SIGTERM`, no escalation) | not run | cfg-checked (asserts it is not reached) |
| Ownership exclusive per workspace, independent across workspaces, released on controller death (`controller_ownership_is_exclusive_per_workspace_and_released_on_death`) | pass | not run | cfg-checked |
| Replaced lock file revokes ownership for admission (`replaced_lock_file_revokes_ownership_for_admission`, unit `replaced_lock_file_invalidates_ownership`) | pass | not run | cfg-checked |
| Clean prior run classified `no-workload` (`clean_prior_run_is_classified_as_no_workload`) | pass | not run | cfg-checked |
| Controller killed with live descendants: `live-abandoned`, then `unresolved` after they exit (`controller_death_leaves_live_abandoned_workload_then_unresolved`) | pass | not run | cfg-checked |
| Controller killed with fd-closing writer: `unresolved` while the writer is still live (`controller_death_with_fd_closing_writer_is_unresolved_not_quiescent`) | pass | not run | cfg-checked |
| cgroup tracks detached, fd-closing writer across controller death; `no-workload(cgroup-empty)` after exit (`cgroup_tracks_escaped_writer_across_controller_death`, ignored by default) | pass (systemd user scope, `Delegate=yes`) | not run | n/a |
| `cgroup.kill` reaches detached, fd-closing, `SIGTERM`-ignoring descendant (`cgroup_kill_reaches_escaped_descendant_ignoring_sigterm`, ignored by default) | pass (same) | not run | n/a |
| Admission unit tests: foreign workspace, unknown, active, second lock (`admission::tests`) | pass | not run | cfg-checked |

The x86_64-linux host was NixOS, kernel 6.18.37, cgroup v2, with a systemd user manager. The default test suite also runs inside the Nix `test` check sandbox. The cgroup tests are `#[ignore]` there because the sandbox has no user manager.

### Observed limitations

- **Process group.** After `setsid`, `kill(-pgid, 0)` reports the group empty while the detached descendant is alive and writing. Group signals do not reach it.
- **Workload token.** After `close-fds`, the token probe reports free while the descendant is alive and writing.
- **macOS narrower contract.** In the same scenario (detached, then `close-fds`), the macOS condition admits capture while a managed writer lives. The test asserts this so the gap stays visible. It has not been executed on macOS.
- **Subreaper.** Tracking ends when the controller dies. Orphans then reparent to the next subreaper (on this host, the systemd user manager) or to init.
- **`/proc` signalling.** Individual signals go to pids discovered by a non-atomic `/proc` scan. A descendant that is not a direct child could exit and have its pid reused between scan and signal. Direct children cannot (unreaped zombies pin their pids), and `cgroup.kill` has no such window. pidfds (`pidfd_open`, Linux ≥ 5.3) would close the gap.
- **Zombies.** A zombie still counts as a process-group member. That makes the result conservative (refuse), never permissive.

## Checkpoint-admission contract

Capture of workspace W may be admitted only if all of the following hold at the moment of the decision:

1. **Ownership.** The caller holds W's controller lock, and `verify()` confirms that the lock path still names the locked inode.
2. **No unresolved prior run.** Recovery classification at controller start returned `NoWorkload`. `LiveAbandoned` refuses as active. `Unresolved` refuses as unknown.
3. **Workload quiescence** for the current run, if any, by the platform's supported condition:
   - **Linux (subreaper, MVP):** the root has been reaped, `waitpid(-1, WNOHANG)` reports `ECHILD` (no children at all), the root's process group is empty, and the workload token is free. With a cgroup: additionally `cgroup.events` reports `populated 0`.
   - **macOS (narrower, unverified):** the root has been reaped, the process group is empty, and the workload token is free. **Assumption, not proven:** no managed descendant both left the process group and closed its inherited descriptors. If this assumption is not acceptable, macOS admission must refuse until an operator or the workload declares completion.
4. **Probes succeeded.** Any probe error makes the state `Unknown`, and admission refuses.

These conditions are stable once true. A process with no managed ancestor cannot create managed processes. An emptied process group cannot be rejoined. An unheld token description cannot be reacquired. An empty cgroup is only repopulated by an explicit move. Capture can therefore follow admission without racing new managed writers.

The spike encodes this contract as `admission::admit(&ControllerLock, WorkloadState)`. `QuiescenceProof` can only be constructed inside the spike crate, by the supervisor or by recovery classification, and is bound to the workspace's state-directory `(dev, ino)`. Admission returns the `QuiescenceBasis` it relied on (`subreaper-empty`, `cgroup-empty`, `group-and-token-released`, `no-interrupted-run`). A future runner should call `Workspace::record_quiescence` only with an admitted basis, and should record which basis it was.

## Recovery after controller failure

The controller writes `<state-dir>/run.record` durably (written to a temporary file, synced, then renamed):

- `running` (plus `cgroup <path>` when a cgroup is used) **before** spawning,
- `quiescent` only after admission succeeds.

A future invocation, after acquiring ownership, classifies prior runs:

| Evidence | Classification | Admission |
| --- | --- | --- |
| Token held | `live-abandoned(token-held)` | refused (active) |
| Record names a cgroup and `populated 1` | `live-abandoned(cgroup-populated)` | refused (active) |
| Record names a cgroup that is `populated 0` or removed (the kernel only removes empty cgroups) | `no-workload(cgroup-empty)` | allowed |
| Record absent or `quiescent`, token free | `no-workload(no-interrupted-run)` | allowed |
| Record `running`, no cgroup, token free | `unresolved(interrupted-run-token-free)` | refused (unknown) |

`unresolved` is deliberate. The fixture `controller_death_with_fd_closing_writer_is_unresolved_not_quiescent` shows a live writer in exactly that state. Without a cgroup, nothing available to a new process can rule out an fd-closing descendant that was orphaned past the dead subreaper. Leaving `unresolved` needs an explicit, recorded operator decision, or a cgroup. That resolution path is a runner design item; it is not implemented here.

A live abandoned workload can be cleaned up by a future invocation as follows:

- **with a cgroup:** `cgroup.kill`, then wait for `populated 0`.
- **without a cgroup on Linux:** scan `/proc/*/fd` for the token inode to find holders. This is not implemented.
- **on macOS:** the holders cannot be enumerated with the investigated mechanisms.

## Termination

`Supervisor::terminate(grace, kill_timeout)`:

1. Sends `SIGTERM` to the process group and, on Linux, individually to every live descendant from the `/proc` scan and every cgroup member.
2. Re-checks quiescence until `grace` expires.
3. If still active, snapshots the survivors (Linux), then repeatedly sends `SIGKILL` to the group and individuals, plus `cgroup.kill`, until quiescent or `kill_timeout` expires.
4. Reports whether escalation was needed, which processes were individually signalled, which survived the grace period, and the final observation.

Diagnostic output is drained by background threads into bounded (64 KiB) buffers. Termination and waits never block on pipe EOF, because a descendant may keep a pipe open indefinitely.

Observed reach on Linux:

- Group `SIGTERM` reaches in-group members.
- Individual `SIGTERM` reaches detached descendants.
- `SIGKILL` escalation reaches a `SIGTERM`-ignoring descendant.
- `cgroup.kill` reaches a detached, fd-closing, `SIGTERM`-ignoring descendant.

On macOS, only the process group is reachable, so a detached descendant is not terminated. The test asserts `quiescent=no` with the token still held, which leads admission to refuse.

## Recommended implementations

**Linux (MVP, unprivileged, no host setup):**

- `flock` ownership lock in a controller-private state directory.
- Controller is a child subreaper, set before the first spawn.
- Workload is spawned in its own process group, with the workload token inherited.
- Quiescence by the subreaper condition above.
- Termination by group signal plus individual signals plus escalation.
- Durable run record.
- Controller death leads to `live-abandoned` or `unresolved`, never to `no-workload`.

**Linux (preferred when available):** add a delegated cgroup v2 leaf, joined in `pre_exec` before the workload runs any code. Detect delegation at runtime, for example by whether the controller's own cgroup is writable, or by launching the controller under `systemd-run --user --scope -p Delegate=yes`. This upgrades controller-death recovery to exact `no-workload` or `live-abandoned`, and termination to `cgroup.kill`. Fall back to the MVP when it is unavailable. Use `clone3(CLONE_INTO_CGROUP)` or keep the `pre_exec` write, and use pidfds for individual signals.

**macOS (narrower MVP):** `flock` ownership, process group, workload token, run record. State the narrower contract explicitly in user-facing behaviour. Controller death leads to `live-abandoned` (token held) or `unresolved` (token free after an interrupted run), never to `no-workload`. Detached descendants cannot be terminated, so a live token leads admission to refuse.

## Unresolved platform verification

- **aarch64-darwin:** not executed. The macOS cfg branches were type-checked and Clippy-checked by cross-compiling with `--target aarch64-apple-darwin` (std only, no linking). On a Mac, the following must be confirmed:
  - `flock` semantics for the token across `fork`/`exec`,
  - `kill(-pgid, 0)` behaviour with zombies,
  - the assertions in `detached_descendant_escapes_process_group_and_token` and `termination_reach_for_detached_descendant`,
  - Unix-socket path length in the Nix darwin sandbox.
- **aarch64-linux:** not executed. Nothing in the code is architecture-specific, but it has not been run.
- **cgroup path:** exercised only on a systemd host with user delegation. Hosts without systemd, or with cgroup v1 or hybrid hierarchies, were not tested. `create` returns `NotDelegated` or `NotUnified` there.

## Changes to the future runner design

- The runner needs a controller-private, per-workspace state directory outside the working directory, holding the ownership lock, workload token, and run record. Mapping a workspace to it must not derive paths from workspace identifiers.
- The controller is a long-lived process for the duration of a run: the subreaper must be the process that outlives the workload. On Linux, one-shot CLI invocations that spawn and exit cannot provide the MVP guarantee.
- `record_quiescence` must be driven by the admission result (with its basis), not by `wait()`. The run record's `running` marker must be written before spawning.
- A recovered `unresolved` state blocks capture and sealing until an explicit, recorded resolution. That needs a user-facing command and a record of who resolved it.
- Diagnostics must be drained continuously and bounded; never join on pipe EOF.

## Reproduction

```sh
nix develop -c cargo test -p baukfs-runner-spike
# Linux with a systemd user manager and cgroup v2 delegation:
nix develop -c cargo test -p baukfs-runner-spike --test scenarios -- --ignored
nix develop -c cargo clippy -p baukfs-runner-spike --all-targets -- --deny warnings
nix build -L .#checks.$(nix config show system).test
```

Repeated-run stress test (30 runs of all scenarios, including ignored ones):

```sh
nix develop -c bash -c 'for i in $(seq 30); do cargo test -q -p baukfs-runner-spike --test scenarios -- --include-ignored || exit 1; done'
```
