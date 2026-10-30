//! Historical retrieval requests whole witness blocks only, never wallet identifiers.
use anyhow::{bail, ensure, Context, Result};
use bitcoin::{
    consensus::{deserialize, serialize},
    hashes::{sha256d, Hash},
    Block, BlockHash, Network,
};
use std::{
    io::{Read, Write},
    net::{TcpStream, ToSocketAddrs},
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub struct Fetcher {
    pub peers: Vec<String>,
    pub proxy: Option<String>,
}
pub struct Tor(std::process::Child);
impl Tor {
    pub fn start(path: &Path, proxy: &str) -> Result<Self> {
        let address: std::net::SocketAddr = proxy
            .parse()
            .context("numeric Tor SOCKS IP:port required")?;
        ensure!(
            address.ip().is_loopback() && address.port() > 0,
            "managed Tor SOCKS must bind loopback on a nonzero port"
        );
        ensure!(!path.is_symlink(), "Tor directory symlink refused");
        // Never silently attach to an unrelated proxy when our Tor fails to bind.
        let reservation = std::net::TcpListener::bind(address)
            .context("private Tor SOCKS port already in use")?;
        std::fs::create_dir_all(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
        }
        let conf = path.join("torrc");
        let config = b"# Electrum Lite private Tor client. No system torrc is loaded.\n";
        if conf.exists() {
            ensure!(
                std::fs::read(&conf)? == config,
                "private torrc was modified"
            );
        } else {
            let mut f = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&conf)?;
            f.write_all(config)?;
            f.sync_all()?;
            std::fs::File::open(path)?.sync_all()?;
        }
        let control = path.join("control-port");
        let cookie = path.join("control-cookie");
        let mut command = std::process::Command::new("tor");
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        drop(reservation);
        let child = command
            .arg("-f")
            .arg(conf)
            .args([
                "--DataDirectory",
                path.to_str().context("Tor path")?,
                "--SocksPort",
                proxy,
                "--ClientOnly",
                "1",
                "--AvoidDiskWrites",
                "1",
                "--ControlPort",
                "auto",
                "--ControlPortWriteToFile",
                control.to_str().context("Tor control path")?,
                "--CookieAuthentication",
                "1",
                "--CookieAuthFile",
                cookie.to_str().context("Tor cookie path")?,
                "--__OwningControllerProcess",
                &std::process::id().to_string(),
                "--Log",
                "notice stderr",
            ])
            .stdin(std::process::Stdio::null())
            .spawn()
            .context("install Tor or use --socks-proxy; no direct fallback")?;
        let mut tor = Self(child);
        let deadline = std::time::Instant::now();
        loop {
            ensure!(
                tor.0.try_wait()?.is_none(),
                "private Tor exited during startup; no direct fallback"
            );
            // Control identity, not public bootstrap or SOCKS availability,
            // determines startup. Local index queries must work while Tor is offline.
            if tor_control_pid(path).ok() == Some(tor.0.id()) {
                break;
            }
            ensure!(
                deadline.elapsed() < Duration::from_secs(10),
                "private Tor control startup timed out; no direct fallback"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        Ok(tor)
    }
}
fn tor_control_pid(path: &Path) -> Result<u32> {
    use std::io::{BufRead, BufReader};
    let port = std::fs::read_to_string(path.join("control-port"))?;
    ensure!(port.len() < 256, "Tor control address limit");
    let address: std::net::SocketAddr = port
        .trim()
        .strip_prefix("PORT=")
        .context("Tor control address")?
        .parse()?;
    ensure!(address.ip().is_loopback(), "Tor control must be loopback");
    let cookie = std::fs::read(path.join("control-cookie"))?;
    ensure!(cookie.len() == 32, "Tor control cookie length");
    let mut s = TcpStream::connect_timeout(&address, Duration::from_millis(200))?;
    s.set_read_timeout(Some(Duration::from_secs(1)))?;
    s.set_write_timeout(Some(Duration::from_secs(1)))?;
    s.write_all(format!("AUTHENTICATE {}\r\n", hex::encode(cookie)).as_bytes())?;
    let mut reader = BufReader::new(s);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    ensure!(line.trim() == "250 OK", "private Tor authentication failed");
    reader.get_mut().write_all(b"GETINFO process/pid\r\n")?;
    line.clear();
    reader.read_line(&mut line)?;
    let pid = line
        .trim()
        .strip_prefix("250-process/pid=")
        .context("private Tor PID response")?
        .parse()?;
    line.clear();
    reader.read_line(&mut line)?;
    ensure!(line.trim() == "250 OK", "private Tor control response");
    Ok(pid)
}
impl Drop for Tor {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            #[cfg(unix)]
            {
                let _ = std::process::Command::new("kill")
                    .args(["-TERM", &self.0.id().to_string()])
                    .status();
            }
            let start = std::time::Instant::now();
            while start.elapsed() < Duration::from_secs(5) {
                if self.0.try_wait().ok().flatten().is_some() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn connect(peer: &str, proxy: Option<&str>) -> Result<TcpStream> {
    let mut s = if let Some(proxy) = proxy {
        let addr = proxy
            .to_socket_addrs()?
            .next()
            .context("SOCKS proxy address")?;
        let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(10))?;
        s.set_read_timeout(Some(Duration::from_secs(30)))?;
        s.set_write_timeout(Some(Duration::from_secs(30)))?;
        s.write_all(&[5, 1, 0])?;
        let mut reply = [0; 2];
        s.read_exact(&mut reply)?;
        ensure!(reply == [5, 0], "SOCKS authentication negotiation failed");
        let (host, port) = peer.rsplit_once(':').context("peer requires host:port")?;
        let host = host.trim_matches(['[', ']']);
        let port: u16 = port.parse()?;
        ensure!(host.len() <= 255, "SOCKS hostname too long");
        let mut request = vec![5, 1, 0, 3, host.len() as u8];
        request.extend(host.as_bytes());
        request.extend(port.to_be_bytes());
        s.write_all(&request)?;
        let mut head = [0; 4];
        s.read_exact(&mut head)?;
        ensure!(head[0] == 5 && head[1] == 0, "SOCKS connection failed");
        let n = match head[3] {
            1 => 4,
            4 => 16,
            3 => {
                let mut b = [0];
                s.read_exact(&mut b)?;
                b[0] as usize
            }
            _ => bail!("invalid SOCKS reply"),
        };
        let mut discard = vec![0; n + 2];
        s.read_exact(&mut discard)?;
        s
    } else {
        ensure!(
            !peer.contains(".onion"),
            "onion peers require --tor or --socks-proxy; refusing local DNS resolution"
        );
        let addr = peer.to_socket_addrs()?.next().context("no peer address")?;
        TcpStream::connect_timeout(&addr, Duration::from_secs(10))?
    };
    s.set_read_timeout(Some(Duration::from_secs(30)))?;
    s.set_write_timeout(Some(Duration::from_secs(30)))?;
    // Keep this function responsible for all outbound connections: proxy mode never falls back.
    s.flush()?;
    Ok(s)
}
fn magic(network: Network) -> [u8; 4] {
    network.magic().to_bytes()
}
fn send(s: &mut TcpStream, network: Network, command: &str, payload: &[u8]) -> Result<()> {
    let mut h = Vec::from(magic(network));
    let mut cmd = [0; 12];
    ensure!(command.len() <= 12, "command too long");
    cmd[..command.len()].copy_from_slice(command.as_bytes());
    h.extend(cmd);
    h.extend((payload.len() as u32).to_le_bytes());
    h.extend(&sha256d::Hash::hash(payload).to_byte_array()[..4]);
    s.write_all(&h)?;
    s.write_all(payload)?;
    Ok(())
}
fn receive(s: &mut TcpStream, network: Network) -> Result<(String, Vec<u8>)> {
    let mut h = [0; 24];
    s.read_exact(&mut h)?;
    ensure!(h[..4] == magic(network), "peer network mismatch");
    let n = u32::from_le_bytes(h[16..20].try_into()?) as usize;
    ensure!(n <= 4_100_000, "P2P message too large");
    let mut b = vec![0; n];
    s.read_exact(&mut b)?;
    ensure!(
        sha256d::Hash::hash(&b).to_byte_array()[..4] == h[20..24],
        "P2P checksum mismatch"
    );
    Ok((
        String::from_utf8(h[4..16].to_vec())?
            .trim_end_matches('\0')
            .to_owned(),
        b,
    ))
}
impl Fetcher {
    pub fn get(
        &self,
        network: Network,
        hash: BlockHash,
        height: u32,
        cache: &Path,
    ) -> Result<Block> {
        self.retrieve_for_index(network, hash, height, cache, true)
    }
    pub(crate) fn retrieve_for_index(
        &self,
        network: Network,
        hash: BlockHash,
        height: u32,
        cache: &Path,
        persist: bool,
    ) -> Result<Block> {
        let path = cache.join(format!("{hash}.blk"));
        if path.exists() {
            let b: Block = deserialize(&std::fs::read(&path)?)?;
            verify_at(&b, hash, network, height)?;
            return Ok(b);
        }
        for peer in self.peers.iter().take(8) {
            match self.fetch_peer(network, hash, height, peer) {
                Ok(b) => {
                    // A semantic audit must not turn a thin server into a full
                    // historical archive. Already cached wallet blocks are reusable.
                    if !persist {
                        return Ok(b);
                    }
                    std::fs::create_dir_all(cache)?;
                    let temp = cache.join(format!("{hash}.pending"));
                    // Never overwrite unknown pending data after an interrupted write.
                    let mut f = std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&temp).context("cannot create cache record; preserve any .pending file from an interrupted write and recover explicitly")?;
                    f.write_all(&serialize(&b))?;
                    f.sync_all()?;
                    std::fs::rename(temp, &path)?;
                    std::fs::File::open(cache)?.sync_all()?;
                    return Ok(b);
                }
                Err(_) => continue,
            }
        }
        bail!("historical block unavailable from configured peers; no API/clearnet fallback")
    }
    fn fetch_peer(
        &self,
        network: Network,
        hash: BlockHash,
        height: u32,
        peer: &str,
    ) -> Result<Block> {
        let mut s = connect(peer, self.proxy.as_deref())?;
        let mut version = 70016i32.to_le_bytes().to_vec();
        version.extend(0u64.to_le_bytes());
        version
            .extend((SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64).to_le_bytes());
        version.extend([0; 52]);
        version.extend(0x454c5448494eu64.to_le_bytes());
        version.push(0);
        version.extend(0i32.to_le_bytes());
        version.push(0);
        send(&mut s, network, "version", &version)?;
        let mut got_version = false;
        let mut got_verack = false;
        for _ in 0..32 {
            let (c, b) = receive(&mut s, network)?;
            match c.as_str() {
                "version" => {
                    ensure!(b.len() >= 12, "short peer version");
                    let services = u64::from_le_bytes(b[4..12].try_into()?);
                    ensure!(services & 8 != 0, "peer lacks witness support");
                    got_version = true;
                    send(&mut s, network, "verack", &[])?;
                }
                "verack" => got_verack = true,
                "ping" => send(&mut s, network, "pong", &b)?,
                _ => {}
            }
            if got_version && got_verack {
                break;
            }
        }
        ensure!(got_version && got_verack, "P2P handshake incomplete");
        let mut request = vec![1];
        request.extend(0x40000002u32.to_le_bytes());
        request.extend(hash.as_byte_array());
        send(&mut s, network, "getdata", &request)?;
        for _ in 0..64 {
            let (c, b) = receive(&mut s, network)?;
            match c.as_str() {
                "block" => {
                    let block: Block = deserialize(&b)?;
                    verify_at(&block, hash, network, height)?;
                    return Ok(block);
                }
                "notfound" => bail!("peer does not have historical block"),
                "ping" => send(&mut s, network, "pong", &b)?,
                _ => {}
            }
        }
        bail!("peer response limit")
    }
}
pub fn verify(block: &Block, hash: BlockHash) -> Result<()> {
    verify_inner(block, hash, true)
}
pub fn verify_at(block: &Block, hash: BlockHash, network: Network, height: u32) -> Result<()> {
    let active = match network {
        Network::Bitcoin => height >= 481_824,
        Network::Testnet => height >= 834_624,
        _ => true,
    };
    verify_inner(block, hash, active)
}
fn verify_inner(block: &Block, hash: BlockHash, witness_active: bool) -> Result<()> {
    ensure!(
        block.block_hash() == hash && block.check_merkle_root() && block.check_witness_commitment(),
        "historical block hash/Merkle/witness verification failed"
    );
    // rust-bitcoin's convenience check accepts a fully stripped block when all
    // witnesses are empty. A peer must not strip witnesses from a committed block.
    let coinbase = block.txdata.first().context("empty block")?;
    ensure!(coinbase.is_coinbase(), "block does not start with coinbase");
    if !witness_active {
        ensure!(
            block
                .txdata
                .iter()
                .all(|tx| tx.input.iter().all(|i| i.witness.is_empty())),
            "witness before activation"
        );
        return Ok(());
    }
    if let Some(output) = coinbase.output.iter().rfind(|o| {
        o.script_pubkey.len() >= 38
            && o.script_pubkey.as_bytes()[..6] == [0x6a, 0x24, 0xaa, 0x21, 0xa9, 0xed]
    }) {
        let reserved = coinbase.input[0].witness.iter().collect::<Vec<_>>();
        ensure!(
            reserved.len() == 1 && reserved[0].len() == 32,
            "historical witness verification failed: stripped or malformed witness"
        );
        let root = block.witness_root().context("witness root")?;
        let commitment = Block::compute_witness_commitment(&root, reserved[0]);
        ensure!(
            commitment.as_byte_array().as_slice() == &output.script_pubkey.as_bytes()[6..38],
            "historical witness verification failed"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reject_wrong_hash() {
        let b = bitcoin::blockdata::constants::genesis_block(Network::Regtest);
        assert!(verify(&b, b.block_hash()).is_ok());
        assert!(verify(&b, BlockHash::all_zeros()).is_err());
    }
    #[test]
    fn reject_fully_stripped_witness_block() {
        let mut b = bitcoin::blockdata::constants::genesis_block(Network::Regtest);
        let reserved = [0u8; 32];
        b.txdata[0].input[0].witness = bitcoin::Witness::from_slice(&[reserved]);
        let commitment =
            Block::compute_witness_commitment(&b.witness_root().expect("root"), &reserved);
        let mut script = vec![0x6a, 0x24, 0xaa, 0x21, 0xa9, 0xed];
        script.extend(commitment.as_byte_array());
        b.txdata[0].output.push(bitcoin::TxOut {
            value: bitcoin::Amount::ZERO,
            script_pubkey: bitcoin::ScriptBuf::from_bytes(script),
        });
        b.header.merkle_root = b.compute_merkle_root().expect("root");
        assert!(verify(&b, b.block_hash()).is_ok());
        let hash = b.block_hash();
        b.txdata[0].input[0].witness = bitcoin::Witness::new();
        assert!(b.check_merkle_root());
        assert!(b.check_witness_commitment());
        assert!(verify(&b, hash).is_err());
    }
    #[test]
    fn socks_failure_never_connects_directly() -> Result<()> {
        use std::net::TcpListener;
        let direct = TcpListener::bind("127.0.0.1:0")?;
        direct.set_nonblocking(true)?;
        let proxy = TcpListener::bind("127.0.0.1:0")?;
        let addr = proxy.local_addr()?;
        let handle = std::thread::spawn(move || -> Result<()> {
            let (mut s, _) = proxy.accept()?;
            let mut greeting = [0; 3];
            s.read_exact(&mut greeting)?;
            ensure!(greeting == [5, 1, 0], "greeting");
            s.write_all(&[5, 0])?;
            let mut request = [0; 5];
            s.read_exact(&mut request)?;
            ensure!(request[..4] == [5, 1, 0, 3], "remote SOCKS domain required");
            let mut target = vec![0; request[4] as usize + 2];
            s.read_exact(&mut target)?;
            s.write_all(&[5, 4, 0, 1, 0, 0, 0, 0, 0, 0])?;
            Ok(())
        });
        let fetcher = Fetcher {
            peers: vec![direct.local_addr()?.to_string()],
            proxy: Some(addr.to_string()),
        };
        let hash = bitcoin::blockdata::constants::genesis_block(Network::Regtest).block_hash();
        let cache = std::env::temp_dir().join(format!("el-absent-cache-{}", std::process::id()));
        assert!(fetcher.get(Network::Regtest, hash, 0, &cache).is_err());
        handle.join().expect("proxy thread")?;
        assert!(matches!(direct.accept(),Err(e) if e.kind()==std::io::ErrorKind::WouldBlock));
        Ok(())
    }
}
