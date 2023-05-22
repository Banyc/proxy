use get_config::toml::get_config;
use proxy_server::ProxyServerSpawner;
use tracing_subscriber::EnvFilter;

#[tokio::main]
pub async fn main() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_line_number(true)
        .try_init();
    let spawner: ProxyServerSpawner = get_config().unwrap();
    let mut join_set = tokio::task::JoinSet::new();
    spawner.spawn(&mut join_set).await;
    join_set.join_next().await.unwrap().unwrap().unwrap();
}
