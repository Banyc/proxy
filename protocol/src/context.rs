use common::proto::context::Context;

use crate::stream::connect::ConcreteStreamConnectorTable;

pub type ConcreteContext = Context<ConcreteStreamConnectorTable>;
