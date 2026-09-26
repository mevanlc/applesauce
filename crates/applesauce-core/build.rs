fn main() {
    #[cfg(feature = "lzfse")]
    build_lzfse();
}

#[cfg(feature = "lzfse")]
fn build_lzfse() {
    println!("cargo:rerun-if-changed=vendor/lzfse");
    // All externally visible symbols are renamed, including internal helpers.
    // The two presets and the unmodified lzfse-sys library must coexist.
    const SYMBOLS: &[&str] = &[
        "lzfse_encode_scratch_size",
        "lzfse_encode_buffer",
        "lzfse_encode_buffer_with_scratch",
        "lzfse_encode_init",
        "lzfse_encode_translate",
        "lzfse_encode_base",
        "lzfse_encode_finish",
        "lzvn_encode",
        "lzvn_encode_scratch_size",
        "lzvn_encode_buffer",
        "fse_init_encoder_table",
        "fse_init_decoder_table",
        "fse_init_value_decoder_table",
        "fse_normalize_freq",
    ];
    for (preset, hash_bits, hash_width, good_match) in
        [("stock", "14", "4", "40"), ("ultra", "16", "8", "100")]
    {
        let mut build = cc::Build::new();
        build
            .warnings(false)
            .include("vendor/lzfse/src")
            .define("LZFSE_ENCODE_HASH_BITS", hash_bits)
            .define("LZFSE_ENCODE_HASH_WIDTH", hash_width)
            .define("LZFSE_ENCODE_GOOD_MATCH", good_match);
        for symbol in SYMBOLS {
            let name = format!("applesauce_lzfse_{preset}_{symbol}");
            build.define(symbol, name.as_str());
        }
        for source in [
            "lzfse_encode.c",
            "lzfse_encode_base.c",
            "lzfse_fse.c",
            "lzvn_encode_base.c",
        ] {
            build.file(format!("vendor/lzfse/src/{source}"));
        }
        build.compile(&format!("applesauce_lzfse_{preset}"));
    }
}
