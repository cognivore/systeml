{
  config,
  lib,
  pkgs,
  ...
}:

let
  inherit (lib)
    mkIf
    mkOption
    mkForce
    types
    ;

  isDarwin = pkgs.stdenv.hostPlatform.isDarwin;

  cfg = config.systeml;

  # The configHome path that home-manager's systemd.nix uses to lay out
  # generated units inside the home-files tree. We mirror the exact same
  # location so `home.activation.reloadSystemd` (when we override it) sees
  # the same `$newGenPath/home-files/.config/systemd/user` directory.
  configHome = lib.removePrefix config.home.homeDirectory config.xdg.configHome;
  unitsSubdir = "${configHome}/systemd/user";

  # The on-disk plist label and path. Kept in sync with the activation
  # script below; both must agree or bootout/bootstrap will leave stale
  # entries in launchd.
  plistLabel = "com.memorici.systeml";
  plistPath = "${config.home.homeDirectory}/Library/LaunchAgents/${plistLabel}.plist";
  logDir = "${config.home.homeDirectory}/Library/Logs";

  # Stable on-disk path for the systeml binary itself. The activation
  # script copies (not symlinks) ${cfg.package}/bin/systeml here every
  # switch.
  #
  # Why bother: macOS TCC keys grants by binary path + content. If the
  # plist points directly at /nix/store/<hash>/bin/systeml, every nix
  # update of systeml rotates <hash> and macOS treats the daemon as a
  # brand-new binary that's never been granted Full Disk Access /
  # Accessibility / etc. — re-prompting (or silently denying for
  # launchd-spawned processes that get no UI prompt). With a stable
  # location under ~/.local/state, the user grants TCC once and the
  # grant survives systeml upgrades.
  stableSystemlBin = "${config.home.homeDirectory}/.local/state/systeml/bin/systeml";
  stableSystemlDir = "${config.home.homeDirectory}/.local/state/systeml/bin";

  # Render the LaunchAgent plist for the systeml daemon itself.
  #
  # ProgramArguments[0] is the stable binary path, *not* /bin/sh wrapping
  # wait4path. We don't need wait4path because our binary lives under
  # the user's home, which is mounted by the time any LaunchAgent runs
  # at login. Side benefit: macOS Login Items / Background Items shows
  # the agent as "systeml" rather than "sh".
  daemonPlist = pkgs.writeText "${plistLabel}.plist" (
    lib.generators.toPlist { escape = true; } {
      Label = plistLabel;
      ProgramArguments = [
        stableSystemlBin
        "--foreground"
        "--log-level"
        cfg.logLevel
      ];
      KeepAlive = true;
      RunAtLoad = true;
      ProcessType = "Interactive";
      EnvironmentVariables = {
        PATH = "/usr/bin:/bin:/usr/sbin:/sbin";
      };
      StandardOutPath = "${logDir}/systeml.out.log";
      StandardErrorPath = "${logDir}/systeml.err.log";
    }
  );

  # The sd-switch replacement that targets `systemlctl`.
  sdSwitchShim = pkgs.callPackage ./sd-switch-shim.nix { systeml = cfg.package; };

in
{
  # ---------------------------------------------------------------------------
  # Options
  # ---------------------------------------------------------------------------
  options.systeml = {
    enable = mkOption {
      type = types.bool;
      default = isDarwin;
      defaultText = lib.literalExpression "pkgs.stdenv.hostPlatform.isDarwin";
      description = ''
        Whether to enable the SystemL home-manager compat overlay. When
        enabled on Darwin, this makes `systemd.user.services.*` and friends
        work by routing them through the SystemL user-mode daemon instead
        of the (non-existent) Linux systemd. On Linux this is a no-op and
        upstream `modules/systemd.nix` continues to handle activation.
      '';
    };

    package = mkOption {
      type = types.package;
      default = pkgs.systeml or (pkgs.callPackage ./package.nix { });
      defaultText = lib.literalExpression "pkgs.systeml";
      description = ''
        The SystemL package providing the `systeml` daemon and `systemlctl`
        CLI binaries. Override this to point at a local checkout or a
        different revision.
      '';
    };

    systemctlAlias = mkOption {
      type = types.bool;
      default = false;
      description = ''
        Install a `systemctl` symlink alongside `systemlctl` (and a
        matching `journalctl` is already provided by the systeml package).

        macOS has no native `systemctl`, so the name is free. Enable
        this if you want shell scripts and muscle memory that type
        `systemctl --user start foo` to work without aliasing. The
        actual binary is the same as `systemlctl`; clap reads
        `argv[0]` only for the program-name in help text — all
        subcommands behave identically either way.
      '';
    };

    logLevel = mkOption {
      type = types.str;
      default = "info";
      example = "debug";
      description = ''
        Log level passed to the `systeml` daemon via `--log-level`. Mirrors
        the standard tracing levels: `error`, `warn`, `info`, `debug`,
        `trace`.
      '';
    };
  };

  # ---------------------------------------------------------------------------
  # Config
  # ---------------------------------------------------------------------------
  #
  # Everything below is gated on (isDarwin && cfg.enable). On Linux the
  # module evaluates to an empty config, leaving upstream systemd.nix
  # untouched — that is the contract the README promises.
  config = mkIf (isDarwin && cfg.enable) {
    # The daemon itself, plus the CLI, on the user's PATH.
    home.packages =
      [ cfg.package ]
      # Optional `systemctl` symlink. Lives in its own tiny derivation
      # so it can be added/removed independently of cfg.package without
      # forcing a rebuild of the systeml binaries themselves.
      ++ lib.optional cfg.systemctlAlias (
        pkgs.runCommand "systemctl-alias-for-systemlctl" { } ''
          mkdir -p $out/bin
          ln -s ${cfg.package}/bin/systemlctl $out/bin/systemctl
        ''
      );

    # Force-enable upstream's systemd.user knob. By default it is gated on
    # `pkgs.stdenv.isLinux`, which means every option underneath it would
    # otherwise be inert on Darwin. mkForce wins over the upstream default.
    systemd.user.enable = mkForce true;

    # Upstream asserts `lib.platforms.linux` for the systemd module. That
    # assertion fires on Darwin even when the user has no services defined,
    # because the module is imported unconditionally. We replace the
    # assertions list with a passing one — only safe to do because we've
    # taken responsibility for the activation logic ourselves below.
    assertions = mkForce [
      {
        assertion = true;
        message = "systeml: overriding systemd platform assertion on Darwin";
      }
    ];

    # Replace upstream's `home.activation.reloadSystemd` (defined in
    # home-manager/modules/systemd.nix, lines 495–554). We keep the same
    # DAG entry name so any module that ordered itself relative to
    # `reloadSystemd` continues to work.
    home.activation.reloadSystemd = mkForce (
      lib.hm.dag.entryAfter [ "linkGeneration" ] ''
        # Disable errexit so a single failing unit doesn't abort the rest
        # of the activation script.
        set +e

        # 1. Ensure XDG_RUNTIME_DIR. macOS has no convention for this, so
        #    we synthesise one under $TMPDIR. The daemon and systemlctl
        #    both honour $XDG_RUNTIME_DIR for the bus socket location.
        if [[ -z "''${XDG_RUNTIME_DIR:-}" ]]; then
          export XDG_RUNTIME_DIR="''${TMPDIR:-/tmp}/systeml-$(id -u)"
        fi
        if [[ ! -d "$XDG_RUNTIME_DIR" ]]; then
          run mkdir -p "$XDG_RUNTIME_DIR"
          run chmod 0700 "$XDG_RUNTIME_DIR"
        fi

        # 1a. Ensure the stable-systeml-binary directory exists, and
        #     refresh the on-disk copy of the systeml binary if it has
        #     drifted from the nix-store source. install -Dm755 -T does
        #     an atomic copy. Skipping when content matches keeps mtime
        #     stable, which some macOS TCC heuristics care about.
        run mkdir -p ${lib.escapeShellArg stableSystemlDir}
        if ! cmp -s ${cfg.package}/bin/systeml ${lib.escapeShellArg stableSystemlBin}; then
          verboseEcho "Refreshing stable systeml binary at ${stableSystemlBin}"
          run install -Dm755 -T ${cfg.package}/bin/systeml ${lib.escapeShellArg stableSystemlBin}
        fi

        # 2. Ensure ~/Library/Logs and ~/Library/LaunchAgents exist.
        run mkdir -p ${lib.escapeShellArg logDir}
        run mkdir -p ${lib.escapeShellArg "${config.home.homeDirectory}/Library/LaunchAgents"}

        # 3. Install / refresh the systeml LaunchAgent. We follow the
        #    bootout-then-bootstrap pattern from
        #    home-manager/modules/launchd/default.nix so that an updated
        #    plist actually takes effect (launchd caches plists in memory).
        srcPlist=${daemonPlist}
        dstPlist=${lib.escapeShellArg plistPath}
        domain="gui/$UID"
        agentName=${lib.escapeShellArg plistLabel}

        if ! cmp -s "$srcPlist" "$dstPlist"; then
          if [[ -f "$dstPlist" ]]; then
            verboseEcho "Bootstrapping out previous systeml agent"
            bootout_output=$(/bin/launchctl bootout "$domain/$agentName" 2>&1) || {
              if [[ "$bootout_output" != *"No such process"* ]]; then
                warnEcho "launchctl bootout failed: $bootout_output"
              fi
            }
            sleep 1
          fi
          verboseEcho "Installing systeml agent plist to $dstPlist"
          run install -Dm444 -T "$srcPlist" "$dstPlist"
          bootstrap_output=$(/bin/launchctl bootstrap "$domain" "$dstPlist" 2>&1) || {
            warnEcho "launchctl bootstrap failed: $bootstrap_output"
          }
        else
          verboseEcho "systeml agent plist already up to date"
          # Even if the plist is unchanged, make sure it's loaded.
          /bin/launchctl print "$domain/$agentName" >/dev/null 2>&1 \
            || /bin/launchctl bootstrap "$domain" "$dstPlist" >/dev/null 2>&1 \
            || true
        fi

        # 4. Wait briefly for the daemon's bus socket to appear, then run
        #    daemon-reload. If the daemon never came up (e.g. crashing on
        #    a config error), we fall through to sd-switch which will
        #    surface its own errors.
        for _ in 1 2 3 4 5 6 7 8 9 10; do
          if ${cfg.package}/bin/systemlctl --user is-system-running >/dev/null 2>&1; then
            break
          fi
          sleep 0.2
        done

        # 5. Diff old vs new units and apply via sd-switch.
        if [[ -v oldGenPath ]]; then
          oldUnitsDir="$oldGenPath/home-files${unitsSubdir}"
          if [[ ! -e "$oldUnitsDir" ]]; then
            oldUnitsDir=
          fi
        else
          oldUnitsDir=
        fi

        newUnitsDir="$newGenPath/home-files${unitsSubdir}"
        if [[ ! -e "$newUnitsDir" ]]; then
          newUnitsDir=${pkgs.emptyDirectory}
        fi

        ${lib.getExe sdSwitchShim} \
          ''${DRY_RUN:+--dry-run} ''${VERBOSE_ARG:-} \
          ''${oldUnitsDir:+--old-units "$oldUnitsDir"} \
          --new-units "$newUnitsDir" || true

        unset newUnitsDir oldUnitsDir
        set -e
      ''
    );
  };
}
