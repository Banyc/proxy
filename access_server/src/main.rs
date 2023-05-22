use access_server::AccessServerSpawner;
use get_config::toml::get_config;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let spawner: AccessServerSpawner = get_config().unwrap();
    let mut join_set = tokio::task::JoinSet::new();
    spawner.spawn(&mut join_set).await;
    join_set.join_next().await.unwrap().unwrap();
}
