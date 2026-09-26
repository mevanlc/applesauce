# Vendored LZFSE encoder

Source: Apple's [LZFSE reference implementation](https://github.com/lzfse/lzfse),
copied from the `lzfse/` directory distributed in `lzfse-sys` 2.0.0.
That crate records Rust-wrapper commit `1290f96297e1895948589b94c0850fbabe0873aa`
and has crates.io archive SHA-256
`23c80418f1cfc4601f8ef2055dcea77eebea47d2edc74ee0dadb005dd3a9f604`.
Only the encoder C files and headers are included. See LICENSE for Apple's BSD
license; the original copyright notices are retained in each source file.

Local changes: three settings in `lzfse_tunables.h` have `#ifndef` guards so
`build.rs` can compile two presets from the same source:

| Backend | Hash bits | Hash width | Good-match threshold |
| --- | --- | --- | --- |
| `vendor` | 14 | 4 | 40 |
| `vendor-ultra` | 16 | 8 | 100 |

`vendor-ultra` uses eight times as much history-table memory and searches more candidate
matches before accepting one. This trades encoder memory and time for potentially
smaller output; it does not guarantee a size improvement for every input.
The LZVN fallback threshold remains 4096 bytes in both presets.
The backend selects the preset; `-l` has no effect. Each backend allocates workspace
for its own encoder and the standard decoder.

All external symbols are prefixed separately for each preset by `build.rs` to
avoid collisions with each other and with `lzfse-sys`. Output uses the standard
LZFSE format. Decoding uses the unmodified `lzfse-sys` decoder.
