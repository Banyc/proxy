use common::stream::pool::PoolBuilder;
use swap::Swap;
use tokio_conn_pool::ConnPool;

use super::{
    addr::{ConcreteStreamAddr, ConcreteStreamAddrStr},
    connection::Connection,
};

pub type ConcretePoolBuilder = PoolBuilder<ConcreteStreamAddrStr>;

pub type ConcreteConnPool = ConnPool<ConcreteStreamAddr, Connection>;
pub type SharedConcreteConnPool = Swap<ConcreteConnPool>;
