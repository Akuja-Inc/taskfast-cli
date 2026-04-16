//! Feature-matrix smoke: each cfg'd module type-resolves when its feature is
//! enabled. Pairs with CI `cargo check --no-default-features --features <x>`.

use taskfast_chains::Chain;

#[cfg(feature = "tempo")]
#[test]
fn tempo_id_is_tempo() {
    assert_eq!(<taskfast_chains::tempo::Tempo as Chain>::id(), "tempo");
}

#[cfg(feature = "polygon")]
#[test]
fn polygon_stub_compiles() {
    assert_eq!(
        <taskfast_chains::polygon::Polygon as Chain>::id(),
        "polygon"
    );
}

#[cfg(feature = "avalanche")]
#[test]
fn avalanche_stub_compiles() {
    assert_eq!(
        <taskfast_chains::avalanche::Avalanche as Chain>::id(),
        "avalanche"
    );
}

#[cfg(feature = "solana")]
#[test]
fn solana_stub_compiles() {
    assert_eq!(<taskfast_chains::solana::Solana as Chain>::id(), "solana");
}

#[cfg(feature = "near")]
#[test]
fn near_stub_compiles() {
    assert_eq!(<taskfast_chains::near::Near as Chain>::id(), "near");
}

#[cfg(feature = "stellar")]
#[test]
fn stellar_stub_compiles() {
    assert_eq!(
        <taskfast_chains::stellar::Stellar as Chain>::id(),
        "stellar"
    );
}
