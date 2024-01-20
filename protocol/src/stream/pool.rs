use common::stream::pool::{ConnectError, PoolBuilder};
use swap::Swap;
use tokio_conn_pool::ConnPool;

use super::{
    addr::{ConcreteStreamAddr, ConcreteStreamAddrStr, ConcreteStreamType},
    connection::Connection,
};

pub type ConcretePoolBuilder = PoolBuilder<ConcreteStreamAddrStr>;

pub type ConcreteConnPool = ConnPool<ConcreteStreamAddr, Connection>;
pub type SharedConcreteConnPool = Swap<ConcreteConnPool>;

pub type ConcreteConnectError = ConnectError<ConcreteStreamType>;
