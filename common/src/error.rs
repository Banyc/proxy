use std::{fmt, io};

pub type AnyResult = Result<(), AnyError>;
pub type AnyError = Box<dyn std::error::Error + Send + Sync>;

/// Wrap a listener-bind failure so a port collision names the address that
/// collided. Walks the error's source chain (a newtype like
/// `ListenerBindError(io::Error)` keeps its `io::Error` as its source), so
/// an `AddrInUse` is detected through the wrappers and gets the "another
/// instance holds this port" hint; any other failure passes through with
/// the address prefixed.
pub fn bind_error(
    addr: impl fmt::Display,
    error: &(dyn std::error::Error + Send + Sync + 'static),
) -> io::Error {
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(error);
    let mut addr_in_use = false;
    while let Some(e) = current {
        if e.downcast_ref::<io::Error>()
            .is_some_and(|io| io.kind() == io::ErrorKind::AddrInUse)
        {
            addr_in_use = true;
            break;
        }
        current = e.source();
    }
    let kind = if addr_in_use {
        io::ErrorKind::AddrInUse
    } else {
        io::ErrorKind::Other
    };
    let detail = if addr_in_use {
        "port already in use (another proxy instance already holds it?)".to_owned()
    } else {
        error.to_string()
    };
    io::Error::new(kind, format!("listen address {addr}: {detail}"))
}
