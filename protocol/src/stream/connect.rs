use common::stream::pool::ConnectError;

use super::addr::ConcreteStreamType;

pub type ConcreteConnectError = ConnectError<ConcreteStreamType>;
