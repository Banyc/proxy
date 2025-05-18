use common::stream::context::StreamContext;

use super::{connect::ConcreteStreamConnectorTable, connection::Conn};

pub type ConcreteStreamContext = StreamContext<Conn, ConcreteStreamConnectorTable>;
