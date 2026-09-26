use super::{Compressor, Kind};

/// LZFSE implementation used to encode the same on-disk format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LzfseBackend {
    Macos,
    Crate,
    Vendor,
    VendorUltra,
}

impl Default for LzfseBackend {
    fn default() -> Self {
        if cfg!(feature = "vendor-ultra-lzfse") {
            Self::VendorUltra
        } else if cfg!(feature = "vendor-lzfse") {
            Self::Vendor
        } else if cfg!(all(feature = "system-lzfse", target_os = "macos")) {
            Self::Macos
        } else {
            Self::Crate
        }
    }
}

/// Encoder selection, separate from the format stored in decmpfs headers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Encoder {
    kind: Kind,
    lzfse_backend: LzfseBackend,
}

impl From<Kind> for Encoder {
    fn from(kind: Kind) -> Self {
        Self {
            kind,
            lzfse_backend: LzfseBackend::default(),
        }
    }
}

impl Encoder {
    #[must_use]
    pub const fn lzfse(backend: LzfseBackend) -> Self {
        Self {
            kind: Kind::Lzfse,
            lzfse_backend: backend,
        }
    }

    #[must_use]
    pub const fn kind(self) -> Kind {
        self.kind
    }

    #[must_use]
    pub const fn supports_level(self) -> bool {
        matches!(self.kind, Kind::Zlib)
    }

    #[must_use]
    pub fn compressor(self) -> Option<Compressor> {
        if self.kind != Kind::Lzfse {
            return self.kind.compressor();
        }
        #[cfg(feature = "lzfse")]
        {
            use super::{lzfse, Data};
            let data = match self.lzfse_backend {
                LzfseBackend::Crate => Data::LzfseCrate(lzfse::Crate::new()),
                LzfseBackend::Vendor => Data::LzfseVendor(lzfse::Vendor::new()),
                LzfseBackend::VendorUltra => Data::LzfseVendorUltra(lzfse::VendorUltra::new()),
                #[cfg(target_os = "macos")]
                LzfseBackend::Macos => Data::LzfseMacos(lzfse::Macos::new()),
                #[cfg(not(target_os = "macos"))]
                LzfseBackend::Macos => return None,
            };
            Some(Compressor(data))
        }
        #[cfg(not(feature = "lzfse"))]
        None
    }
}
