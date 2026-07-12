//! Routing layer for Ferrogate.
//!
//! Resolves a `(capability, model)` pair to a concrete provider, and (from
//! milestone M6) applies fallback chains, circuit breaking and load balancing.
//! Implemented starting at milestone M2.
