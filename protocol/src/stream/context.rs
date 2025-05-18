use common::proto::context::StreamContext;

use super::connect::ConcreteStreamConnectorTable;

pub type ConcreteStreamContext = StreamContext<ConcreteStreamConnectorTable>;
