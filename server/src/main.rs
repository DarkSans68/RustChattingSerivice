use anyhow::{anyhow, Result};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot, RwLock},
    time::timeout,
};

type ClientTx = mpsc::Sender<String>;
type ShutdownTx = oneshot::Sender<()>;

#[derive(Default)]
struct Registry {
    by_id: HashMap<u64, ClientTx>,
    id_by_name: HashMap<String, u64>,
    name_by_id: HashMap<u64, String>,
    shutdown: HashMap<u64, ShutdownTx>,
}

type Shared = Arc<RwLock<Registry>>;
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

#[tokio::main]
async fn main() -> Result<()> {
    // Get primary network IP using Linux `ip route`
    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg("ip route get 1.1.1.1 | awk '{print $7}'")
        .output()
        .expect("failed to run ip route");

    let ip = String::from_utf8_lossy(&output.stdout)
        .trim()
        .to_string();

    let bind_addr = format!("{ip}:5555");

    let listener = TcpListener::bind(&bind_addr).await?;
    println!("Server running on {bind_addr}");

    let reg: Shared = Arc::new(RwLock::new(Registry::default()));

    loop {
        let (sock, addr) = listener.accept().await?;
        println!("Client connected: {addr}");
        let reg = reg.clone();

        tokio::spawn(async move {
            if let Err(e) = handle_client(sock, reg).await {
                eprintln!("Client {addr} error: {e}");
            }
            println!("Client {addr} disconnected");
        });
    }
}

async fn handle_client(stream: TcpStream, reg: Shared) -> Result<()> {
    let _ = stream.set_nodelay(true);
    let (reader, writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    let my_id = NEXT_ID.fetch_add(1, Ordering::Relaxed);

    // Get nickname with a timeout and fast failure feedback.
    let nick_line = match timeout(HANDSHAKE_TIMEOUT, lines.next_line()).await {
        Ok(Ok(Some(line))) => line,
        Ok(Ok(None)) => return Err(anyhow!("client disconnected before sending a nickname")),
        Ok(Err(e)) => return Err(anyhow!("failed to read nickname: {e}")),
        Err(_) => {
            let mut writer = writer;
            let _ = writer
                .write_all(b"ERR timeout waiting for NICK\n")
                .await;
            return Err(anyhow!("client handshake timed out"));
        }
    };

    let name = match parse_nick(&nick_line) {
        Some(n) => n,
        None => {
            let mut writer = writer;
            let _ = writer
                .write_all(b"ERR expected: NICK <name>\n")
                .await;
            return Err(anyhow!("bad nickname command"));
        }
    };
    println!("[LOGIN] {name} assigned ID {my_id}");

    {
        let r = reg.read().await;
        if r.id_by_name.contains_key(&name) {
            let mut writer = writer;
            let _ = writer
                .write_all(b"ERR name already in use\n")
                .await;
            return Err(anyhow!("name '{}' already in use", name));
        }
    }

    let (tx, mut rx) = mpsc::channel::<String>(64);
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();

    let mut writer = writer;
    let writer_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if writer.write_all(msg.as_bytes()).await.is_err() { break; }
            if writer.write_all(b"\n").await.is_err() { break; }
        }
    });

    {
        let mut r = reg.write().await;
        r.by_id.insert(my_id, tx.clone());
        r.id_by_name.insert(name.clone(), my_id);
        r.name_by_id.insert(my_id, name.clone());
        r.shutdown.insert(my_id, shutdown_tx);
    }

    send_to_id(&reg, my_id, &format!("WELCOME {my_id} {name}")).await?;
    send_to_id(&reg, my_id, "[server] commands: TO <name> <msg> | TOID <id> <msg> | KICK <name> | KICKID <id>").await?;

    // Handle commands/messages
    loop {
        let line_opt = tokio::select! {
            r = timeout(IDLE_TIMEOUT, lines.next_line()) => match r {
                Ok(Ok(line)) => line,
                Ok(Err(e)) => return Err(anyhow!(e)),
                Err(_) => {
                    send_to_id(&reg, my_id, "[server] timed out due to inactivity").await.ok();
                    None
                }
            },
            _ = &mut shutdown_rx => {
                send_to_id(&reg, my_id, "[server] disconnected").await.ok();
                None
            }
        };

        let Some(line) = line_opt else {
            break;
        };
        let line = line.trim();

        // ---- KICK BY NAME ----
        if let Some(target_name) = line.strip_prefix("KICK ") {
            println!("[ADMIN] {name} ({my_id}) requested kick on {target_name}");

            if let Some(tid) = find_id_by_name(&reg, target_name).await {
                send_to_id(&reg, tid, "[server] kicked").await.ok();
                disconnect_client(&reg, tid).await;
                send_to_id(&reg, my_id, "[server] user kicked").await?;
            } else {
                send_to_id(&reg, my_id, "[server] user not found").await?;
            }
            continue;
        }

        // ---- KICK BY ID ----
        if let Some(id_str) = line.strip_prefix("KICKID ") {
            println!("[ADMIN] {name} ({my_id}) requested kick on ID: {id_str}");

            if name != "admin" {
                send_to_id(&reg, my_id, "[server] permission denied").await?;
                println!("[DENIED] {name} ({my_id}) tried to use admin command.");
                continue;
            }

            if let Ok(tid) = id_str.parse::<u64>() {
                send_to_id(&reg, tid, "[server] kicked").await.ok();
                disconnect_client(&reg, tid).await;
                send_to_id(&reg, my_id, "[server] user kicked").await?;
            } else {
                send_to_id(&reg, my_id, "[server] invalid ID").await?;
            }
            continue;
        }

        // ---- MESSAGING ----
        if let Some((target_name, msg)) = parse_to(line) {
            let target_id = find_id_by_name(&reg, target_name).await;

            println!("[MSG] {name} ({my_id}) -> {target_name}: {msg}");

            if let Some(tid) = target_id {
                let payload = format!("from {name}({my_id}): {msg}");
                if send_to_id(&reg, tid, &payload).await.is_err() {
                    send_to_id(&reg, my_id, "[server] target disconnected").await?;
                }
            } else {
                send_to_id(&reg, my_id, "[server] target not found").await?;
            }
            continue;
        }

        if let Some((tid, msg)) = parse_toid(line) {
            let tname = {
                let r = reg.read().await;
                r.name_by_id.get(&tid).cloned().unwrap_or_else(|| "?".into())
            };

            println!("[MSG] {name} ({my_id}) -> {tname} ({tid}): {msg}");

            let payload = format!("from {name}({my_id}): {msg}");
            if send_to_id(&reg, tid, &payload).await.is_err() {
                send_to_id(&reg, my_id, "[server] target offline").await?;
            }
            continue;
        }

        send_to_id(&reg, my_id, "[server] commands: TO | TOID | KICK | KICKID").await?;
    }

    disconnect_client(&reg, my_id).await;
    let _ = writer_task.await;
    Ok(())
}

async fn find_id_by_name(reg: &Shared, name: &str) -> Option<u64> {
    let r = reg.read().await;
    r.id_by_name.get(name).copied()
}

async fn disconnect_client(reg: &Shared, id: u64) {
    let mut r = reg.write().await;

    if let Some(shutdown) = r.shutdown.remove(&id) {
        let _ = shutdown.send(());
    }

    if let Some(name) = r.name_by_id.remove(&id) {
        println!("[DISCONNECT] {name} ({id}) was removed.");
        r.id_by_name.remove(&name);
    }

    r.by_id.remove(&id);
}

fn parse_nick(line: &str) -> Option<String> {
    let (cmd, nick) = line.split_once(' ')?;
    if cmd.eq_ignore_ascii_case("NICK") {
        Some(nick.trim().to_string())
    } else {
        None
    }
}

fn parse_to(line: &str) -> Option<(&str, &str)> {
    let mut p = line.splitn(3, ' ');
    match (p.next(), p.next(), p.next()) {
        (Some("TO"), Some(name), Some(rest)) => Some((name, rest)),
        _ => None,
    }
}

fn parse_toid(line: &str) -> Option<(u64, &str)> {
    let mut p = line.splitn(3, ' ');
    match (p.next(), p.next(), p.next()) {
        (Some("TOID"), Some(id_s), Some(rest)) => id_s.parse::<u64>().ok().map(|id| (id, rest)),
        _ => None,
    }
}

async fn send_to_id(reg: &Shared, id: u64, msg: &str) -> Result<()> {
    let tx = {
        let r = reg.read().await;
        r.by_id.get(&id).cloned()
    }.ok_or_else(|| anyhow!("no such id"))?;

    tx.send(msg.to_string())
        .await
        .map_err(|_| anyhow!("failed to deliver message to {id}"))
}
