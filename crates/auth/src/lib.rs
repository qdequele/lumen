//! Authentication, virtual keys, quotas and hard budgets for Ferrogate.
//!
//! Implemented starting at milestone M5. Budget enforcement happens *inside*
//! the request path, before any upstream call, so an exhausted budget can
//! never leak spend to a provider.
