{
  description = "Tyde – Tauri desktop app dev environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { nixpkgs, rust-overlay, flake-utils, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };
        glib-schemas = pkgs.lib.concatMapStringsSep ":" (p: "${p}/share/gsettings-schemas/${p.name}") [
          pkgs.gsettings-desktop-schemas
          pkgs.gtk3
        ];
        rust = pkgs.rust-bin.stable.latest.default.override {
          extensions = [ "rust-src" "rust-analyzer" ];
        };
        tauriNativeDeps = with pkgs; [
          webkitgtk_4_1
          gtk3
          glib
          glib-networking
          libsoup_3
          cairo
          pango
          gdk-pixbuf
          atk
          harfbuzz
          librsvg
          openssl
          libayatana-appindicator
          gsettings-desktop-schemas
        ];
      in
      {
        devShells.default = pkgs.mkShell {
          nativeBuildInputs = with pkgs; [
            rust
            pkg-config
            nodejs_22
            cargo-tauri
          ];

          buildInputs = tauriNativeDeps;

          shellHook = ''
            export GIO_MODULE_PATH="${pkgs.glib-networking}/lib/gio/modules"
            export XDG_DATA_DIRS="${glib-schemas}''${XDG_DATA_DIRS:+:$XDG_DATA_DIRS}"
            export PATH="$PWD/node_modules/.bin:$PATH"
            export LD_LIBRARY_PATH="${pkgs.lib.makeLibraryPath tauriNativeDeps}''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
          '';
        };
      }
    );
}
