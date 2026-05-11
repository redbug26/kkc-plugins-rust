#!/bin/bash
cargo build
mkdir -p "$HOME/Library/Application Support/be.kyuran.kkc/plugins/ayt"
cp "$HOME/.rust-target/debug/libkkc_viewer_ayt.dylib" "$HOME/Library/Application Support/be.kyuran.kkc/plugins/ayt"
cp plugin.toml "$HOME/Library/Application Support/be.kyuran.kkc/plugins/ayt"
