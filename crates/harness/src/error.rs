//! Error type shared by every module.

use thiserror::Error;

/// Result alias used across the crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Failure of one harness operation.
///
/// The variants are kept separate because the CLI maps them to different exit
/// codes, and because "the model refused" is not "the host refused": one is
/// recoverable by re-running with a different model, the other is an
/// installation or capability problem.
#[derive(Debug, Error)]
pub enum Error {
    /// The host rejected the call, died, or reported a failed load.
    ///
    /// The message is the host's own text. No numeric code is attached: codes
    /// exist so a *wire* client can branch without parsing text, and there is no
    /// wire between the harness and the host in this build.
    #[error("host: {0}")]
    Host(String),

    /// The model endpoint failed, or returned something unusable.
    #[error("model: {0}")]
    Model(String),

    /// Local configuration is wrong or incomplete.
    #[error("config: {0}")]
    Config(String),

    /// Local file or process I/O failed.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// A payload did not match the expected shape.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}
