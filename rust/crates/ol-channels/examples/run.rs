//! run openlumara with only this crate's channels compiled in (smoke tests)
fn main() {
    let mut r = ol_core::registry::Registry::new();
    ol_channels::register(&mut r);
    ol_core::app::run(r, std::env::args().skip(1).collect());
}
