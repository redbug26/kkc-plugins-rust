#!/bin/bash
cargo build
mkdir /Users/miguelvanhove/Library/Application\ Support/be.kyuran.kkc/plugins/xlsx
cp /Users/miguelvanhove/.rust-target/debug/libkkc_viewer_xlsx.dylib /Users/miguelvanhove/Library/Application\ Support/be.kyuran.kkc/plugins/xlsx
cp plugin.toml /Users/miguelvanhove/Library/Application\ Support/be.kyuran.kkc/plugins/xlsx
