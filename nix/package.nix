{
  lib,
  rustPlatform,
  pkg-config,
}:

# Builds both `systeml` (daemon) and `systemlctl` (CLI) from the workspace.
#
# Sibling agents own the Rust crates under `crates/`. This derivation only
# wires the workspace into Nix. The Cargo.lock at the repo root is generated
# by `cargo` on first build; if it is missing we still let the build go
# through (the user will see a cargo error rather than an opaque eval error).
rustPlatform.buildRustPackage rec {
  pname = "systeml";
  version = "0.1.0";

  src = lib.cleanSourceWith {
    src = ../.;
    filter =
      path: type:
      let
        baseName = baseNameOf (toString path);
      in
      !(
        # Strip flake-only / editor / build-output noise so the derivation
        # rebuilds only when source actually changes.
        baseName == "target"
        || baseName == "result"
        || baseName == ".direnv"
        || baseName == ".git"
        || baseName == "nix"
        || lib.hasSuffix ".nix" baseName
      );
  };

  cargoLock = {
    lockFile = ../Cargo.lock;
    # No git-only deps yet; populate this map when we add any.
    outputHashes = { };
  };

  nativeBuildInputs = [ pkg-config ];

  # On nixpkgs unstable post-2025-01, the legacy `darwin.apple_sdk.frameworks.*`
  # namespace was retired; the system SDK frameworks (CoreFoundation,
  # CoreServices, Security, SystemConfiguration) come in transparently
  # via stdenv-darwin. No explicit buildInputs needed.

  # Both binaries are built from the workspace; -p selects them explicitly so
  # we do not pull in dev/test-only artifacts.
  cargoBuildFlags = [
    "-p"
    "systeml"
    "-p"
    "systemlctl"
  ];

  # Keep tests opt-in via `nix flake check` rather than slowing every package
  # build; the workspace tests rely on tokio runtimes that can be flaky in
  # the sandbox.
  doCheck = false;

  meta = with lib; {
    description = "User-mode systemd-compatible service manager for macOS";
    homepage = "https://github.com/memorici-de/systeml";
    license = licenses.mit;
    platforms = platforms.darwin ++ platforms.linux;
    mainProgram = "systeml";
  };
}
