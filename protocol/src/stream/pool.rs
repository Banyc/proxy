use common::stream::{
    AsConn,
    pool::{ConnectError, PoolBuilder},
};
use swap::Swap;
use tokio_conn_pool::ConnPool;

use super::addr::{ConcreteStreamAddr, ConcreteStreamAddrStr};

pub type ConcretePoolBuilder = PoolBuilder<ConcreteStreamAddrStr>;

pub type ConcreteConnPool = ConnPool<ConcreteStreamAddr, Box<dyn AsConn>>;
pub type SharedConcreteConnPool = Swap<ConcreteConnPool>;

pub type ConcreteConnectError = ConnectError;
