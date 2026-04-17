//! Dynamic-dispatch wrapper over enabled chain features.
//!
//! With zero features enabled this is an empty (unconstructable) enum — matches
//! Rust's "no variant = bottom type" idiom and keeps `cargo check
//! --no-default-features` honest. Each enabled feature adds its variant.

#[derive(Debug)]
pub enum AnyChain {
    #[cfg(feature = "tempo")]
    Tempo(crate::tempo::Tempo),
    #[cfg(feature = "polygon")]
    Polygon(crate::polygon::Polygon),
    #[cfg(feature = "avalanche")]
    Avalanche(crate::avalanche::Avalanche),
    #[cfg(feature = "solana")]
    Solana(crate::solana::Solana),
    #[cfg(feature = "near")]
    Near(crate::near::Near),
    #[cfg(feature = "stellar")]
    Stellar(crate::stellar::Stellar),
}

#[cfg(feature = "tempo")]
impl From<crate::tempo::Tempo> for AnyChain {
    fn from(c: crate::tempo::Tempo) -> Self {
        Self::Tempo(c)
    }
}

#[cfg(feature = "polygon")]
impl From<crate::polygon::Polygon> for AnyChain {
    fn from(c: crate::polygon::Polygon) -> Self {
        Self::Polygon(c)
    }
}

#[cfg(feature = "avalanche")]
impl From<crate::avalanche::Avalanche> for AnyChain {
    fn from(c: crate::avalanche::Avalanche) -> Self {
        Self::Avalanche(c)
    }
}

#[cfg(feature = "solana")]
impl From<crate::solana::Solana> for AnyChain {
    fn from(c: crate::solana::Solana) -> Self {
        Self::Solana(c)
    }
}

#[cfg(feature = "near")]
impl From<crate::near::Near> for AnyChain {
    fn from(c: crate::near::Near) -> Self {
        Self::Near(c)
    }
}

#[cfg(feature = "stellar")]
impl From<crate::stellar::Stellar> for AnyChain {
    fn from(c: crate::stellar::Stellar) -> Self {
        Self::Stellar(c)
    }
}
