use common::context::Context;

use crate::stream::{
    addr::ConcreteStreamType, connect::ConcreteStreamConnectorTable, connection::Connection,
};

pub type ConcreteContext = Context<Connection, ConcreteStreamConnectorTable, ConcreteStreamType>;
