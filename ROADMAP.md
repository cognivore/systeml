# SystemL roadmap

Living document of what works, what's stubbed, and what's deferred.
The bulk of phase 1, 2 and 5 work landed in the v0.1 round — see git
history. This file tracks what happens next.

The phase numbering in this file matches the one in `README.md`'s status
table.

## Phase 1 — `.service` + dep engine + 12-cmd CLI ✅

**Done.**

- `[Unit]` parsed: `Description`, `Documentation`, `Wants`, `Requires`,
  `Requisite`, `BindsTo`, `PartOf`, `Upholds`, `After`, `Before`,
  `Conflicts`, `OnFailure`, `OnSuccess`, `PropagatesReloadTo`,
  `PropagatesStopTo`, `JoinsNamespaceOf`, `DefaultDependencies`,
  `StopWhenUnneeded`, `RefuseManualStart`/`Stop`, `AllowIsolate`,
  `JobTimeoutSec`, `JobRunningTimeoutSec`, `OnFailureJobMode`,
  `IgnoreOnIsolate`, `CollectMode`, `FailureAction`, `SuccessAction`,
  `RebootArgument`, `SourcePath`, every `Condition*=`, every `Assert*=`.
- `[Install]` parsed: `WantedBy`, `RequiredBy`, `UpheldBy`, `Also`,
  `Alias`, `DefaultInstance`. Symlink semantics implemented in
  `systeml-runtime::install` — `enable`/`disable`/`mask`/`unmask` create
  the right links under `$XDG_CONFIG_HOME/systemd/user/`.
- `[Service]` parsed: `Type` (`simple`/`exec`/`oneshot`/`forking`/`notify`/
  `notify-reload`/`dbus`/`idle`), every `Exec*=`, `Restart`/`RestartSec`/
  `RestartSteps`/`RestartMaxDelaySec`, `Timeout*Sec`, `RuntimeMaxSec`,
  `WatchdogSec`, `StartLimit*`, `Environment`/`EnvironmentFile`/
  `PassEnvironment`/`UnsetEnvironment`, `WorkingDirectory`/`RootDirectory`/
  `User`/`Group`/`SupplementaryGroups`/`UMask`/`Nice`,
  `Standard{Input,Output,Error}`, `KillMode`/`KillSignal`/family,
  `SuccessExitStatus`/`RestartPreventExitStatus`/`RestartForceExitStatus`,
  every `Limit*=`, `Sockets`, `FileDescriptorStoreMax`.
- Dependency engine: full transaction expansion (start/stop/restart/reload
  with reverse propagation through `BindsTo`/`PartOf`/`PropagatesStopTo`),
  topological sort with cycle detection, all six `JobMode`s
  (`Replace`/`Fail`/`Isolate`/`Flush`/`IgnoreDependencies`/
  `IgnoreRequirements`).
- Service supervisor: `Type=simple/exec/oneshot/forking/notify/
  notify-reload/idle` (and `dbus` aliased to `simple`), `sd_notify`
  protocol parser (`READY`/`STOPPING`/`RELOADING`/`WATCHDOG`/`BARRIER`/
  `MAINPID`/`STATUS`), `Restart=` policies with exponential backoff,
  `KillMode=` via `setsid` + `killpg`, full stdio sink set.
- D-Bus surface: full `org.freedesktop.systemd1.Manager` interface with
  20+ methods, per-unit `Unit` and `Service` interfaces, signal bridge
  for `UnitNew`/`UnitRemoved`/`JobNew`/`JobRemoved`. Object-path escape
  matches systemd byte-for-byte.
- CLI: 17 subcommands — `start`/`stop`/`restart`/`reload`/`enable`/
  `disable`/`mask`/`unmask`/`is-active`/`is-enabled`/`status`/`cat`/
  `show`/`list-units`/`list-unit-files`/`daemon-reload`/
  `is-system-running`. Exit codes match upstream (3 inactive, 1
  disabled, 4 not-found).

**Phase-1 gaps** (filed for later passes — the contracts work, the
implementations are partial):

- `Manager::start_unit` returns `JobId`, but the bus emits an
  ObjectPath keyed off the *unit*, not a per-job
  `/org/freedesktop/systemd1/job/<id>` object. Per-job objects don't
  exist yet. Tools that follow `JobNew(path)`→
  `Properties.Get(path, "JobType")` won't find anything.
- `KillUnit`, `ResetFailedUnit`, `ResetFailed`, `GetUnitProcesses` are
  stubbed in the bus and lack Manager methods.
- `PropertiesChanged` on `ActiveState`/`SubState` transitions is plumbed
  through `Manager::events` but doesn't reach the per-unit object's
  signal — the bridge hasn't been wired through.
- `Type=dbus` warns and aliases to `simple`. Real `BusName=` watching is
  Phase-3.
- Linux abstract namespace `@foo` sockets warn and fall back to
  `/tmp/foo`.
- `WatchdogSec=` runtime monitoring loop: the initial `READY=1` works,
  but the subsequent watchdog-keepalive loop (kill-on-skip) is Phase-3.
- Persistent timer fire-on-startup-after-missed: `read_last_fire` /
  `write_last_fire` exist; the daemon-side wiring to actually replay
  missed runs at boot is Phase-3.
- Linux-only resource control (`MemoryMax`, `CPUQuota`, every cgroup
  directive) is parsed and warned; they won't be enforced. Phase-4 may
  approximate some via `sandbox-exec`.
- `[Install]` `Also=` cascade on enable/disable is partial — we follow
  one hop, not the transitive closure.

## Phase 2 — `.timer` / `.path` / `.socket` / `.target` ✅

**Done.**

- `.timer`: `OnCalendar` (full systemd-time syntax — wildcards, lists,
  ranges, steps, weekday prefixes, shortcuts), `OnBootSec`,
  `OnStartupSec`, `OnUnitActiveSec`, `OnUnitInactiveSec`, `OnActiveSec`,
  `Persistent` (helpers exist), `AccuracySec`, `RandomizedDelaySec`,
  `Unit=`. `next_fire` algorithm walks minute-by-minute with month-skip.
- `.path`: kqueue-based watcher. `PathExists`, `PathExistsGlob`,
  `PathChanged`, `PathModified`, `DirectoryNotEmpty`, `MakeDirectory`,
  `DirectoryMode`, `TriggerLimitIntervalSec`/`TriggerLimitBurst`.
- `.socket`: `ListenStream`/`ListenDatagram`/`ListenSequentialPacket`/
  `ListenFIFO`/`ListenSpecial`. TCP, IPv4 + IPv6, Unix path, Unix
  abstract (warns on macOS). `LISTEN_FDS` / `LISTEN_PID` /
  `LISTEN_FDNAMES` env-var protocol via `pre_exec` `dup2`.
  `MaxConnections`, `Backlog`, `KeepAlive`, `NoDelay`, `ReusePort`,
  `SocketMode`, `SocketUser`/`SocketGroup`, `RemoveOnStop`, `Symlinks`.
- `.target`: pure dependency aggregation. No execution path needed.

**Phase-2 gaps:**

- `Accept=yes` on a socket binds and accepts, but the per-connection
  template `foo@N.service` instantiation loop runs only at the listener
  level — the manager-side spawn-on-accept is wired only as a hook, not
  end-to-end.
- `OnClockChange=` / `OnTimezoneChange=` parse but aren't event-driven.
- `.scope` units are parsed and represented but registering an external
  PID into a scope is Phase-3.

## Phase 3 — templates, drop-ins, specifiers, conditions

**Deferred.** Not started in v0.1.

- Template instantiation: `foo@bar.service` falling back to
  `foo@.service` works at *load* time but `%i`/`%n`/`%u`/`%h`/`%t`/etc.
  specifier expansion in directive values is not done. ExecStart=
  `/bin/echo %i` will pass `%i` literally today.
- Drop-ins (`foo.service.d/*.conf`): basic stacking works (the loader
  re-renders the merged INI). What's missing: drop-in *specifier
  expansion*, type-wide drop-ins (`service.d/`), and the multi-search-
  path drop-in walk.
- Condition *evaluation*: every `Condition*=` and `Assert*=` parses, but
  the runtime doesn't actually evaluate them yet. A unit with
  `ConditionPathExists=/never/here` will start anyway.
- `Sockets=` reverse linkage from a service to its socket units (so the
  service's stdio attaches to the right fd).

## Phase 4 — sandbox-exec mappings + best-effort security

**Deferred.** Not started in v0.1.

The ~50 Linux-kernel-only directives (cgroups, namespaces, seccomp,
capabilities, MAC frameworks, `DynamicUser`, `PAM`, audit) are parsed
and warned today. Phase 4 picks the subset that has a meaningful macOS
analogue and best-efforts them via `sandbox-exec`:

- `ProtectSystem=`/`ProtectHome=` → sandbox-exec profile with
  filesystem deny rules.
- `ReadOnlyPaths=`/`ReadWritePaths=`/`InaccessiblePaths=` → fs profile.
- `PrivateTmp=` → unique `TMPDIR` per service + chroot-ish sandbox rule.
- `PrivateNetwork=` → sandbox-exec network deny (limited; macOS sandbox
  doesn't have proper network namespaces).
- `LimitNOFILE=` & friends → already work (POSIX `setrlimit`).
- Everything else stays parsed-and-warned.

There is no path to honoring `SystemCallFilter=`, capability bounding,
namespaces, or DynamicUser on macOS. These will keep warning forever.

## Phase 5 — home-manager compat overlay ✅

**Done.**

- `nix/home-manager-module.nix` exposes `systeml.{enable,package,
  logLevel}`. On Darwin: `mkForce`s `systemd.user.enable = true`,
  neutralises the upstream `assertPlatform "systemd" linux` assertion,
  replaces `home.activation.reloadSystemd` with a hook that bootstraps
  `~/Library/LaunchAgents/com.memorici.systeml.plist` and runs
  `nix/sd-switch-shim.nix`.
- The shim reads old/new unit dirs, parses `Unit.X-SwitchMethod` per
  unit, and dispatches `systemlctl` start/stop/restart/reload calls.
- `nix/package.nix` builds `systeml` and `systemlctl` via
  `rustPlatform.buildRustPackage`.
- Inert on Linux — every config block is `mkIf isDarwin`.

**Phase-5 gaps:**

- The plist's `wait4path` references the package's `/bin/systeml` —
  this assumes it's a static path under the Nix store. Verify on first
  end-to-end install with a real home-manager generation.
- The activation hook hasn't been exercised against a real home-manager
  switch yet (would need a home-manager flake input). End-to-end test
  is in the todo backlog.

## Cross-cutting backlog

- **Persistent state**: `$XDG_STATE_HOME/systeml/timers/<name>.timer`
  for `Persistent=yes` is written but not yet *read* on startup. Result:
  a missed daily timer doesn't replay across daemon restarts.
- **Journaling**: stdout/stderr go to
  `$XDG_STATE_HOME/systeml/journal/<unit>.{out,err}.log` for `Standard*=
  journal` and unsupported sinks. There is no `journalctl` equivalent
  yet — `systemlctl status` just shows the last-line preview path. A
  `systemlctl logs` subcommand would be a small lift.
- **Cargo lockfile**: not committed. `nix/package.nix` reads
  `cargoLock.lockFile = ../Cargo.lock` so a build will create one on
  first invocation, but a CI build needs it pinned. Add it in the next
  commit pass.
- **Integration test harness**: end-to-end (boot daemon, run
  `systemlctl`, assert state) was validated manually but not codified.
  A `tests/e2e/` crate that spawns the binaries against a tempdir
  `XDG_RUNTIME_DIR` would lock this in.
- **D-Bus introspection XML**: zbus generates this, but we haven't
  cross-checked the output against upstream's
  `org.freedesktop.systemd1.{Manager,Unit,Service}.xml`. Some method
  signatures may drift.

## Out of scope

The following will not be implemented as part of SystemL — it would
require a kernel macOS doesn't have:

- `.mount` / `.automount` / `.swap` / `.device` / `.slice` units
  (parsed and represented, never activated).
- Cgroups in any form.
- Linux namespaces (mount/pid/net/ipc/user).
- Seccomp.
- `DynamicUser=`, `PAMName=`, `SELinuxContext=`, `AppArmorProfile=`,
  `SmackProcessLabel=`.
- PID-1 init duties (`systemctl reboot`, `systemctl poweroff`,
  generators that read `/etc/fstab`).
