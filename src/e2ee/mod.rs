//! Gateway-to-gateway encryption with Noise IK.

pub mod dialer;
pub mod handshake;
pub mod identity;
pub mod listener;
pub mod session;

pub use dialer::Dialer;
pub use identity::Identity;
pub use session::NoiseSession;
