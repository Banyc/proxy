use std::{
    collections::{HashMap, VecDeque},
    io,
    net::SocketAddr,
    ops::DerefMut,
    sync::{Arc, RwLock},
    time::Duration,
};

use tokio::net::TcpStream;
use tokio::sync::RwLock as TokioRwLock;

use crate::{
    error::ProxyProtocolError, header::InternetAddr, heartbeat::send_noop, stream::CreatedStream,
};

const QUEUE_LEN: usize = 16;
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
const RETRY_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
struct SocketCell {
    cell: Arc<TokioRwLock<Option<TcpStream>>>,
}

impl SocketCell {
    pub async fn create(addr: &InternetAddr, heartbeat_interval: Duration) -> io::Result<Self> {
        let tcp = TcpStream::connect(addr.to_socket_addr().await?).await?;
        let cell = Arc::new(TokioRwLock::new(Some(tcp)));
        tokio::spawn({
            let cell = Arc::clone(&cell);
            async move {
                loop {
                    tokio::time::sleep(heartbeat_interval).await;
                    let mut cell = cell.write().await;
                    let tcp = match cell.deref_mut() {
                        Some(x) => x,
                        None => break,
                    };
                    match send_noop(tcp).await {
                        Ok(()) => (),
                        Err(e) => {
                            // Drop the stream
                            cell.take();
                            return Err(e);
                        }
                    }
                }
                Result::<(), ProxyProtocolError>::Ok(())
            }
        });
        Ok(Self { cell })
    }

    pub fn try_take(&self) -> TryTake {
        let mut cell = match self.cell.try_write() {
            Ok(x) => x,
            Err(_) => return TryTake::Occupied,
        };
        match cell.take() {
            Some(tcp) => TryTake::Ok(tcp),
            None => TryTake::Killed,
        }
    }
}

enum TryTake {
    Ok(TcpStream),
    Occupied,
    Killed,
}

#[derive(Debug, Clone)]
struct SocketQueue {
    queue: Arc<RwLock<VecDeque<SocketCell>>>,
}

impl SocketQueue {
    pub fn new() -> Self {
        Self {
            queue: Default::default(),
        }
    }

    pub async fn insert(
        &self,
        addr: &InternetAddr,
        heartbeat_interval: Duration,
    ) -> io::Result<()> {
        let cell = SocketCell::create(addr, heartbeat_interval).await?;
        let mut queue = self.queue.write().unwrap();
        queue.push_back(cell);
        Ok(())
    }

    pub fn spawn_insert(&self, addr: InternetAddr, heartbeat_interval: Duration) {
        let this = self.clone();
        tokio::spawn(async move {
            loop {
                match this.insert(&addr, heartbeat_interval).await {
                    Ok(()) => break,
                    Err(_e) => {
                        tokio::time::sleep(RETRY_INTERVAL).await;
                    }
                }
            }
        });
    }

    pub fn try_swap(&self, addr: &InternetAddr, heartbeat_interval: Duration) -> Option<TcpStream> {
        let front = {
            let mut queue = match self.queue.try_write() {
                Ok(x) => x,
                // The queue is being occupied
                Err(_) => return None,
            };
            match queue.pop_front() {
                Some(x) => x,
                // The queue is empty
                None => return None,
            }
        };
        let res = match front.try_take() {
            TryTake::Ok(tcp) => Some(tcp),
            TryTake::Occupied => return None,
            TryTake::Killed => None,
        };

        // Replenish
        self.spawn_insert(addr.clone(), heartbeat_interval);

        res
    }
}

#[derive(Debug, Clone)]
pub struct TcpPool {
    pool: Arc<RwLock<HashMap<InternetAddr, SocketQueue>>>,
}

impl TcpPool {
    pub fn new() -> Self {
        Self {
            pool: Default::default(),
        }
    }

    pub fn add_many_queues<I>(&self, addrs: I)
    where
        I: Iterator<Item = InternetAddr>,
    {
        for addr in addrs {
            self.add_queue(addr);
        }
    }

    pub fn add_queue(&self, addr: InternetAddr) {
        let queue = SocketQueue::new();
        for _ in 0..QUEUE_LEN {
            queue.spawn_insert(addr.clone(), HEARTBEAT_INTERVAL);
        }
        self.pool.write().unwrap().insert(addr, queue);
    }

    pub async fn open_stream(&self, addr: &InternetAddr) -> Option<(CreatedStream, SocketAddr)> {
        let tcp = {
            let pool = match self.pool.try_read() {
                Ok(x) => x,
                Err(_) => return None,
            };
            let queue = match pool.get(addr) {
                Some(x) => x,
                None => return None,
            };
            queue.try_swap(addr, HEARTBEAT_INTERVAL)
        };
        let tcp = match tcp {
            Some(x) => x,
            None => return None,
        };
        let peer_addr = match tcp.peer_addr() {
            Ok(x) => x,
            Err(_) => return None,
        };
        Some((CreatedStream::Tcp(tcp), peer_addr))
    }
}

impl Default for TcpPool {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use tokio::{io::AsyncReadExt, net::TcpListener, task::JoinSet};

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
        let pool = TcpPool::new();
        let addr = "0.0.0.0:0".parse::<SocketAddr>().unwrap();
        pool.add_queue(addr.into());
        let mut join_set = JoinSet::new();
        for _ in 0..100 {
            let pool = pool.clone();
            join_set.spawn(async move {
                let res = pool.open_stream(&addr.into()).await;
                assert!(res.is_none());
            });
        }
    }

    #[tokio::test]
    async fn take_some() {
        let pool = TcpPool::new();
        let addr = spawn_listener().await;
        pool.add_queue(addr.into());
        for _ in 0..10 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            for _ in 0..QUEUE_LEN {
                let res = pool.open_stream(&addr.into()).await;
                assert!(res.is_some());
            }
        }
    }
}
