//! Error type for the auth/storage layer.
//!
//! These are *internal* errors: the server maps them to `LM-5001` (opaque 500)
//! — never to a misleading 401 during a gateway malfunction. Variants never
//! embed key material.

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
    /// Carries no detail by design — there is nothing safe to say.
    #[error("provider key decryption failed")]
    Decrypt,
}
