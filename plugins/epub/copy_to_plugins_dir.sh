#!/bin/bash
cargo build
mkdir /Users/miguelvanhove/Library/Application\ Support/be.kyuran.kkc/plugins/epub
cp /Users/miguelvanhove/.rust-target/debug/libkkc_viewer_epub.dylib /Users/miguelvanhove/Library/Application\ Support/be.kyuran.kkc/plugins/epub
cp plugin.toml /Users/miguelvanhove/Library/Application\ Support/be.kyuran.kkc/plugins/epub
