#![cfg(feature = "lzfse")]

use applesauce_core::compressor::{Encoder, Kind, LzfseBackend};
use applesauce_core::BLOCK_SIZE;

fn backends() -> Vec<LzfseBackend> {
    let mut backends = vec![
        LzfseBackend::Crate,
        LzfseBackend::Vendor,
        LzfseBackend::VendorUltra,
    ];
    if cfg!(target_os = "macos") {
        backends.push(LzfseBackend::Macos);
    }
    backends
}

fn encode(encoder: Encoder, input: &[u8], level: u32) -> Vec<u8> {
    let mut compressor = encoder.compressor().unwrap();
    assert_eq!(compressor.kind(), Kind::Lzfse);
    let mut output = vec![0; input.len() + 1024];
    let len = compressor.compress(&mut output, input, level).unwrap();
    output.truncate(len);
    output
}

#[test]
fn all_backends_cross_decode_at_format_boundaries() {
    let mut state = 0x9876_5432_1234_5678_u64;
    let random: Vec<u8> = (0..BLOCK_SIZE)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect();
    let source = include_bytes!("../vendor/lzfse/src/lzfse_encode_base.c");
    let text: Vec<u8> = source.iter().copied().cycle().take(BLOCK_SIZE).collect();
    for input in [&random, &text] {
        // Empty files contain no compressed blocks; covered by the reader tests.
        for len in [1, 7, 8, 255, 4095, 4096, BLOCK_SIZE - 1, BLOCK_SIZE] {
            for backend in backends() {
                let compressed = encode(Encoder::lzfse(backend), &input[..len], 5);
                for decoder in backends() {
                    let mut decoder = Encoder::lzfse(decoder).compressor().unwrap();
                    let mut decoded = vec![0; len + 1];
                    let size = decoder.decompress(&mut decoded, &compressed).unwrap();
                    assert_eq!(&decoded[..size], &input[..len], "{backend:?}");
                }
            }
        }
    }
}

#[test]
fn vendor_backends_are_fixed_and_stock_matches_crate() {
    let input = include_bytes!("../vendor/lzfse/src/lzfse_encode_base.c");
    let stock = encode(Encoder::lzfse(LzfseBackend::Crate), input, 5);
    let ultra = encode(Encoder::lzfse(LzfseBackend::VendorUltra), input, 5);
    // This source fixture exercises the stronger match finder.
    assert_ne!(stock, ultra);
    for (backend, expected) in [
        (LzfseBackend::Vendor, stock),
        (LzfseBackend::VendorUltra, ultra),
    ] {
        let encoder = Encoder::lzfse(backend);
        assert!(!encoder.supports_level());
        let mut vendor = encoder.compressor().unwrap();
        let mut output = vec![0; input.len() + 1024];
        for level in 1..=12 {
            let len = vendor.compress(&mut output, input, level).unwrap();
            assert_eq!(&output[..len], expected);
            let mut decoded = vec![0; input.len() + 1];
            let len = vendor.decompress(&mut decoded, &output[..len]).unwrap();
            assert_eq!(&decoded[..len], input);
        }
    }
}

#[test]
fn feature_selected_default_matches_explicit_backend() {
    let input = include_bytes!("../vendor/lzfse/src/lzfse_encode_base.c");
    let expected = if cfg!(feature = "vendor-ultra-lzfse") {
        LzfseBackend::VendorUltra
    } else if cfg!(feature = "vendor-lzfse") {
        LzfseBackend::Vendor
    } else if cfg!(all(feature = "system-lzfse", target_os = "macos")) {
        LzfseBackend::Macos
    } else {
        LzfseBackend::Crate
    };
    assert_eq!(LzfseBackend::default(), expected);
    assert_eq!(Encoder::from(Kind::Lzfse), Encoder::lzfse(expected));
    assert_eq!(
        encode(Kind::Lzfse.into(), input, 5),
        encode(Encoder::lzfse(expected), input, 5)
    );
}
