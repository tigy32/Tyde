//! Mobile UI primitives.
//!
//! Small, mobile-first Leptos building blocks that the feature
//! components compose. Lives in one place so visual identity, touch
//! targets, and accessibility patterns stay consistent across the
//! product surface.
//!
//! Every primitive that renders an interactive element accepts a
//! `data-mobile-test="…"` attribute so wasm tests can target it by
//! semantic id rather than CSS class. CSS classes are styling-only.
mod button;
mod card;
mod empty_state;
mod pill;
mod safe_area;
mod skeleton;
mod spinner;
mod status_dot;

pub use button::{Button, ButtonSize, ButtonVariant};
pub use card::Card;
pub use empty_state::EmptyState;
pub use pill::{Pill, PillTone};
pub use safe_area::SafeArea;
pub use skeleton::Skeleton;
pub use spinner::Spinner;
pub use status_dot::{StatusDot, StatusTone};
