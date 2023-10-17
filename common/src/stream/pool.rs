use std::{
    collections::{HashMap, VecDeque},
    io,
    net::SocketAddr,
    ops::DerefMut,
    sync::{Arc, RwLock},
    time::Duration,
};

use metrics::counter;
use serde::{Deserialize, Serialize};
use tokio::{sync::RwLock as TokioRwLock, task::JoinSet};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::{
    addr::{InternetAddr, ParseInternetAddrError},
    header::heartbeat::{send_noop, HeartbeatError},
    stream::CreatedStream,
};

use super::{
    addr::StreamAddrStr,
    connect::{ArcStreamConnect, STREAM_CONNECTOR_TABLE},
    IoAddr, StreamAddr,
};

const QUEUE_LEN: usize = 16;
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
const RETRY_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoolBuilder(#[serde(default)] pub Vec<StreamAddrStr>);

impl PoolBuilder {
    pub fn new() -> Self {
        Self(vec![])
    }

    pub fn build(self, cancellation: CancellationToken) -> Result<Pool, ParseInternetAddrError> {
        let pool = Pool::new(self.0.into_iter().map(|a| a.0), cancellation);
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
        connector: &ArcStreamConnect,
        addr: InternetAddr,
        heartbeat_interval: Duration,
    ) -> io::Result<Self> {
        let sock_addr = addr.to_socket_addr().await?;
        let stream = connector
            .timed_connect(sock_addr, HEARTBEAT_INTERVAL)
            .await?;
        let cell = Arc::new(TokioRwLock::new(Some(stream)));
        let mut task_handle = JoinSet::new();
        counter!("stream_pool.put_streams", 1);
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
                    match send_noop(stream, HEARTBEAT_INTERVAL).await {
                        Ok(()) => (),
                        Err(e) => {
                            warn!(?e, %addr, "Stream pool failed to send noop heartbeat");
                            // Drop the stream
                            cell.take();
                            counter!("stream_pool.dead_streams", 1);
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
                counter!("stream_pool.taken_streams", 1);
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
}

impl SocketQueue {
    pub fn new() -> Self {
        Self {
            queue: Default::default(),
            task_handles: Default::default(),
        }
    }

    async fn insert(
        queue: Arc<RwLock<VecDeque<SocketCell>>>,
        connector: &ArcStreamConnect,
        addr: InternetAddr,
        heartbeat_interval: Duration,
    ) -> io::Result<()> {
        let cell = SocketCell::create(connector, addr, heartbeat_interval).await?;
        let mut queue = queue.write().unwrap();
        queue.push_back(cell);
        Ok(())
    }

    pub fn spawn_insert(
        &mut self,
        connector: ArcStreamConnect,
        addr: InternetAddr,
        heartbeat_interval: Duration,
    ) {
        let queue = self.queue.clone();
        self.task_handles.spawn(async move {
            loop {
                let queue = queue.clone();
                match Self::insert(queue, &connector, addr.clone(), heartbeat_interval).await {
                    Ok(()) => break,
                    Err(_e) => {
                        tokio::time::sleep(RETRY_INTERVAL).await;
                    }
                }
            }
        });
    }

    pub fn try_swap(
        &mut self,
        connector: ArcStreamConnect,
        addr: &InternetAddr,
        heartbeat_interval: Duration,
    ) -> Option<CreatedStream> {
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
        self.spawn_insert(connector, addr.clone(), heartbeat_interval);

        res
    }
}

#[derive(Debug, Clone)]
pub struct Pool {
    pool: Arc<RwLock<HashMap<StreamAddr, SocketQueue>>>,
    _task_handle: Arc<JoinSet<()>>,
}

impl Pool {
    pub fn empty() -> Self {
        Self {
            pool: Default::default(),
            _task_handle: Default::default(),
        }
    }

    pub fn new<I>(addrs: I, cancellation: CancellationToken) -> Self
    where
        I: Iterator<Item = StreamAddr>,
    {
        fn add_queue(pool: &mut HashMap<StreamAddr, SocketQueue>, addr: StreamAddr) {
            let mut queue = SocketQueue::new();
            let connector = {
                let connector = STREAM_CONNECTOR_TABLE.get(addr.stream_type);
                Arc::clone(connector)
            };
            for _ in 0..QUEUE_LEN {
                queue.spawn_insert(
                    Arc::clone(&connector),
                    addr.address.clone(),
                    HEARTBEAT_INTERVAL,
                );
            }
            pool.insert(addr, queue);
        }

        let mut pool: HashMap<StreamAddr, SocketQueue> = HashMap::new();
        addrs.for_each(|addr| add_queue(&mut pool, addr));
        let pool = Arc::new(RwLock::new(pool));

        let mut task_handle = JoinSet::new();
        task_handle.spawn({
            let pool = Arc::clone(&pool);
            async move {
                tokio::select! {
                    _ = cancellation.cancelled() => {
                        let mut pool = pool.write().unwrap();
                        pool.clear();
                    }
                }
            }
        });

        Self {
            pool,
            _task_handle: Arc::new(task_handle),
        }
    }

    pub async fn open_stream(&self, addr: &StreamAddr) -> Option<(CreatedStream, SocketAddr)> {
        let stream = {
            let mut pool = match self.pool.try_write() {
                Ok(x) => x,
                Err(_) => return None,
            };
            let queue = match pool.get_mut(addr) {
                Some(x) => x,
                None => return None,
            };
            let connector = {
                let connector = STREAM_CONNECTOR_TABLE.get(addr.stream_type);
                Arc::clone(connector)
            };
            queue.try_swap(connector, &addr.address, HEARTBEAT_INTERVAL)
        };
        let stream = match stream {
            Some(x) => x,
            None => return None,
        };
        let peer_addr = match stream.peer_addr() {
            Ok(x) => x,
            Err(_) => return None,
        };
        counter!("stream_pool.opened_streams", 1);
        Some((stream, peer_addr))
    }
}

#[cfg(test)]
mod tests {
    use tokio::{io::AsyncReadExt, net::TcpListener, task::JoinSet};

    use crate::stream::addr::StreamType;

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

    #[tokio::test]
    async fn take_none() {
        let addr = "0.0.0.0:0".parse::<SocketAddr>().unwrap();
        let addr = StreamAddr {
            address: addr.into(),
            stream_type: StreamType::Tcp,
        };
        let pool = Pool::new(vec![addr.clone()].into_iter(), CancellationToken::new());
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
            stream_type: StreamType::Tcp,
        };
        let pool = Pool::new(vec![addr.clone()].into_iter(), CancellationToken::new());
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
