//! Experimental LSM Electrum TCP service. Blocking state lives on ONE worker.
mod protocol;
use super::{
    mempool::{Budget, Mempool, Refresh},
    sync::{self, Options as SyncOptions},
    BitcoinStore, IndexLimits,
};
use crate::{rpc::Core, Chain};
use anyhow::{ensure, Result};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    net::SocketAddr,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, RwLock,
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::{oneshot, Semaphore},
    task::JoinSet,
    time,
};

const MAX_FRAME: usize = 8_100_000;
const MAX_REPLY: usize = 16_200_000;
const QUEUE: usize = 64;
#[derive(Clone)]
pub struct Options {
    pub chain: Chain,
    pub root: PathBuf,
    pub core: Core,
    pub listen: SocketAddr,
    pub connections: usize,
    pub subscriptions: usize,
    pub query_limit: usize,
    pub index_limits: IndexLimits,
    pub mempool_budget: Budget,
    pub poll: Duration,
    pub min_free_bytes: u64,
}
#[derive(Clone, Debug)]
struct Fault(i64, &'static str);
impl std::fmt::Display for Fault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.1)
    }
}
impl std::error::Error for Fault {}
type Reply = std::result::Result<Value, Fault>;
fn sanitize(error: anyhow::Error) -> Fault {
    error.downcast_ref::<Fault>().cloned().unwrap_or(Fault(
        1,
        "LSM query unavailable or configured response limit exceeded",
    ))
}
struct Request {
    method: String,
    params: Value,
    reply: oneshot::Sender<Reply>,
}
struct Health {
    data: Value,
    updated: Instant,
    max_age: Duration,
}
impl Health {
    fn value(&self) -> Value {
        let mut data = self.data.clone();
        data["observation_age_ms"] = json!(self.updated.elapsed().as_millis());
        if self.updated.elapsed() >= self.max_age {
            data["ready"] = json!(false);
        }
        data
    }
}
struct Worker {
    options: Options,
    store: BitcoinStore,
    pool: Mempool,
    health: Arc<RwLock<Health>>,
    stop: Arc<AtomicBool>,
}
impl Worker {
    fn new(options: Options, health: Arc<RwLock<Health>>, stop: Arc<AtomicBool>) -> Result<Self> {
        let store = if options.root.exists() {
            BitcoinStore::open_with_limits(&options.root, options.chain, options.index_limits)?
        } else {
            BitcoinStore::create_with_limits(&options.root, options.chain, options.index_limits)?
        };
        Ok(Self {
            options,
            store,
            pool: Mempool::default(),
            health,
            stop,
        })
    }
    fn publish(
        &self,
        ready: bool,
        core_height: Option<u32>,
        pool_count: Option<usize>,
        error: Option<&'static str>,
    ) {
        let snapshot = self.store.snapshot().ok();
        let tip = snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.state().tip.as_ref());
        let data = json!({"backend":"lsm","network":self.options.chain.name(),"ready":ready,"generation":snapshot.as_ref().map(|snapshot|snapshot.generation()),"indexed_tip":tip.map(|tip|json!({"height":tip.height,"hash":tip.hash})),"counts":snapshot.as_ref().and_then(|snapshot|snapshot.counts().ok()),"core_height":core_height,"mempool_transactions":pool_count,"last_error":error});
        if let Ok(mut health) = self.health.write() {
            *health = Health {
                data,
                updated: Instant::now(),
                max_age: self.options.mempool_budget.max_age,
            };
        }
    }
    fn refresh(&mut self) -> bool {
        self.pool.invalidate();
        if let Ok(mut health) = self.health.write() {
            health.data["ready"] = json!(false);
        }
        if !self.store.ready && self.store.recover().is_err() {
            self.publish(false, None, None, Some("LSM recovery required"));
            return false;
        }
        let options = SyncOptions {
            max_blocks: 8,
            min_free_bytes: self.options.min_free_bytes,
            ..SyncOptions::default()
        };
        let outcome = match sync::cycle(&mut self.store, &self.options.core, &options, &self.stop) {
            Ok(value) => value,
            Err(error) => {
                let description = error.to_string();
                let diagnostic = if description.contains("count limit") {
                    "LSM cumulative index limit reached"
                } else if description.contains("pruned") {
                    "required historical blocks are unavailable"
                } else if description.contains("undo") {
                    "deep reorg requires recovery into a new root"
                } else {
                    "Core or LSM synchronization unavailable"
                };
                self.publish(false, None, None, Some(diagnostic));
                return false;
            }
        };
        if !outcome.confirmed_caught_up {
            self.publish(
                false,
                Some(outcome.observed_height),
                None,
                Some("confirmed index catching up"),
            );
            return false;
        }
        let result = self.store.snapshot().and_then(|snapshot| {
            self.pool.reconcile(
                &snapshot,
                &self.options.core,
                self.options.mempool_budget,
                &self.stop,
            )
        });
        match result {
            Ok(Refresh::Ready { transactions, .. }) => {
                self.publish(
                    true,
                    Some(outcome.observed_height),
                    Some(transactions),
                    None,
                );
                true
            }
            Ok(Refresh::Warming { .. }) => {
                self.publish(
                    false,
                    Some(outcome.observed_height),
                    None,
                    Some("complete mempool snapshot warming"),
                );
                false
            }
            Err(_) => {
                self.publish(
                    false,
                    Some(outcome.observed_height),
                    None,
                    Some("mempool unavailable, changed or over budget"),
                );
                false
            }
        }
    }
    fn call(&mut self, method: &str, params: &Value) -> Reply {
        if !self.refresh() {
            return Err(Fault(
                -32000,
                "LSM not ready: caught-up confirmed state and complete mempool required",
            ));
        }
        self.dispatch(method, params).map_err(sanitize)
    }
}

async fn ask(sender: &mpsc::SyncSender<Request>, method: &str, params: Value) -> Reply {
    let (reply, receiver) = oneshot::channel();
    sender
        .try_send(Request {
            method: method.to_owned(),
            params,
            reply,
        })
        .map_err(|error| match error {
            mpsc::TrySendError::Full(_) => Fault(-32001, "LSM worker queue full"),
            mpsc::TrySendError::Disconnected(_) => Fault(-32001, "LSM worker unavailable"),
        })?;
    time::timeout(Duration::from_secs(30), receiver)
        .await
        .map_err(|_| Fault(-32002, "LSM worker request timed out"))?
        .map_err(|_| Fault(-32001, "LSM worker stopped"))?
}
fn response(id: Value, result: Reply) -> Value {
    match result {
        Ok(value) => json!({"jsonrpc":"2.0","id":id,"result":value}),
        Err(error) => json!({"jsonrpc":"2.0","id":id,"error":{"code":error.0,"message":error.1}}),
    }
}
#[derive(Default)]
struct Connection {
    scripts: BTreeMap<String, Value>,
    header: Option<Value>,
    last_tip: Option<String>,
}
impl Connection {
    async fn one(
        &mut self,
        request: &Value,
        sender: &mpsc::SyncSender<Request>,
        health: &Arc<RwLock<Health>>,
        options: &Options,
    ) -> Option<Value> {
        let Some(object) = request.as_object() else {
            return Some(response(Value::Null, Err(Fault(-32600, "invalid request"))));
        };
        let id = object.get("id").cloned();
        if id
            .as_ref()
            .is_some_and(|id| !id.is_null() && !id.is_string() && !id.is_number())
            || object
                .get("jsonrpc")
                .is_some_and(|version| version != "2.0")
        {
            return Some(response(Value::Null, Err(Fault(-32600, "invalid request"))));
        }
        let Some(method) = object
            .get("method")
            .and_then(Value::as_str)
            .filter(|method| method.len() <= 128)
        else {
            return Some(response(
                id.unwrap_or(Value::Null),
                Err(Fault(-32600, "invalid request")),
            ));
        };
        let empty = json!([]);
        let params = object.get("params").unwrap_or(&empty);
        let result = match protocol::validate(method, params) {
            Err(error) => Err(sanitize(error)),
            Ok(()) => match method {
                "server.health" => health
                    .read()
                    .map(|health| health.value())
                    .map_err(|_| Fault(1, "diagnostics unavailable")),
                "server.version" => protocol::version(params).map_err(sanitize),
                "server.features" => {
                    let host = options.listen.ip().to_string();
                    Ok(
                        json!({"server_version":"electrum-lite 0.0.1-beta.1 LSM experimental","protocol_min":"1.4","protocol_max":"1.4","genesis_hash":options.chain.genesis(),"hash_function":"sha256","pruning":null,"hosts":{host:{"tcp_port":options.listen.port()}}}),
                    )
                }
                "server.ping" => Ok(Value::Null),
                "server.banner" => Ok(json!(
                    "Experimental private LSM server; not production validated"
                )),
                "server.donation_address" => Ok(json!("")),
                "server.peers.subscribe" => Ok(json!([])),
                "server.add_peer" => Ok(json!(false)),
                "blockchain.scripthash.unsubscribe" => protocol::wire_script(params)
                    .map(|(wire, _)| json!(self.scripts.remove(&wire).is_some()))
                    .map_err(sanitize),
                "blockchain.scripthash.subscribe" => match protocol::wire_script(params) {
                    Err(error) => Err(sanitize(error)),
                    Ok((wire, _)) => {
                        if !self.scripts.contains_key(&wire)
                            && self.scripts.len() + usize::from(self.header.is_some())
                                >= options.subscriptions
                        {
                            Err(Fault(1, "subscription limit exceeded"))
                        } else {
                            ask(sender, "__subscribe", params.clone())
                                .await
                                .and_then(|result| {
                                    let tip = result["tip"].as_str().ok_or(Fault(
                                        1,
                                        "invalid internal subscription snapshot",
                                    ))?;
                                    // Seed at subscribe, not first poll: a same-status
                                    // reorg can happen before that first poll arrives.
                                    self.last_tip.get_or_insert_with(|| tip.to_owned());
                                    let value = result["status"].clone();
                                    self.scripts.insert(wire, value.clone());
                                    Ok(value)
                                })
                        }
                    }
                },
                "blockchain.headers.subscribe" => {
                    if self.header.is_none() && self.scripts.len() >= options.subscriptions {
                        Err(Fault(1, "subscription limit exceeded"))
                    } else {
                        let result = ask(sender, method, params.clone()).await;
                        if let Ok(value) = &result {
                            self.header = Some(value.clone());
                        }
                        result
                    }
                }
                _ => ask(sender, method, params.clone()).await,
            },
        };
        id.map(|id| response(id, result)) // valid notifications execute, but never reply
    }
    async fn frame(
        &mut self,
        value: Value,
        sender: &mpsc::SyncSender<Request>,
        health: &Arc<RwLock<Health>>,
        options: &Options,
    ) -> Option<Value> {
        if let Some(requests) = value.as_array() {
            if requests.is_empty() || requests.len() > 32 {
                return Some(response(
                    Value::Null,
                    Err(Fault(-32600, "batch must contain 1..32 requests")),
                ));
            }
            let mut replies = Vec::new();
            let mut bytes = 2;
            for request in requests {
                if let Some(reply) = self.one(request, sender, health, options).await {
                    bytes += serde_json::to_vec(&reply).ok()?.len() + 1;
                    if bytes > MAX_REPLY {
                        return Some(response(
                            Value::Null,
                            Err(Fault(1, "batch response byte limit exceeded")),
                        ));
                    }
                    replies.push(reply);
                }
            }
            if replies.is_empty() {
                None
            } else {
                Some(Value::Array(replies))
            }
        } else {
            self.one(&value, sender, health, options).await
        }
    }
    async fn notifications(&mut self, sender: &mpsc::SyncSender<Request>) -> Vec<Value> {
        if self.header.is_none() && self.scripts.is_empty() {
            return Vec::new();
        }
        let Ok(value) = ask(
            sender,
            "__poll",
            json!(self.scripts.keys().collect::<Vec<_>>()),
        )
        .await
        else {
            return Vec::new();
        };
        let tip = value["tip"].as_str().unwrap_or_default().to_owned();
        let force = self
            .last_tip
            .as_ref()
            .is_some_and(|previous| previous != &tip);
        self.last_tip = Some(tip);
        let mut result = Vec::new();
        if let Some(previous) = &mut self.header {
            if *previous != value["header"] {
                *previous = value["header"].clone();
                result.push(json!({"jsonrpc":"2.0","method":"blockchain.headers.subscribe","params":[previous]}));
            }
        }
        for (wire, previous) in &mut self.scripts {
            let current = &value["statuses"][wire];
            if force || *previous != *current {
                *previous = current.clone();
                result.push(json!({"jsonrpc":"2.0","method":"blockchain.scripthash.subscribe","params":[wire,current]}));
            }
        }
        result
    }
}

// pending lives OUTSIDE the future so a timer cancelling read_frame cannot
// discard a fragmented JSON request. No read_until allocation without a bound.
async fn read_frame(
    reader: &mut BufReader<TcpStream>,
    pending: &mut Vec<u8>,
) -> Result<Option<Vec<u8>>> {
    loop {
        let buffer = reader.fill_buf().await?;
        if buffer.is_empty() {
            ensure!(pending.is_empty(), "EOF in fragmented frame");
            return Ok(None);
        }
        let length = buffer
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|n| n + 1)
            .unwrap_or(buffer.len());
        ensure!(
            pending.len() + length <= MAX_FRAME,
            "request frame exceeds limit"
        );
        let complete = buffer[length - 1] == b'\n';
        pending.extend_from_slice(&buffer[..length]);
        reader.consume(length);
        if complete {
            return Ok(Some(std::mem::take(pending)));
        }
    }
}
async fn write_value(reader: &mut BufReader<TcpStream>, value: &Value) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    ensure!(bytes.len() <= MAX_REPLY, "response exceeds byte limit");
    bytes.push(b'\n');
    time::timeout(Duration::from_secs(15), reader.get_mut().write_all(&bytes)).await??;
    Ok(())
}
async fn connection(
    stream: TcpStream,
    sender: mpsc::SyncSender<Request>,
    health: Arc<RwLock<Health>>,
    options: Options,
) -> Result<()> {
    stream.set_nodelay(true)?;
    let mut reader = BufReader::new(stream);
    let mut pending = Vec::new();
    let mut state = Connection::default();
    let mut poll = time::interval(options.poll);
    poll.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let mut last_frame = Instant::now();
    loop {
        tokio::select! {
            frame=read_frame(&mut reader,&mut pending)=>{let Some(frame)=frame? else{return Ok(());};last_frame=Instant::now();
                let reply=match serde_json::from_slice::<Value>(&frame){Ok(value)=>state.frame(value,&sender,&health,&options).await,Err(_)=>Some(response(Value::Null,Err(Fault(-32700,"parse error"))))};
                if let Some(reply)=reply {write_value(&mut reader,&reply).await?;}
            },
            _=poll.tick()=>{ensure!(last_frame.elapsed()<Duration::from_secs(120),"idle connection timeout");for value in state.notifications(&sender).await {write_value(&mut reader,&value).await?;}}
        }
    }
}

pub async fn run(options: Options) -> Result<()> {
    options.mempool_budget.validate()?;
    ensure!(
        options.listen.ip().is_loopback() && options.listen.port() != 0,
        "LSM service requires explicit nonzero loopback listen address"
    );
    ensure!(
        (1..=128).contains(&options.connections)
            && (1..=256).contains(&options.subscriptions)
            && (1..=100_000).contains(&options.query_limit),
        "LSM transport/query budget outside supported scope"
    );
    ensure!(
        options.poll >= Duration::from_millis(100) && options.poll <= Duration::from_secs(5),
        "LSM poll interval must be 100..5000 ms"
    );
    let listener = TcpListener::bind(options.listen).await?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let health = Arc::new(RwLock::new(Health {
        data: json!({"backend":"lsm","network":options.chain.name(),"ready":false,"last_error":"LSM startup verification"}),
        updated: Instant::now(),
        max_age: options.mempool_budget.max_age,
    }));
    let stop = Arc::new(AtomicBool::new(false));
    let (sender, receiver) = mpsc::sync_channel::<Request>(QUEUE);
    let (terminal, mut terminated) = oneshot::channel();
    let worker_options = options.clone();
    let worker_health = health.clone();
    let worker_stop = stop.clone();
    let worker = std::thread::Builder::new()
        .name("lsm-core-writer".into())
        .spawn(move || {
            let result = (|| -> Result<()> {
                let mut state = Worker::new(worker_options, worker_health, worker_stop.clone())?;
                state.refresh();
                while !worker_stop.load(Ordering::Relaxed) {
                    match receiver.recv_timeout(state.options.poll) {
                        Ok(request) => {
                            if !request.reply.is_closed() {
                                let value = state.call(&request.method, &request.params);
                                let _ = request.reply.send(value);
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            state.refresh();
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
                Ok(())
            })();
            let _ = terminal.send(result.map_err(|_| ()));
        })?;
    let permits = Arc::new(Semaphore::new(options.connections));
    let mut tasks = JoinSet::new();
    let ctrl = tokio::signal::ctrl_c();
    tokio::pin!(ctrl);
    let mut failed = false;
    loop {
        tokio::select! {
            _=&mut ctrl=>break,_=terminate.recv()=>break,
            result=&mut terminated=>{failed = !matches!(result,Ok(Ok(())));break;},
            accepted=listener.accept()=>{let (stream,_)=match accepted {Ok(value)=>value,Err(_)=>{failed=true;break;}};if let Ok(permit)=permits.clone().try_acquire_owned(){let sender=sender.clone();let health=health.clone();let options=options.clone();tasks.spawn(async move {let _permit=permit;let _=connection(stream,sender,health,options).await;});}},
            _=tasks.join_next(),if !tasks.is_empty()=>{}
        }
    }
    stop.store(true, Ordering::Relaxed);
    drop(sender);
    tasks.shutdown().await;
    tokio::task::spawn_blocking(move || worker.join())
        .await?
        .map_err(|_| anyhow::anyhow!("LSM worker panicked"))?;
    ensure!(!failed, "LSM startup or worker verification failed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn public_errors_never_echo_underlying_credentials_or_wallet_data() {
        let error = sanitize(anyhow::anyhow!("cookie SECRET wallet-identifying-script"));
        assert_eq!(error.0, 1);
        assert!(!error.1.contains("SECRET"));
        assert!(!error.1.contains("wallet-identifying"));
    }
    #[tokio::test]
    async fn fragmented_frame_survives_cancelled_read() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let client = tokio::spawn(async move {
            let mut stream = TcpStream::connect(address).await?;
            stream.write_all(b"{\"id\":1,").await?;
            time::sleep(Duration::from_millis(100)).await;
            stream.write_all(b"\"method\":\"server.ping\"}\n").await?;
            Ok::<_, std::io::Error>(())
        });
        let (stream, _) = listener.accept().await?;
        let mut reader = BufReader::new(stream);
        let mut pending = Vec::new();
        assert!(time::timeout(
            Duration::from_millis(20),
            read_frame(&mut reader, &mut pending)
        )
        .await
        .is_err());
        assert!(!pending.is_empty());
        let frame = read_frame(&mut reader, &mut pending).await?.unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&frame)?["method"],
            "server.ping"
        );
        client.await??;
        Ok(())
    }
    #[tokio::test]
    async fn worker_queue_rejects_instead_of_blocking_async_task() {
        let (sender, _receiver) = mpsc::sync_channel(1);
        let (reply, _) = oneshot::channel();
        assert!(sender
            .try_send(Request {
                method: "unused".into(),
                params: Value::Null,
                reply
            })
            .is_ok());
        let error = ask(&sender, "unused", Value::Null).await.unwrap_err();
        assert_eq!(error.0, -32001);
    }

    #[tokio::test]
    async fn oversized_frame_fails_before_eof_and_stays_bounded() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let client = tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut stream = TcpStream::connect(address).await?;
            let _ = stream.write_all(&vec![b'x'; MAX_FRAME + 1]).await;
            let _ = stream.read(&mut [0u8; 1]).await;
            Ok::<_, std::io::Error>(())
        });
        let (stream, _) = listener.accept().await?;
        let mut reader = BufReader::new(stream);
        let mut pending = Vec::new();
        assert!(read_frame(&mut reader, &mut pending).await.is_err());
        assert!(pending.len() <= MAX_FRAME);
        drop(reader);
        client.await??;
        Ok(())
    }

    #[tokio::test]
    async fn reorg_before_first_poll_notifies_even_with_unchanged_status() -> Result<()> {
        let (sender, receiver) = mpsc::sync_channel::<Request>(2);
        let wire = "00".repeat(32);
        let worker_wire = wire.clone();
        let worker = std::thread::spawn(move || {
            let request = receiver.recv().unwrap();
            assert_eq!(request.method, "__subscribe");
            let _ = request
                .reply
                .send(Ok(json!({"status":null,"tip":"old-tip"})));
            let request = receiver.recv().unwrap();
            assert_eq!(request.method, "__poll");
            let mut statuses = serde_json::Map::new();
            statuses.insert(worker_wire, Value::Null);
            let _ = request.reply.send(Ok(
                json!({"tip":"new-tip","header":null,"statuses":statuses}),
            ));
        });
        let options = Options {
            chain: Chain::Regtest,
            root: PathBuf::new(),
            core: Core::new(
                "http://127.0.0.1:1/",
                std::path::Path::new("/unused-cookie"),
            )?,
            listen: "127.0.0.1:60603".parse()?,
            connections: 4,
            subscriptions: 4,
            query_limit: 10,
            index_limits: IndexLimits::default(),
            mempool_budget: Budget::default(),
            poll: Duration::from_millis(250),
            min_free_bytes: 0,
        };
        let health = Arc::new(RwLock::new(Health {
            data: Value::Null,
            updated: Instant::now(),
            max_age: Duration::from_secs(10),
        }));
        let mut connection = Connection::default();
        let request = json!({"id":1,"method":"blockchain.scripthash.subscribe","params":[wire]});
        assert!(connection
            .one(&request, &sender, &health, &options)
            .await
            .unwrap()["result"]
            .is_null());
        let notifications = connection.notifications(&sender).await;
        assert_eq!(notifications.len(), 1);
        assert_eq!(notifications[0]["params"], json!([wire, null]));
        worker.join().unwrap();
        Ok(())
    }
}
