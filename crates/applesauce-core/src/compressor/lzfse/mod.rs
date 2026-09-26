mod external;
#[cfg(target_os = "macos")]
mod system;
mod vendor;

use crate::compressor::lz;
pub type Crate = lz::Lz<external::Impl>;
#[cfg(target_os = "macos")]
pub type Macos = lz::Lz<system::Impl>;
pub type Vendor = lz::Lz<vendor::Impl<false>>;
pub type VendorUltra = lz::Lz<vendor::Impl<true>>;

#[cfg(all(
    not(feature = "vendor-ultra-lzfse"),
    not(feature = "vendor-lzfse"),
    feature = "system-lzfse",
    target_os = "macos"
))]
pub type Lzfse = Macos;
#[cfg(all(
    not(feature = "vendor-ultra-lzfse"),
    not(feature = "vendor-lzfse"),
    not(all(feature = "system-lzfse", target_os = "macos"))
))]
pub type Lzfse = Crate;
#[cfg(all(not(feature = "vendor-ultra-lzfse"), feature = "vendor-lzfse"))]
pub type Lzfse = Vendor;
#[cfg(feature = "vendor-ultra-lzfse")]
pub type Lzfse = VendorUltra;

#[test]
fn round_trip() {
    let mut compressor = Lzfse::new();
    super::tests::compressor_round_trip(&mut compressor);
}
