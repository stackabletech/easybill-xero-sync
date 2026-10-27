# Development shell for the easybill → Xero exporter (Rust).
#
# Usage:
#   nix-shell            # drop into a shell with the full Rust toolchain
#   direnv allow         # or let direnv load it automatically (see .envrc)
#
# The toolchain comes from nixpkgs, so `rustup` is not involved.
#
# By default this tracks your ambient <nixpkgs> channel. To pin a specific
# nixpkgs revision for reproducibility, replace the `pkgs ?` default with a
# fetchTarball of a known commit, e.g.:
#
#   pkgs ? import (fetchTarball {
#     url = "https://github.com/NixOS/nixpkgs/archive/<commit>.tar.gz";
#     sha256 = "<run once; Nix prints the correct hash>";
#   }) { }

{ pkgs ? import <nixpkgs> { } }:

pkgs.mkShell {
  name = "xero-export";

  # Rust toolchain + build tools.
  nativeBuildInputs = with pkgs; [
    rustc
    cargo
    clippy
    rustfmt
    rust-analyzer

    pkg-config
  ];

  # Let rust-analyzer resolve the standard library sources.
  RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";

  # CA bundle so `cargo` can fetch crates from inside the shell.
  SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";

  shellHook = ''
    echo "xero-export dev shell — $(rustc --version)"

    # Load Xero OAuth credentials from ~/.secrets/xero.
    #
    # This is done here in the shellHook (plain bash at shell entry), NOT via
    # builtins.readFile: reading the secret at Nix eval time would copy it into
    # the world-readable Nix store. Values are never printed.
    #
    # Supported layouts:
    #   - a file:      ~/.secrets/xero  containing `XERO_CLIENT_ID=...` /
    #                  `XERO_CLIENT_SECRET=...` lines (with or without `export`)
    #   - a directory: ~/.secrets/xero/{client_id,client_secret}
    xero_secret="$HOME/.secrets/xero"
    if [ -f "$xero_secret" ]; then
      set -a
      # shellcheck disable=SC1090
      . "$xero_secret"
      set +a
    elif [ -d "$xero_secret" ]; then
      [ -f "$xero_secret/client_id" ] \
        && export XERO_CLIENT_ID="$(cat "$xero_secret/client_id")"
      [ -f "$xero_secret/client_secret" ] \
        && export XERO_CLIENT_SECRET="$(cat "$xero_secret/client_secret")"
    fi

    if [ -n "''${XERO_CLIENT_ID:-}" ] && [ -n "''${XERO_CLIENT_SECRET:-}" ]; then
      echo "Xero credentials loaded from $xero_secret"
    else
      echo "Xero credentials NOT loaded — expected $xero_secret as an env file" \
           "or a directory with client_id/client_secret files."
    fi
  '';
}
