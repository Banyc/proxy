pub type AnyResult = Result<(), AnyError>;
pub type AnyError = Box<dyn std::error::Error + Send + Sync>;
