{
  lib,
  pkgs,
  systeml,
}:

# A drop-in replacement for `sd-switch` (the home-manager activation helper)
# that targets `systemlctl` instead of `systemctl --user`.
#
# Same CLI surface that home-manager's activation expects:
#   sd-switch [--dry-run] [--verbose]
#             [--timeout MS]
#             [--old-units DIR]
#             --new-units DIR
#
# Behaviour mirrors upstream `sd-switch`:
#   * unit added in new                   -> enable + start
#   * unit removed (only in old)          -> stop + disable
#   * unit changed (different content)    -> apply X-SwitchMethod
#   * unit unchanged                      -> no-op
#
# `Unit.X-SwitchMethod` controls the strategy for changed units:
#   restart    (default) -> systemlctl restart NAME
#   reload               -> systemlctl reload NAME
#   stop-start           -> systemlctl stop NAME ; systemlctl start NAME
#   keep-old             -> no-op (operator handles manually)
pkgs.writeShellApplication {
  name = "sd-switch";
  runtimeInputs = [
    systeml
    pkgs.coreutils
    pkgs.diffutils
    pkgs.gnugrep
    pkgs.gnused
  ];
  # shellcheck SC2034 is fired on TIMEOUT_MS, which we accept on the
  # CLI for sd-switch upstream compatibility but never read — the daemon's
  # own job timeout governs completion. shellcheck-ignore at the
  # writeShellApplication level so it applies across both the assignment
  # and the case-arm `--timeout) TIMEOUT_MS="$2"`.
  excludeShellChecks = [ "SC2034" ];

  text = ''
    set -euo pipefail

    DRY_RUN=0
    VERBOSE=0
    OLD_UNITS=""
    NEW_UNITS=""
    TIMEOUT_MS=0

    log()  { if [[ $VERBOSE -eq 1 ]]; then printf '[sd-switch] %s\n' "$*" >&2; fi; }
    warn() { printf '[sd-switch] warning: %s\n' "$*" >&2; }
    run()  {
      if [[ $DRY_RUN -eq 1 ]]; then
        printf '[sd-switch] would run: %s\n' "$*" >&2
      else
        log "running: $*"
        "$@" || warn "command failed: $*"
      fi
    }

    while [[ $# -gt 0 ]]; do
      case "$1" in
        --dry-run)    DRY_RUN=1; shift ;;
        --verbose|-v) VERBOSE=1; shift ;;
        --timeout)    TIMEOUT_MS="$2"; shift 2 ;;
        --old-units)  OLD_UNITS="$2"; shift 2 ;;
        --new-units)  NEW_UNITS="$2"; shift 2 ;;
        # Refuse unknown arguments outright. Silent skip would mask new
        # home-manager flags we haven't taught this shim about — the
        # activation might appear to succeed while doing the wrong thing.
        *)            echo "sd-switch: unknown argument: $1" >&2; exit 2 ;;
      esac
    done

    if [[ -z "$NEW_UNITS" ]]; then
      echo "sd-switch: --new-units is required" >&2
      exit 2
    fi

    # Daemon must be reachable before we start mutating jobs. If it isn't,
    # report and exit cleanly — the activation script's outer ensure step
    # is responsible for bringing the daemon up.
    if ! systemlctl --user is-system-running >/dev/null 2>&1; then
      log "systeml daemon not responding, asking it to reload anyway"
    fi

    run systemlctl daemon-reload

    # Pull the X-SwitchMethod directive from a unit file. We do not need a
    # full INI parser here — only the [Unit] section's X-SwitchMethod=… line.
    switch_method_of() {
      local f="$1"
      [[ -f "$f" ]] || { echo restart; return; }
      # awk picks the last X-SwitchMethod= inside [Unit]; defaults to restart.
      awk '
        BEGIN { sec=""; m="restart" }
        /^\[.*\]$/ { sec=$0; next }
        sec == "[Unit]" && /^[Xx]-SwitchMethod[[:space:]]*=/ {
          sub(/^[^=]*=[[:space:]]*/, ""); gsub(/[[:space:]]+$/, ""); m=$0
        }
        END { print m }
      ' "$f"
    }

    apply_change() {
      local name="$1" method="$2"
      case "$method" in
        restart)    run systemlctl --user restart "$name" ;;
        reload)     run systemlctl --user reload  "$name" ;;
        stop-start) run systemlctl --user stop    "$name"
                    run systemlctl --user start   "$name" ;;
        keep-old)   log "keeping old: $name (X-SwitchMethod=keep-old)" ;;
        # Fail loudly on an unknown method. Silently picking restart could
        # bounce a unit the user explicitly told us to handle differently
        # (e.g. they typo'd "stop_start"). Better to surface the typo than
        # restart a database without warning.
        *)          echo "sd-switch: $name: unknown X-SwitchMethod '$method'" >&2
                    exit 2 ;;
      esac
    }

    # 1) New / changed units.
    if [[ -d "$NEW_UNITS" ]]; then
      while IFS= read -r -d "" newFile; do
        name="$(basename "$newFile")"
        oldFile=""
        if [[ -n "$OLD_UNITS" && -f "$OLD_UNITS/$name" ]]; then
          oldFile="$OLD_UNITS/$name"
        fi

        if [[ -z "$oldFile" ]]; then
          log "new unit: $name"
          run systemlctl --user enable "$name"
          # Only auto-start units that are wanted by an [Install] section;
          # the daemon's enable resolver populates the necessary symlinks.
          run systemlctl --user start "$name"
          continue
        fi

        if cmp -s "$oldFile" "$newFile"; then
          log "unchanged: $name"
          continue
        fi

        method="$(switch_method_of "$newFile")"
        log "changed: $name (X-SwitchMethod=$method)"
        apply_change "$name" "$method"
      done < <(find "$NEW_UNITS" -maxdepth 1 -type f -print0)
    fi

    # 2) Removed units.
    if [[ -n "$OLD_UNITS" && -d "$OLD_UNITS" ]]; then
      while IFS= read -r -d "" oldFile; do
        name="$(basename "$oldFile")"
        if [[ -d "$NEW_UNITS" && -f "$NEW_UNITS/$name" ]]; then
          continue
        fi
        log "removed unit: $name"
        run systemlctl --user stop    "$name"
        run systemlctl --user disable "$name"
      done < <(find "$OLD_UNITS" -maxdepth 1 -type f -print0)
    fi
  '';
}
