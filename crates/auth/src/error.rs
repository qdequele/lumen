//! Error type for the auth/storage layer.
//!
//! These are *internal* errors: the server maps them to `LM-5001` (opaque
//! 500) - never to a misleading 401 during a gateway malfunction. Variants
//! never embed key material.

/// An auth/storage failure.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// A database operation failed.
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),

    /// Applying embedded migrations failed.
    #[error("migration error: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),

    /// `LUMEN_MASTER_KEY` is missing or malformed.
    #[error("invalid master key: {0}")]
    InvalidMasterKey(&'static str),

    /// Decryption failed: wrong master key or corrupted ciphertext.
    /// Carries no detail by design - there is nothing safe to say.
    #[error("provider key decryption failed")]
    Decrypt,

    /// A key create/patch referenced a budget group that does not exist or
    /// was soft-deleted (ADR 009). Unlike the other variants this one maps
    /// to a client error (400 `LM-1001`), not an opaque 500: the caller
    /// named the group, so naming it back leaks nothing.
    #[error("unknown budget group '{0}'")]
    UnknownGroup(String),

    /// A grant targeted an active key or group whose `budget_max` is NULL
    /// (unlimited). There is no cap to raise; the caller must set one with
    /// a PATCH first. Client error (400 `LM-1001`) like [`Self::UnknownGroup`]:
    /// the caller named the id, so naming it back leaks nothing.
    #[error("'{0}' has no budget cap to grant to (budget_max is unlimited)")]
    NoBudgetCap(String),
}
