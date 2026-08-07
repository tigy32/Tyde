# Tyde Agent Adapter

Shared backend traits, lifecycle contracts, and conformance utilities for
Tyde agent integrations.

This crate depends only on Tyde's wire-level `protocol` crate. Backend
implementations remain in the server and depend on this crate, never the other
way around.
