use common::stream::context::StreamContext;

use super::{
    addr::ConcreteStreamType, connection::Connection, connector_table::ConcreteStreamConnectorTable,
};

pub type ConcreteStreamContext =
    StreamContext<Connection, ConcreteStreamConnectorTable, ConcreteStreamType>;
