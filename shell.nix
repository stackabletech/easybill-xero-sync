# Development shell for the easybill → Xero exporter (Rust).
#
# Usage:
#   nix-shell            # drop into a shell with the full Rust toolchain
#   direnv allow         # or let direnv load it automatically (see .envrc;
#                        # personal additions go in .envrc.local)
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
  name = "easybill-xero";

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
    echo "easybill-xero dev shell — $(rustc --version)"

    # Load the credentials from ~/.secrets/xero.
    #
    # This is done here in the shellHook (plain bash at shell entry), NOT via
    # builtins.readFile: reading the secret at Nix eval time would copy it into
    # the world-readable Nix store. Values are never printed.
    #
    # Supported layouts:
    #   - a file:      ~/.secrets/xero  containing `KEY=value` lines (with or
    #                  without `export`); every variable in it is exported
    #   - a directory: ~/.secrets/xero/ with one file per value:
    #                  client_id          -> XERO_CLIENT_ID
    #                  client_secret      -> XERO_CLIENT_SECRET
    #                  easybill_api_token -> EASYBILL_API_TOKEN
    #                  auth               -> XERO_AUTH (`oauth` or `client-credentials`)
    xero_secret="$HOME/.secrets/xero"
    if [ -f "$xero_secret" ]; then
      set -a
      # shellcheck disable=SC1090
      . "$xero_secret"
      set +a
    elif [ -d "$xero_secret" ]; then
      for entry in client_id:XERO_CLIENT_ID client_secret:XERO_CLIENT_SECRET \
                   easybill_api_token:EASYBILL_API_TOKEN auth:XERO_AUTH; do
        file="$xero_secret/''${entry%%:*}"
        [ -f "$file" ] && export "''${entry#*:}=$(cat "$file")"
      done
      unset entry file
    fi

    missing=""
    for var in XERO_CLIENT_ID XERO_CLIENT_SECRET EASYBILL_API_TOKEN; do
      [ -n "$(printenv "$var")" ] || missing="$missing $var"
    done
    if [ -z "$missing" ]; then
      echo "Credentials loaded from $xero_secret (Xero auth: ''${XERO_AUTH:-oauth})"
    else
      echo "Not set:$missing — expected $xero_secret as an env file" \
           "or a directory with one file per value (see shell.nix)."
    fi
    unset missing var
  '';
}
