# SystemL

A user-mode, systemd-compatible service manager for macOS.

SystemL lets you write `systemd.user.services.foo = { ... }` in
[home-manager] on Darwin and have it Just Work. It runs as a single
`LaunchAgent`, supervises your services directly, and exposes the
standard `org.freedesktop.systemd1` D-Bus interface so upstream
`systemctl --user` and the rest of the ecosystem connect without
modification.

It is **not** a Linux init replacement — it doesn't try to be PID 1,
doesn't manage cgroups, and doesn't do anything that requires a kernel
feature macOS doesn't have. What it does is the chunk of `systemd`
people actually use day-to-day for personal services: parse the unit
files, resolve dependencies, supervise processes, fire timers, watch
paths, hand off socket fds.

## Status

| Phase | Surface | Status |
| ----- | ------- | ------ |
| 1 | `.service` (simple/oneshot/forking/notify) + dep engine + `[Install]` symlinks + 17-cmd `systemlctl` | ✅ landed in v0.1 |
| 2 | `.timer` / `.path` / `.socket` / `.target` activation | ✅ landed in v0.1 |
| 3 | template units, drop-ins, specifiers (`%n`/`%i`/…), condition evaluation | deferred |
| 4 | `sandbox-exec` mappings for `Protect*` / `ReadOnly*` / `PrivateTmp` | deferred |
| 5 | home-manager compat overlay (Nix flake) | ✅ landed in v0.1 |

See [`ROADMAP.md`](ROADMAP.md) for the precise list of what's
implemented, what's stubbed, and what's deferred.

Linux-kernel-only directives (`CPUQuota`, `MemoryLimit`, `Private*`,
`SystemCallFilter`, `DynamicUser`, `PAMName`, MAC frameworks, …) are
parsed and ignored with structured warnings — the unit still loads.

## Architecture

```
            ┌────────────────────────────────────────────────┐
upstream ──▶│ systemctl --user / loginctl / systemd-run / …  │
tools       └────────────────────────────────────────────────┘
                              │  D-Bus (peer-to-peer over unix socket)
                              ▼
        $XDG_RUNTIME_DIR/systeml/private  (or /private/tmp/systeml-$UID/…)
                              │
            ┌─────────────────┴─────────────────┐
            │   systeml daemon (LaunchAgent)    │
            │ ┌───────────────────────────────┐ │
            │ │ org.freedesktop.systemd1.*    │ │  ── systeml-bus
            │ │ Manager / Unit / Service / …  │ │
            │ └───────────────────────────────┘ │
            │ ┌───────────────────────────────┐ │
            │ │ Manager (registry + events)   │ │  ── systeml-runtime
            │ │ ┌─────────────────────────┐   │ │
            │ │ │ transaction engine       ──┼─┼─── systeml-deps
            │ │ │  jobs / topo / modes     │ │ │
            │ │ └─────────────────────────┘   │ │
            │ │ ┌─────┐┌─────┐┌─────┐┌─────┐ │ │
            │ │ │ svc ││timer││ path││ sock│ │ │
            │ │ │ run ││sched││kqueue│ FDs │ │ │
            │ │ └─────┘└─────┘└─────┘└─────┘ │ │
            │ └───────────────────────────────┘ │
            │                                   │
            │ unit AST + INI parser  ──── systeml-unit
            └───────────────────────────────────┘
                              │ fork / exec / setsid / killpg
                              ▼
                       child services
```

The crate split exists so each piece is testable in isolation:

- **`systeml-unit`** is pure parsing and types — no I/O beyond reading
  unit files. The other crates consume its AST.
- **`systeml-deps`** is graph theory and job-set algebra. It takes a
  snapshot of loaded units and a `ManagerView` trait impl, and returns
  a sorted batch of jobs to run.
- **`systeml-runtime`** holds the live state: the `Manager` registry,
  the per-service supervisors, the kqueue watchers, the calendar
  scheduler, the socket binder.
- **`systeml-bus`** is a thin shim that exposes the runtime's `Manager`
  on a private D-Bus, using zbus's peer-to-peer mode (no
  `dbus-daemon` dependency on macOS).
- **`systeml`** (the daemon) wires the four together and handles
  signals.
- **`systemlctl`** is a CLI client that speaks the same D-Bus surface.

The daemon is itself launched by **launchd** as a `LaunchAgent`, so it
survives login, restarts on crash, and follows macOS's normal user-
session lifecycle. From there it does what `systemd --user` does on
Linux: directly fork/exec child services, hold their fds, watch their
exit, restart per `Restart=` policy.

We do **not** translate each unit into its own `.plist`. That's what
[wayfinder]'s launchd adapter does, and it caps out at a small subset
of systemd. SystemL needs the full dependency engine, socket
activation, `Type=notify`, and so on, so the daemon supervises
everything itself.

## Running on macOS

### Build

```sh
nix develop
cargo build --release --workspace
```

This produces:

- `target/release/systeml` — the daemon
- `target/release/systemlctl` — the CLI

You can also `nix build .#systeml` to get a Nix-store build.

### Standalone (without home-manager)

If you just want to try it:

```sh
# 1. Pick a runtime dir. macOS has no XDG runtime dir by convention,
#    so SystemL picks $TMPDIR/systeml-$UID if XDG_RUNTIME_DIR is unset.
export XDG_RUNTIME_DIR="$TMPDIR/systeml-$UID"
mkdir -p "$XDG_RUNTIME_DIR" && chmod 700 "$XDG_RUNTIME_DIR"

# 2. Drop a .service somewhere SystemL will find it.
mkdir -p ~/.config/systemd/user
cat > ~/.config/systemd/user/hello.service <<EOF
[Unit]
Description=Hello, SystemL

[Service]
Type=oneshot
ExecStart=/bin/echo hello world
RemainAfterExit=yes

[Install]
WantedBy=default.target
EOF

# 3. Start the daemon (in another terminal — it runs in foreground).
./target/release/systeml --foreground

# 4. Drive it.
./target/release/systemlctl is-system-running       # → "running"
./target/release/systemlctl list-units              # tabular view
./target/release/systemlctl enable hello.service    # creates symlink
./target/release/systemlctl start hello.service     # runs ExecStart
./target/release/systemlctl status hello.service    # full status block
./target/release/systemlctl is-active hello.service # exit 0 if active
```

Search paths follow XDG semantics on every platform — yes, even on
macOS we read `~/.config/systemd/user/`, not `~/Library/Application
Support/`. That's intentional: existing home-manager configs target
`~/.config` regardless of platform.

### With home-manager

The intended use-case. Add SystemL as a flake input and import the
provided home-manager module:

```nix
# flake.nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    home-manager = {
      url = "github:nix-community/home-manager";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    systeml = {
      url = "github:cognivore/systeml";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { home-manager, systeml, ... }: {
    homeConfigurations.you = home-manager.lib.homeManagerConfiguration {
      modules = [
        systeml.homeManagerModules.default
        {
          systeml.enable = true;            # default-on for Darwin
          systeml.logLevel = "info";        # tracing filter

          # Now this works on Darwin, exactly as on Linux:
          systemd.user.services.my-thing = {
            Unit.Description = "My background thing";
            Service.ExecStart = "${pkgs.my-thing}/bin/my-thing";
            Service.Restart = "on-failure";
            Install.WantedBy = [ "default.target" ];
          };
        }
      ];
    };
  };
}
```

`home-manager switch` then:

1. Generates `~/.config/systemd/user/my-thing.service` (this part is
   unchanged from upstream).
2. Installs `systeml` as a `LaunchAgent` at
   `~/Library/LaunchAgents/com.memorici.systeml.plist` if it isn't
   there already.
3. `launchctl bootstrap`s the daemon.
4. Runs the SystemL `sd-switch` shim against the old/new unit
   directories, which calls `systemlctl daemon-reload` then
   `start`/`stop`/`restart`/`reload` per unit based on
   `Unit.X-SwitchMethod`.

Inert on Linux — every Darwin-conditional config is wrapped in
`lib.mkIf isDarwin`. The upstream home-manager systemd path keeps
working unchanged on real Linux.

### Inspecting state

```sh
# Running services
systemlctl list-units

# What's installed on disk?
systemlctl list-unit-files

# Why did this thing fail?
systemlctl status some.service
systemlctl cat some.service

# Force a re-scan (after editing a unit file by hand)
systemlctl daemon-reload
```

Logs land at `$XDG_STATE_HOME/systeml/journal/<unit>.{out,err}.log` for
units that use `StandardOutput=journal` or one of the unsupported
sinks. The daemon's own logs go to `~/Library/Logs/systeml.{out,err}.log`
when run via launchd, or to stderr when run with `--foreground`.

## What works today

- Every common `[Unit]` / `[Install]` / `[Service]` directive.
- `Type=` `simple`, `exec`, `oneshot`, `forking`, `notify`,
  `notify-reload`, `idle` (`dbus` aliased to `simple`).
- Dependency expansion across `Wants`, `Requires`, `BindsTo`, `PartOf`,
  `Conflicts`, `After`, `Before`, `OnFailure`, `OnSuccess`.
- All six `JobMode`s (`replace`, `fail`, `isolate`, `flush`,
  `ignore-dependencies`, `ignore-requirements`).
- `Restart=` with `RestartSec`, `RestartSteps`, `RestartMaxDelaySec`
  exponential backoff.
- `KillMode=` `process` / `control-group` / `mixed` via `setsid` +
  `killpg`.
- `Environment=`, `EnvironmentFile=` (with `-` optional), `${VAR}`
  expansion, `PassEnvironment=`, `UnsetEnvironment=`.
- `User=`/`Group=`/`SupplementaryGroups=`/`UMask=`/`Nice=`.
- Stdio sinks: `inherit`, `null`, `tty`, `file:`, `append:`,
  `truncate:`, `socket`, `fd:NAME`. `journal*`/`kmsg`/`syslog` route to
  a flat per-unit file under the state dir.
- Calendar timers (full grammar) plus monotonic triggers.
- Path watching via kqueue.
- Socket activation with `LISTEN_FDS` / `LISTEN_PID` / `LISTEN_FDNAMES`
  fd-passing.
- `[Install]` symlinks under `$XDG_CONFIG_HOME/systemd/user/`.
- `systemlctl` subcommands: `start`, `stop`, `restart`, `reload`,
  `enable`, `disable`, `mask`, `unmask`, `is-active`, `is-enabled`,
  `status`, `cat`, `show`, `list-units`, `list-unit-files`,
  `daemon-reload`, `is-system-running`.

## What doesn't (and why)

- Anything cgroup-based (`MemoryLimit`, `CPUQuota`, `IOWeight`,
  `TasksMax`, slices). macOS has no cgroups.
- Anything namespace-based (`PrivateNetwork`, `PrivateTmp`,
  `ProtectSystem`, `ProtectHome`, `BindPaths`, `ReadOnlyPaths`).
  Phase 4 will best-effort some of these via `sandbox-exec`.
- Anything seccomp-based (`SystemCallFilter`,
  `RestrictAddressFamilies`, `MemoryDenyWriteExecute`).
- Linux capabilities (`CapabilityBoundingSet`, `AmbientCapabilities`).
- `DynamicUser=`, `PAMName=`, `SELinuxContext=`, `AppArmorProfile=`,
  `SmackProcessLabel=`. Macros / MAC frameworks that don't exist on
  macOS.
- `.mount` / `.automount` / `.swap` / `.device` / `.slice` units.
  Parsed for round-trip but never activated.
- Real `journalctl`. Logs are flat files for now; a `systemlctl logs`
  subcommand would close the gap.

All Linux-only directives are **parsed and warned**, never errored —
the unit still loads. This means existing home-manager configs that
sprinkle `PrivateTmp = true` everywhere keep working; they just don't
get the sandboxing they would on Linux.

See [`ROADMAP.md`](ROADMAP.md) for the per-phase backlog.

## Project layout

```
.
├── Cargo.toml              # workspace
├── flake.nix               # nix dev shell + package + home-manager module export
├── ROADMAP.md
├── CLAUDE.md               # agent / contributor notes
├── crates/
│   ├── systeml-unit/       # parser + AST (45 tests)
│   ├── systeml-deps/       # transaction engine (30 tests)
│   ├── systeml-runtime/    # supervisor + activation (30 tests)
│   ├── systeml-bus/        # D-Bus surface (10 tests)
│   ├── systeml/            # daemon binary
│   └── systemlctl/         # CLI binary
└── nix/
    ├── package.nix         # rustPlatform.buildRustPackage
    ├── home-manager-module.nix   # the compat overlay
    └── sd-switch-shim.nix  # bash shim that mimics sd-switch
```

## Contributing

Workspace lints are strict: `unsafe_code = "deny"` everywhere except
inside `systeml-runtime` (where `fork`/`kqueue`/`dup2` need it,
contained behind `nix` crate wrappers); `clippy::all` and
`clippy::cargo` warn-level. Every commit must pass:

```sh
nix develop --command cargo build --workspace
nix develop --command cargo test --workspace
nix develop --command cargo clippy --workspace --all-targets -- -D warnings
```

Commit style is C4 — atomic, non-breaking, first line ≤ 72 chars,
Problem/Solution body, never amend (always create a new commit).
Linux-kernel-only directives are **parsed, warned, ignored** — never
erroring; they round-trip in `cat`/`show` output for fidelity.

[home-manager]: https://github.com/nix-community/home-manager
[wayfinder]: https://github.com/cognivore/grim-monolith/tree/main/crates/wayfinder
