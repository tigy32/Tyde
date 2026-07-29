//! One module per ACP agent Tyde knows something extra about.
//!
//! Adding an agent here is only necessary when it deviates from the
//! specification. An agent that conforms needs no module — it runs on
//! [`stock::StockAdapter`] with a user-configured command.

pub mod kiro;
pub mod stock;
