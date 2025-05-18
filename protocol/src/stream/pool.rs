use common::stream::{
    AsConn,
    addr::{StreamAddr, StreamAddrStr},
    pool::{ConnectError, PoolBuilder},
};
use swap::Swap;
use tokio_conn_pool::ConnPool;

pub type ConcretePoolBuilder = PoolBuilder<StreamAddrStr>;

pub type ConcreteConnPool = ConnPool<StreamAddr, Box<dyn AsConn>>;
pub type SharedConcreteConnPool = Swap<ConcreteConnPool>;

pub type ConcreteConnectError = ConnectError;
