# SystemL — agent notes

This file is loaded into Claude's context for every session in this repo.

## What this is
User-mode systemd-compatible service manager for macOS. See `README.md` for
the architecture and phase scope. Persistent project memory in
`~/.claude/projects/-Users-sweater-Github-systeml/memory/` has more detail.

## Reference checkouts (read-only siblings)
- `/Users/sweater/Github/grim-monolith/crates/wayfinder/src/ship/local/launchd.rs` — wayfinder's launchd adapter ("genius"). Reusable patterns for `Restart`→`KeepAlive`, `OnCalendar`→`StartCalendarInterval`. **Don't edit.**
- `/Users/sweater/Github/systemd-stable/` — upstream systemd source. Cross-reference semantics; **don't edit.**
- `/Users/sweater/Github/home-manager/modules/systemd.nix` — Linux activation logic + option schema. The compat overlay must mirror this 1:1.

## House rules
- Edition 2021, resolver 2.
- Workspace lints are strict: `unsafe_code = "deny"`, `clippy::all = "warn"`, `clippy::cargo = "warn"`. Unsafe is allowed only inside `systeml-runtime` (fork/exec/kqueue FFI) with scoped `#[allow(unsafe_code)]`.
- No `rustfmt.toml` / `clippy.toml` overrides — defaults only.
- Linux-kernel-only directives are **parsed, warned, ignored** — never erroring. They round-trip in `cat`/`show` output for fidelity.
- D-Bus interface name is `org.freedesktop.systemd1` (not `org.memorici.systeml`). The point is upstream-tool compat. Object path root is `/org/freedesktop/systemd1`.
- Bus socket lives at `$XDG_RUNTIME_DIR/systeml/private` on macOS. `XDG_RUNTIME_DIR` defaults to `/private/tmp/systeml-$UID` if unset (macOS has no XDG runtime dir by convention).
- Unit search paths (user scope): `$XDG_CONFIG_HOME/systemd/user/`, `~/.config/systemd/user/`, `/etc/systemd/user/`, `/usr/local/lib/systemd/user/`, `/run/systemd/user/`. Match systemd-stable's order.
- State directory: `$XDG_STATE_HOME/systeml/` (timers/, journal-stub/, …).

## Commit style (C4)
Atomic non-breaking commits. First line ≤72 chars. Body has Problem/Solution.
Never amend; always create a new commit. `unsafe_code` warnings or clippy errors block.
