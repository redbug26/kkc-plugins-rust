#!/bin/bash
cargo build --release
mkdir -p "$HOME/Library/Application Support/be.kyuran.kkc/plugins/ym"
cp "$HOME/.rust-target/release/libkkc_audio_ym.dylib" "$HOME/Library/Application Support/be.kyuran.kkc/plugins/ym"
cp plugin.toml "$HOME/Library/Application Support/be.kyuran.kkc/plugins/ym"
