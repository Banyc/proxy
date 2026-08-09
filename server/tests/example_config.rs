//! Temporary verification: the example config must deserialize through the
//! real `ServerConfig` (deny_unknown_fields, RouteAddr parsing, ...).

#[test]
fn example_config_deserializes() {
    let src = std::fs::read_to_string("config.toml").unwrap();
    let config: server::ServerConfig = toml::from_str(&src).unwrap();
    assert_eq!(config.reverse_tunnel.initiator.len(), 2);
    assert_eq!(config.reverse_tunnel.responder.len(), 2);
}
