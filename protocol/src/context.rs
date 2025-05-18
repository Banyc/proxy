use common::context::Context;

use crate::stream::{connect::ConcreteStreamConnectorTable, connection::Conn};

pub type ConcreteContext = Context<Conn, ConcreteStreamConnectorTable>;
