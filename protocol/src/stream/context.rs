use common::stream::context::StreamContext;

use super::{
    addr::ConcreteStreamType, connect::ConcreteStreamConnectorTable, connection::Connection,
};

pub type ConcreteStreamContext =
    StreamContext<Connection, ConcreteStreamConnectorTable, ConcreteStreamType>;
