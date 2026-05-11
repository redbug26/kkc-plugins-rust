#!/bin/bash
cargo build
mkdir /Users/miguelvanhove/Library/Application\ Support/be.kyuran.kkc/plugins/pdf
cp /Users/miguelvanhove/.rust-target/debug/libkkc_viewer_pdf.dylib /Users/miguelvanhove/Library/Application\ Support/be.kyuran.kkc/plugins/pdf
cp plugin.toml /Users/miguelvanhove/Library/Application\ Support/be.kyuran.kkc/plugins/pdf
