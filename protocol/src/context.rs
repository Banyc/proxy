use common::context::Context;

use crate::stream::{
    addr::ConcreteStreamType, connection::Connection, connector_table::ConcreteStreamConnectorTable,
};

pub type ConcreteContext = Context<Connection, ConcreteStreamConnectorTable, ConcreteStreamType>;
