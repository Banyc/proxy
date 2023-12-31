use std::{
    collections::{HashMap, VecDeque},
    io,
    net::SocketAddr,
    ops::DerefMut,
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};

use arc_swap::ArcSwap;
use metrics::counter;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{sync::RwLock as TokioRwLock, task::JoinSet};
use tracing::warn;

use crate::{
    header::heartbeat::{send_noop, HeartbeatError},
    proxy_table::ProxyConfig,
    stream::{
        proxy_table::{StreamProxyConfigBuildError, StreamProxyConfigBuilder},
        IoAddr,
    },
};

use super::{
    addr::{ConcreteStreamAddr, ConcreteStreamAddrStr},
    connector_table::STREAM_CONNECTOR_TABLE,
    created_stream::CreatedStream,
};

const QUEUE_LEN: usize = 16;
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
const RETRY_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoolBuilder(#[serde(default)] pub Vec<StreamProxyConfigBuilder<ConcreteStreamAddrStr>>);

impl PoolBuilder {
    pub fn new() -> Self {
        Self(vec![])
    }

    pub fn build(self) -> Result<Pool, StreamProxyConfigBuildError> {
        let c = self
            .0
            .into_iter()
            .map(|c| c.build())
            .collect::<Result<Vec<_>, _>>()?;
        let pool = Pool::new(c.into_iter());
        Ok(pool)
    }
}

impl Default for PoolBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
struct SocketCell {
    cell: Arc<TokioRwLock<Option<CreatedStream>>>,
    _task_handle: JoinSet<Result<(), HeartbeatError>>,
}

impl SocketCell {
    pub async fn create(
        proxy_config: ProxyConfig<ConcreteStreamAddr>,
        heartbeat_interval: Duration,
    ) -> io::Result<Self> {
        let addr = proxy_config.address.clone();
        let sock_addr = addr.address.to_socket_addr().await?;
        let stream = STREAM_CONNECTOR_TABLE
            .timed_connect(
                proxy_config.address.stream_type,
                sock_addr,
                HEARTBEAT_INTERVAL,
            )
            .await?;
        let cell = Arc::new(TokioRwLock::new(Some(stream)));
        let mut task_handle = JoinSet::new();
        counter!("stream_pool.put_streams").increment(1);
        task_handle.spawn({
            let cell = Arc::clone(&cell);
            async move {
                loop {
                    tokio::time::sleep(heartbeat_interval).await;
                    let mut cell = cell.write().await;
                    let stream = match cell.deref_mut() {
                        Some(x) => x,
                        None => break,
                    };
                    match send_noop(stream, HEARTBEAT_INTERVAL, &proxy_config.crypto.clone()).await
                    {
                        Ok(()) => (),
                        Err(e) => {
                            warn!(?e, %addr, "Stream pool failed to send noop heartbeat");
                            // Drop the stream
                            cell.take();
                            counter!("stream_pool.dead_streams").increment(1);
                            return Err(e);
                        }
                    }
                }
                Result::<(), HeartbeatError>::Ok(())
            }
        });
        Ok(Self {
            cell,
            _task_handle: task_handle,
        })
    }

    pub fn try_take(&self) -> TryTake {
        let mut cell = match self.cell.try_write() {
            Ok(x) => x,
            Err(_) => return TryTake::Occupied,
        };
        match cell.take() {
            Some(stream) => {
                counter!("stream_pool.taken_streams").increment(1);
                TryTake::Ok(stream)
            }
            None => TryTake::Killed,
        }
    }
}

enum TryTake {
    Ok(CreatedStream),
    Occupied,
    Killed,
}

#[derive(Debug)]
struct SocketQueue {
    queue: Arc<RwLock<VecDeque<SocketCell>>>,
    task_handles: JoinSet<()>,
    proxy_config: ProxyConfig<ConcreteStreamAddr>,
}

impl SocketQueue {
    pub fn new(proxy_config: ProxyConfig<ConcreteStreamAddr>) -> Self {
        Self {
            queue: Default::default(),
            task_handles: Default::default(),
            proxy_config,
        }
    }

    async fn insert(
        queue: Arc<RwLock<VecDeque<SocketCell>>>,
        proxy_config: ProxyConfig<ConcreteStreamAddr>,
        heartbeat_interval: Duration,
    ) -> io::Result<()> {
        let cell = SocketCell::create(proxy_config, heartbeat_interval).await?;
        let mut queue = queue.write().unwrap();
        queue.push_back(cell);
        Ok(())
    }

    pub fn spawn_insert(&mut self, heartbeat_interval: Duration) {
        let queue = self.queue.clone();
        let proxy_config = self.proxy_config.clone();
        self.task_handles.spawn(async move {
            loop {
                let queue = queue.clone();
                match Self::insert(queue, proxy_config.clone(), heartbeat_interval).await {
                    Ok(()) => break,
                    Err(_e) => {
                        tokio::time::sleep(RETRY_INTERVAL).await;
                    }
                }
            }
        });
    }

    pub fn try_swap(&mut self, heartbeat_interval: Duration) -> Option<CreatedStream> {
        let res = {
            let mut queue = match self.queue.try_write() {
                Ok(x) => x,
                // The queue is being occupied
                Err(_) => return None,
            };
            let front = match queue.pop_front() {
                Some(x) => x,
                // The queue is empty
                None => return None,
            };
            match front.try_take() {
                TryTake::Ok(tcp) => Some(tcp),
                TryTake::Occupied => {
                    queue.push_back(front);
                    return None;
                }
                TryTake::Killed => None,
            }
        };

        // Replenish
        self.spawn_insert(heartbeat_interval);

        res
    }
}

#[derive(Debug, Clone)]
pub struct Pool {
    pool: Arc<ArcSwap<PoolInner>>,
}

impl Pool {
    pub fn empty() -> Self {
        Self {
            pool: Arc::new(Arc::new(PoolInner::empty()).into()),
        }
    }

    pub fn new<I>(proxy_configs: I) -> Self
    where
        I: Iterator<Item = ProxyConfig<ConcreteStreamAddr>>,
    {
        Self {
            pool: Arc::new(Arc::new(PoolInner::new(proxy_configs)).into()),
        }
    }

    pub async fn open_stream(
        &self,
        addr: &ConcreteStreamAddr,
    ) -> Option<(CreatedStream, SocketAddr)> {
        self.pool.load().open_stream(addr).await
    }

    pub fn replaced_by(&self, new: Self) {
        self.pool.store(new.pool.load_full());
    }
}

#[derive(Debug)]
struct PoolInner {
    pool: HashMap<ConcreteStreamAddr, Mutex<SocketQueue>>,
}

impl PoolInner {
    pub fn empty() -> Self {
        Self {
            pool: Default::default(),
        }
    }

    pub fn new<I>(proxy_configs: I) -> Self
    where
        I: Iterator<Item = ProxyConfig<ConcreteStreamAddr>>,
    {
        let mut pool = HashMap::new();
        proxy_configs.for_each(|c| {
            let addr = c.address.clone();
            let mut queue = SocketQueue::new(c);
            for _ in 0..QUEUE_LEN {
                queue.spawn_insert(HEARTBEAT_INTERVAL);
            }
            pool.insert(addr, Mutex::new(queue));
        });

        Self { pool }
    }

    pub async fn open_stream(
        &self,
        addr: &ConcreteStreamAddr,
    ) -> Option<(CreatedStream, SocketAddr)> {
        let stream = {
            let mut queue = match self.pool.get(addr).and_then(|queue| queue.try_lock().ok()) {
                Some(queue) => queue,
                None => return None,
            };
            queue.try_swap(HEARTBEAT_INTERVAL)
        };
        let stream = match stream {
            Some(x) => x,
            None => return None,
        };
        let peer_addr = match stream.peer_addr() {
            Ok(x) => x,
            Err(_) => return None,
        };
        counter!("stream_pool.opened_streams").increment(1);
        Some((stream, peer_addr))
    }
}

pub async fn connect_with_pool(
    addr: &ConcreteStreamAddr,
    stream_pool: &Pool,
    allow_loopback: bool,
    timeout: Duration,
) -> Result<(CreatedStream, SocketAddr), ConnectError> {
    let stream = stream_pool.open_stream(addr).await;
    let ret = match stream {
        Some((stream, sock_addr)) => (stream, sock_addr),
        None => {
            let sock_addr =
                addr.address
                    .to_socket_addr()
                    .await
                    .map_err(|e| ConnectError::ResolveAddr {
                        source: e,
                        addr: addr.clone(),
                    })?;
            if !allow_loopback && sock_addr.ip().is_loopback() {
                // Prevent connections to localhost
                return Err(ConnectError::Loopback {
                    addr: addr.clone(),
                    sock_addr,
                });
            }
            let stream = STREAM_CONNECTOR_TABLE
                .timed_connect(addr.stream_type, sock_addr, timeout)
                .await
                .map_err(|e| ConnectError::ConnectAddr {
                    source: e,
                    addr: addr.clone(),
                    sock_addr,
                })?;
            (stream, sock_addr)
        }
    };
    Ok(ret)
}

#[derive(Debug, Error)]
pub enum ConnectError {
    #[error("Failed to resolve address: {source}, {addr}")]
    ResolveAddr {
        #[source]
        source: io::Error,
        addr: ConcreteStreamAddr,
    },
    #[error("Refused to connect to loopback address: {addr}, {sock_addr}")]
    Loopback {
        addr: ConcreteStreamAddr,
        sock_addr: SocketAddr,
    },
    #[error("Failed to connect to address: {source}, {addr}, {sock_addr}")]
    ConnectAddr {
        #[source]
        source: io::Error,
        addr: ConcreteStreamAddr,
        sock_addr: SocketAddr,
    },
}

#[cfg(test)]
mod tests {
    use tokio::{io::AsyncReadExt, net::TcpListener, task::JoinSet};

    use crate::stream::{addr::StreamAddr, concrete::addr::ConcreteStreamType};

    use super::*;

    async fn spawn_listener() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    let mut buf = [0; 1024];
                    loop {
                        if let Err(_e) = stream.read_exact(&mut buf).await {
                            break;
                        }
                    }
                });
            }
        });
        addr
    }

    fn create_random_crypto() -> tokio_chacha20::config::Config {
        let key: [u8; 32] = rand::random();
        tokio_chacha20::config::Config::new(key.into())
    }

    #[tokio::test]
    async fn take_none() {
        let addr = "0.0.0.0:0".parse::<SocketAddr>().unwrap();
        let addr = StreamAddr {
            address: addr.into(),
            stream_type: ConcreteStreamType::Tcp,
        };
        let proxy_config = ProxyConfig {
            address: addr.clone(),
            crypto: create_random_crypto(),
        };
        let pool = Pool::new(vec![proxy_config].into_iter());
        let mut join_set = JoinSet::new();
        for _ in 0..100 {
            let pool = pool.clone();
            let addr = addr.clone();
            join_set.spawn(async move {
                let res = pool.open_stream(&addr).await;
                assert!(res.is_none());
            });
        }
    }

    #[tokio::test]
    async fn take_some() {
        let addr = spawn_listener().await;
        let addr = StreamAddr {
            address: addr.into(),
            stream_type: ConcreteStreamType::Tcp,
        };
        let proxy_config = ProxyConfig {
            address: addr.clone(),
            crypto: create_random_crypto(),
        };
        let pool = Pool::new(vec![proxy_config].into_iter());
        for _ in 0..10 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            for _ in 0..QUEUE_LEN {
                let addr = addr.clone();
                let res = pool.open_stream(&addr).await;
                assert!(res.is_some());
            }
        }
    }
}
