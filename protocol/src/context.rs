use common::context::Context;

use crate::stream::{
    addr::ConcreteStreamType, connect::ConcreteStreamConnectorTable, connection::Conn,
};

pub type ConcreteContext = Context<Conn, ConcreteStreamConnectorTable, ConcreteStreamType>;
