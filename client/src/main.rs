use anyhow::{anyhow, Result};
use std::env;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    time::{timeout, Duration},
};

#[tokio::main]
async fn main() -> Result<()> {
    let mut stdin = BufReader::new(tokio::io::stdin()).lines();
    let args: Vec<String> = env::args().collect();

    // Config via simple flags.
    let mut address_arg: Option<String> = None;
    let mut nick_arg: Option<String> = None;
    let mut idx = 1;
    while idx < args.len() {
        match args[idx].as_str() {
            "--addr" if idx + 1 < args.len() => {
                address_arg = Some(args[idx + 1].clone());
                idx += 1;
            }
            "--nick" if idx + 1 < args.len() => {
                nick_arg = Some(args[idx + 1].clone());
                idx += 1;
            }
            _ => {}
        }
        idx += 1;
    }

    // Read server address
    let mut address = address_arg.unwrap_or_else(|| "127.0.0.1:5555".into());
    if address.trim().is_empty() {
        println!("Enter server address (ip:port):");
        while address.trim().is_empty() {
            address = stdin.next_line().await?.unwrap_or_default();
        }
    }

    // Read nickname
    let mut name = nick_arg.unwrap_or_default();
    if name.trim().is_empty() {
        println!("Enter your nickname:");
        while name.trim().is_empty() {
            name = stdin.next_line().await?.unwrap_or_default();
        }
    }

    // Connect
    println!("Connecting to {} ...", address);
    let stream = TcpStream::connect(address.trim()).await?;
    let _ = stream.set_nodelay(true);
    let (reader, mut writer) = stream.into_split();

    // Send nickname
    writer
        .write_all(format!("NICK {}\n", name.trim()).as_bytes())
        .await?;

    // Wait for server welcome/err with a short timeout so we fail fast.
    let mut incoming = BufReader::new(reader).lines();
    let first = timeout(Duration::from_secs(5), incoming.next_line())
        .await
        .map_err(|_| anyhow!("server did not respond in time"))??;

    match first {
        Some(line) if line.starts_with("WELCOME") => {
            println!("{line}");
        }
        Some(line) => {
            println!("Connection rejected: {line}");
            return Ok(());
        }
        None => {
            return Err(anyhow!("server closed connection during handshake"));
        }
    }
    println!("Registered as: {}", name.trim());

    // Listen for incoming messages
    tokio::spawn(async move {
        while let Ok(Some(line)) = incoming.next_line().await {
            println!("{line}");
            if line.starts_with("[server] disconnected") || line.starts_with("BYE") {
                break;
            }
        }
        println!("Server closed the connection");
    });

    // Forward user input to server
    while let Some(line) = stdin.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        writer.write_all(line.as_bytes()).await?;
        writer.write_all(b"\n").await?;
    }

    Ok(())
}
