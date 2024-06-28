#[cfg(feature = "dhat-heap")]
use axum::{extract::State, routing::get, Router};
#[cfg(feature = "dhat-heap")]
pub fn profiler_router(profiler: dhat::Profiler) -> Router {
    Router::new()
        .route(
            "/profile",
            get(
                |State(profiler): State<
                    std::sync::Arc<std::sync::Mutex<Option<dhat::Profiler>>>,
                >| async move {
                    let mut profiler = profiler.lock().unwrap();
                    drop(profiler.take().unwrap());
                    std::process::exit(0);
                    #[allow(unreachable_code)]
                    ()
                },
            ),
        )
        .with_state(std::sync::Arc::new(std::sync::Mutex::new(Some(profiler))))
}
