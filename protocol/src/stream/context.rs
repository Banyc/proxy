use common::stream::context::StreamContext;

use super::connect::ConcreteStreamConnectorTable;

pub type ConcreteStreamContext = StreamContext<ConcreteStreamConnectorTable>;
