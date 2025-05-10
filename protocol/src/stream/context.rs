use common::stream::context::StreamContext;

use super::{
    addr::ConcreteStreamType, connect::ConcreteStreamConnectorTable, connection::Conn,
};

pub type ConcreteStreamContext =
    StreamContext<Conn, ConcreteStreamConnectorTable, ConcreteStreamType>;
