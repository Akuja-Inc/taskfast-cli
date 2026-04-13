//! Deterministic exit-code taxonomy — stable contract for agent orchestrators.

#[derive(Debug, Clone, Copy)]
pub enum ExitCode {
    Success = 0,
    Usage = 2,
    Auth = 3,
    RateLimited = 4,
    Wallet = 5,
    Server = 6,
    Validation = 7,
    Unimplemented = 70,
}

impl From<ExitCode> for std::process::ExitCode {
    fn from(code: ExitCode) -> Self {
        std::process::ExitCode::from(code as u8)
    }
}
