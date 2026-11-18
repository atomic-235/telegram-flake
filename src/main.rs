use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use chrono::{DateTime, Local, TimeZone};
use clap::{Parser, Subcommand};
use grammers_client::client::Client;
use grammers_client::session::SessionData;
use grammers_client::session::storages::MemorySession;
use grammers_client::tl;
use grammers_client::SenderPool;

/// Telegram CLI. Secrets arrive as JSON on stdin, piped from `sops decrypt`.
#[derive(Parser)]
#[command(name = "tg", version, about)]
struct Cli {
    /// Channel map for contrarian-news: {"<channel id>": "<display name>"}
    #[arg(long, global = true, value_name = "PATH", env = "TELEGRAM_CHANNELS")]
    channels: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show the current account
    Me,
    /// List dialogs
    Chats {
        #[arg(long, default_value_t = 30)]
        limit: usize,
        #[arg(long, value_name = "TYPE")]
        r#type: Option<String>,
    },
    /// List messages in a chat
    Messages {
        chat: String,
        #[arg(long, default_value_t = 20)]
        limit: i32,
        #[arg(long, default_value_t = 1)]
        page: i32,
    },
    /// Search messages in a chat
    Search {
        chat: String,
        query: String,
        #[arg(long, default_value_t = 20)]
        limit: i32,
    },
    /// Send a message
    Send { chat: String, text: String },
    /// List forum topics in a supergroup
    Topics { chat: String },
    /// List contacts
    Contacts {
        #[arg(long)]
        search: Option<String>,
    },
    /// Fetch recent messages from several chats at once
    Digest {
        #[arg(required = true)]
        chats: Vec<String>,
        #[arg(long, default_value_t = 10)]
        limit: i32,
    },
    /// Run a JSON array of operations in one login and one connection
    Batch { spec: PathBuf },
    /// Download media from a message
    Download {
        chat: String,
        message_id: i32,
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Fetch news from the contrarian source channels
    ContrarianNews {
        #[arg(long, value_name = "DATE")]
        since: Option<String>,
        #[arg(long, value_name = "DATE")]
        until: Option<String>,
        #[arg(long, default_value = "full")]
        format: String,
        #[arg(long, default_value_t = 0)]
        max_chars: usize,
        #[arg(long, default_value_t = 5)]
        max_per_channel: usize,
    },
}

struct Secrets {
    api_id: i32,
    api_hash: String,
    session: String,
}

fn read_secrets() -> Result<Secrets> {
    let mut raw = String::new();
    std::io::stdin()
        .read_to_string(&mut raw)
        .context("read secrets from stdin")?;
    if raw.trim().is_empty() {
        bail!("no secrets on stdin: pipe `sops decrypt --output-type json <file>` into tg");
    }
    let value: serde_json::Value =
        serde_json::from_str(&raw).context("parse secrets JSON from stdin")?;

    let field = |key: &str| -> Option<String> {
        match value.get(key) {
            Some(serde_json::Value::String(s)) => Some(s.clone()),
            Some(serde_json::Value::Number(n)) => Some(n.to_string()),
            _ => None,
        }
    };

    let api_id: i32 = field("TELEGRAM_API_ID")
        .ok_or_else(|| anyhow!("secrets document has no TELEGRAM_API_ID"))?
        .parse()
        .context("TELEGRAM_API_ID is not a number")?;
    let api_hash =
        field("TELEGRAM_API_HASH").ok_or_else(|| anyhow!("no TELEGRAM_API_HASH"))?;
    let session = field("TELEGRAM_SESSION_STRING")
        .ok_or_else(|| anyhow!("no TELEGRAM_SESSION_STRING"))?;

    Ok(Secrets {
        api_id,
        api_hash,
        session,
    })
}

/// Decode Telethon's StringSession (`1` + base64url of dc|ip|port|auth_key)
/// into a grammers session.
fn session_data_from_telethon(encoded: &str) -> Result<SessionData> {
    if !encoded.starts_with('1') {
        bail!("TELEGRAM_SESSION_STRING: unsupported version prefix");
    }
    let bytes = base64::engine::general_purpose::URL_SAFE
        .decode(&encoded[1..])
        .context("decode TELEGRAM_SESSION_STRING")?;

    let ip_len = match bytes.len() {
        263 => 4,
        275 => 16,
        other => bail!("TELEGRAM_SESSION_STRING: unexpected length {other}"),
    };
    if bytes.len() != 1 + ip_len + 2 + 256 {
        bail!("TELEGRAM_SESSION_STRING: unexpected layout");
    }

    let dc = i32::from(bytes[0]);
    let key_start = 1 + ip_len + 2;
    let mut auth_key = [0u8; 256];
    auth_key.copy_from_slice(&bytes[key_start..key_start + 256]);

    let mut data = SessionData::default();
    data.home_dc = dc;
    match data.dc_options.get_mut(&dc) {
        Some(option) => option.auth_key = Some(auth_key),
        None => bail!("TELEGRAM_SESSION_STRING: unknown data centre {dc}"),
    }
    Ok(data)
}


/// v2rayA's plain socks port, used when it is listening. `TG_PROXY` overrides;
/// `TG_PROXY=none` forces a direct connection.
fn proxy_url() -> Option<String> {
    if let Ok(value) = std::env::var("TG_PROXY") {
        if value.is_empty() || value == "none" {
            return None;
        }
        return Some(value);
    }
    let address: std::net::SocketAddr = "127.0.0.1:12334".parse().ok()?;
    std::net::TcpStream::connect_timeout(&address, std::time::Duration::from_millis(300)).ok()?;
    Some(format!("socks5://{address}"))
}

async fn connect(secrets: &Secrets) -> Result<Client> {
    let data = session_data_from_telethon(&secrets.session)?;
    let session = Arc::new(MemorySession::default());
    data.import_to(&*session)
        .await
        .map_err(|e| anyhow!("import session: {e:?}"))?;

    let mut params = grammers_client::sender::ConnectionParams::default();
    params.proxy_url = proxy_url();

    let pool = SenderPool::with_configuration(Arc::clone(&session), secrets.api_id, params);
    let client = Client::new(pool.handle.clone());
    tokio::spawn(pool.runner.run());

    if !client.is_authorized().await? {
        bail!("session is not authorized; relogin with the Python client first");
    }
    Ok(client)
}

fn stamp(seconds: i64) -> String {
    Local
        .timestamp_opt(seconds, 0)
        .single()
        .map(|dt: DateTime<Local>| dt.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "?".to_string())
}

fn peer_name(peer: &grammers_client::peer::Peer) -> String {
    peer.name()
        .map(str::to_string)
        .unwrap_or_else(|| peer.id().to_string())
}

/// The dialog list is the only way to map a bare numeric id to a peer.
async fn dialogs(client: &Client) -> Result<Vec<grammers_client::peer::Peer>> {
    let mut out = Vec::new();
    let mut iter = client.iter_dialogs();
    while let Some(dialog) = iter.next().await? {
        out.push(dialog.peer);
    }
    Ok(out)
}

async fn find_peer(
    client: &Client,
    peers: &[grammers_client::peer::Peer],
    chat: &str,
) -> Result<grammers_client::peer::Peer> {
    let wanted = chat.trim().trim_start_matches('@');
    if let Ok(id) = wanted.parse::<i64>() {
        let bare = id.rem_euclid(1_000_000_000_000);
        for peer in peers {
            if peer.id().bare_id() == Some(bare) {
                return Ok(peer.clone());
            }
        }
        bail!("cannot resolve chat {chat:?}: not in the dialog list");
    }

    if let Some(peer) = client.resolve_username(wanted).await? {
        return Ok(peer);
    }
    for peer in peers {
        if peer.name().is_some_and(|name| name.eq_ignore_ascii_case(wanted)) {
            return Ok(peer.clone());
        }
    }
    bail!("cannot resolve chat {chat:?}")
}

async fn peer_ref(
    peer: &grammers_client::peer::Peer,
) -> Result<grammers_client::session::types::PeerRef> {
    match peer.to_ref().await {
        Ok(Some(reference)) => Ok(reference),
        Ok(None) => bail!(
            "peer {} is not in the session cache; list chats first",
            peer_name(peer)
        ),
        Err(error) => bail!("peer {}: {error}", peer_name(peer)),
    }
}


async fn print_message(
    peers: &[grammers_client::peer::Peer],
    message: &grammers_client::message::Message,
) {
    let mut text = message.text().to_string();
    if text.is_empty() {
        text = "[media]".to_string();
    }
    let sender = message
        .sender_ref()
        .await
        .ok()
        .flatten()
        .and_then(|reference| {
            peers
                .iter()
                .find(|peer| peer.id() == reference.id)
                .map(peer_name)
        })
        .unwrap_or_default();
    let prefix = if sender.is_empty() {
        String::new()
    } else {
        format!("{sender}: ")
    };
    println!(
        "[{}] {} {prefix}{text}",
        message.id(),
        stamp(message.date().as_second())
    );
}

async fn print_messages(
    client: &Client,
    peers: &[grammers_client::peer::Peer],
    peer: &grammers_client::peer::Peer,
    limit: i32,
    page: i32,
    empty: &str,
) -> Result<()> {
    let reference = peer_ref(peer).await?;
    let mut messages = Vec::new();
    let mut iter = client.iter_messages(reference).limit(limit.max(1) as usize);
    while let Some(message) = iter.next().await? {
        messages.push(message);
    }
    if page > 1 {
        let skip = ((page - 1) * limit.max(1)) as usize;
        messages = messages.into_iter().skip(skip).collect();
    }
    if messages.is_empty() {
        println!("{empty}");
        return Ok(());
    }
    for message in &messages {
        print_message(peers, message).await;
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    // Fail on a bad channel map before decrypting secrets or opening a session.
    let channels = match &cli.command {
        Command::ContrarianNews { .. } => Some(load_channels(cli.channels.as_ref())?),
        _ => None,
    };

    let secrets = read_secrets()?;
    let client = connect(&secrets).await?;
    let peers = dialogs(&client).await?;

    match cli.command {
        Command::Me => {
            let me = client.get_me().await?;
            println!("Name:     {}", me.full_name());
            if let Some(username) = me.username() {
                println!("Username: @{username}");
            }
        }
        Command::Chats { limit, r#type } => {
            let mut iter = client.iter_dialogs();
            let mut shown = 0;
            while let Some(dialog) = iter.next().await? {
                let peer = &dialog.peer;
                let kind = match peer.id().kind() {
                    grammers_client::session::types::PeerKind::User => "user",
                    grammers_client::session::types::PeerKind::Chat => "chat",
                    grammers_client::session::types::PeerKind::Channel => "channel",
                };
                if let Some(wanted) = &r#type {
                    if wanted != kind {
                        continue;
                    }
                }
                println!(
                    "{:<15} {:<10} {}",
                    peer.id().bare_id().unwrap_or(0),
                    kind,
                    peer_name(peer)
                );
                shown += 1;
                if shown >= limit {
                    break;
                }
            }
        }
        Command::Messages { chat, limit, page } => {
            let peer = find_peer(&client, &peers, &chat).await?;
            print_messages(&client, &peers, &peer, limit, page, "No messages.").await?;
        }
        Command::Search { chat, query, limit } => {
            let peer = find_peer(&client, &peers, &chat).await?;
            let reference = peer_ref(&peer).await?;
            let mut results = Vec::new();
            let mut iter = client
                .search_messages(reference)
                .query(&query)
                .limit(limit as usize);
            while let Some(message) = iter.next().await? {
                results.push(message);
            }
            if results.is_empty() {
                println!("No results.");
            }
            for message in &results {
                print_message(&peers, message).await;
            }
            if results.len() as i32 >= limit {
                eprintln!("  (limit {limit} reached; raise --limit for more)");
            }
        }
        Command::Send { chat, text } => {
            let peer = find_peer(&client, &peers, &chat).await?;
            let message = client.send_message(peer_ref(&peer).await?, text.as_str()).await?;
            println!("Sent: message ID {}", message.id());
        }
        Command::Topics { chat } => {
            let _ = chat;
            bail!("topics is not available: this TL layer has no forum-topic types");
        }
        Command::Contacts { search } => {
            let result = client
                .invoke(&tl::functions::contacts::GetContacts { hash: 0 })
                .await?;
            let users = match result {
                tl::enums::contacts::Contacts::Contacts(contacts) => contacts.users,
                tl::enums::contacts::Contacts::NotModified => Vec::new(),
            };
            let needle = search.map(|s| s.to_lowercase());
            for user in users {
                let tl::enums::User::User(user) = user else {
                    continue;
                };
                let name = format!(
                    "{} {}",
                    user.first_name.as_deref().unwrap_or(""),
                    user.last_name.as_deref().unwrap_or("")
                )
                .trim()
                .to_string();
                if let Some(needle) = &needle {
                    if !name.to_lowercase().contains(needle)
                        && user.id.to_string() != *needle
                    {
                        continue;
                    }
                }
                let phone = user
                    .phone
                    .as_deref()
                    .map(|p| format!("  {p}"))
                    .unwrap_or_default();
                println!("{:<15} {}{}", user.id, name, phone);
            }
        }
        Command::Digest { chats, limit } => {
            for chat in chats {
                println!();
                match find_peer(&client, &peers, &chat).await {
                    Ok(peer) => {
                        println!("{}", "=".repeat(60));
                        println!("## {}  (id: {})", peer_name(&peer), chat);
                        println!("{}", "=".repeat(60));
                        let reference = peer_ref(&peer).await?;
                        let mut messages = Vec::new();
                        let mut iter = client.iter_messages(reference).limit(limit.max(1) as usize);
                        while let Some(message) = iter.next().await? {
                            messages.push(message);
                        }
                        if messages.is_empty() {
                            println!("  (no messages)");
                        }
                        for message in &messages {
                            print_message(&peers, message).await;
                        }
                    }
                    Err(error) => {
                        println!("{}", "=".repeat(60));
                        println!("## {chat}  (id: {chat})");
                        println!("{}", "=".repeat(60));
                        println!("  (fetch failed: cannot resolve chat: {error})");
                    }
                }
            }
        }
        Command::Batch { spec } => {
            let operations = load_batch(&spec)?;
            for (index, operation) in operations.iter().enumerate() {
                println!();
                println!("{}", "=".repeat(60));
                println!("## [{}/{}] {}", index + 1, operations.len(), operation.label());
                println!("{}", "=".repeat(60));
                if let Err(error) = run_batch(&client, &peers, operation).await {
                    eprintln!("  (failed: {error})");
                }
            }
        }
        Command::Download {
            chat,
            message_id,
            out,
        } => {
            let peer = find_peer(&client, &peers, &chat).await?;
            let messages = client
                .get_messages_by_id(peer_ref(&peer).await?, &[message_id])
                .await?;
            let message = messages
                .into_iter()
                .flatten()
                .next()
                .ok_or_else(|| anyhow!("message {message_id} not found"))?;
            let media = message
                .media()
                .ok_or_else(|| anyhow!("message {message_id} has no media"))?;
            let path = out.unwrap_or_else(|| PathBuf::from(format!("download_{message_id}")));
            client.download_media(&media, &path).await?;
            println!("Downloaded to: {}", path.display());
        }
        Command::ContrarianNews {
            since,
            until,
            format,
            max_chars,
            max_per_channel,
        } => {
            let channels = channels.expect("loaded before connecting");
            let news = fetch_news(&client, &peers, &channels, since, until).await?;
            print_news(news, &format, max_chars, max_per_channel);
        }
    }

    Ok(())
}

#[derive(serde::Deserialize)]
struct BatchOp {
    cmd: String,
    #[serde(default)]
    chat: Option<String>,
    #[serde(default)]
    chats: Option<Vec<String>>,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    search: Option<String>,
    #[serde(default)]
    out: Option<String>,
    #[serde(default)]
    limit: Option<i32>,
    #[serde(default)]
    page: Option<i32>,
    #[serde(default)]
    message_id: Option<i32>,
    #[serde(default)]
    r#type: Option<String>,
}

impl BatchOp {
    fn label(&self) -> String {
        let mut parts = vec![self.cmd.clone()];
        if let Some(chat) = &self.chat {
            parts.push(format!("chat={chat}"));
        }
        if let Some(query) = &self.query {
            parts.push(format!("query={query}"));
        }
        if let Some(limit) = self.limit {
            parts.push(format!("limit={limit}"));
        }
        parts.join(" ")
    }
}

fn load_batch(path: &std::path::Path) -> Result<Vec<BatchOp>> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("read batch spec {}", path.display()))?;
    let operations: Vec<BatchOp> = serde_json::from_str(&raw)
        .with_context(|| format!("{} is not a JSON array of operations", path.display()))?;
    if operations.is_empty() {
        bail!("{}: the operation list is empty", path.display());
    }
    Ok(operations)
}

async fn run_batch(
    client: &Client,
    peers: &[grammers_client::peer::Peer],
    operation: &BatchOp,
) -> Result<()> {
    match operation.cmd.as_str() {
        "me" => {
            let me = client.get_me().await?;
            println!("Name:     {}", me.full_name());
        }
        "chats" => {
            let limit = operation.limit.unwrap_or(30) as usize;
            let mut iter = client.iter_dialogs();
            let mut shown = 0;
            while let Some(dialog) = iter.next().await? {
                println!("{}", peer_name(&dialog.peer));
                shown += 1;
                if shown >= limit {
                    break;
                }
            }
        }
        "messages" => {
            let chat = operation.chat.as_deref().unwrap_or_default();
            let peer = find_peer(client, peers, chat).await?;
            print_messages(
                client,
                peers,
                &peer,
                operation.limit.unwrap_or(20),
                operation.page.unwrap_or(1),
                "No messages.",
            )
            .await?;
        }
        "search" => {
            let chat = operation.chat.as_deref().unwrap_or_default();
            let peer = find_peer(client, peers, chat).await?;
            let query = operation.query.as_deref().unwrap_or_default();
            let limit = operation.limit.unwrap_or(20);
            let reference = peer_ref(&peer).await?;
            let mut iter = client
                .search_messages(reference)
                .query(query)
                .limit(limit as usize);
            let mut found = 0;
            while let Some(message) = iter.next().await? {
                print_message(peers, &message).await;
                found += 1;
            }
            if found == 0 {
                println!("No results.");
            }
        }
        "topics" => bail!("topics inside a batch is not supported yet"),
        "contacts" => bail!("contacts inside a batch is not supported yet"),
        "digest" => {
            let limit = operation.limit.unwrap_or(10);
            for chat in operation.chats.as_deref().unwrap_or_default() {
                let peer = find_peer(client, peers, chat).await?;
                println!("## {}  (id: {})", peer_name(&peer), chat);
                let reference = peer_ref(&peer).await?;
                let mut iter = client.iter_messages(reference).limit(limit as usize);
                while let Some(message) = iter.next().await? {
                    print_message(peers, &message).await;
                }
            }
        }
        "send" => {
            let chat = operation.chat.as_deref().unwrap_or_default();
            let peer = find_peer(client, peers, chat).await?;
            let text = operation.text.as_deref().unwrap_or_default();
            let message = client.send_message(peer_ref(&peer).await?, text).await?;
            println!("Sent: message ID {}", message.id());
        }
        "download" => bail!("download inside a batch is not supported yet"),
        other => bail!("unknown cmd {other:?}"),
    }
    Ok(())
}



struct NewsItem {
    date: i64,
    channel: String,
    id: i32,
    text: String,
}

fn parse_date(value: &Option<String>, fallback: DateTime<Local>) -> Result<DateTime<Local>> {
    let Some(value) = value else {
        return Ok(fallback);
    };
    for layout in ["%Y-%m-%d", "%Y-%m-%dT%H:%M"] {
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(value, layout) {
            if let Some(local) = Local.from_local_datetime(&dt).single() {
                return Ok(local);
            }
        }
        if let Ok(date) = chrono::NaiveDate::parse_from_str(value, layout) {
            if let Some(local) = date.and_hms_opt(0, 0, 0).and_then(|dt| {
                Local.from_local_datetime(&dt).single()
            }) {
                return Ok(local);
            }
        }
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(value) {
        return Ok(dt.with_timezone(&Local));
    }
    bail!("invalid date {value:?}, use YYYY-MM-DD or YYYY-MM-DDTHH:MM")
}

type ChannelMap = std::collections::BTreeMap<i64, String>;

fn default_channels_path() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("telegram").join("channels.json");
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home)
        .join(".config")
        .join("telegram")
        .join("channels.json")
}

fn load_channels(path: Option<&PathBuf>) -> Result<ChannelMap> {
    let path = path.cloned().unwrap_or_else(default_channels_path);
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("read channel map {}", path.display()))?;
    let map: ChannelMap = serde_json::from_str(&raw).with_context(|| {
        format!(
            "{} must be a JSON object of {{\"<channel id>\": \"<name>\"}}",
            path.display()
        )
    })?;
    if map.is_empty() {
        bail!("{} lists no channels", path.display());
    }
    Ok(map)
}

async fn fetch_news(
    client: &Client,
    peers: &[grammers_client::peer::Peer],
    channels: &ChannelMap,
    since: Option<String>,
    until: Option<String>,
) -> Result<Vec<NewsItem>> {
    let now = Local::now();
    let midnight = now
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .and_then(|dt| Local.from_local_datetime(&dt).single())
        .ok_or_else(|| anyhow!("local midnight"))?;
    let since_dt = parse_date(&since, midnight - chrono::Duration::days(1))?.timestamp();
    let until_dt = parse_date(&until, now)?.timestamp();

    let mut items = Vec::new();
    for (id, name) in channels {
        let Some(peer) = peers.iter().find(|p| p.id().bare_id() == Some(*id)) else {
            eprintln!("  (failed {name}: not in the dialog list)");
            continue;
        };
        let reference = peer_ref(peer).await?;
        let mut iter = client.iter_messages(reference).limit(300);
        while let Some(message) = iter.next().await? {
            let date = message.date().as_second();
            if date < since_dt {
                break;
            }
            if date > until_dt || message.text().is_empty() {
                continue;
            }
            items.push(NewsItem {
                date,
                channel: (*name).to_string(),
                id: message.id(),
                text: message.text().to_string(),
            });
        }
    }
    items.sort_by_key(|item| item.date);
    Ok(items)
}

fn print_news(items: Vec<NewsItem>, format: &str, max_chars: usize, max_per_channel: usize) {
    let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    let kept: Vec<&NewsItem> = items
        .iter()
        .filter(|item| {
            let count = counts.entry(item.channel.as_str()).or_insert(0);
            *count += 1;
            max_per_channel == 0 || *count <= max_per_channel
        })
        .collect();

    if format == "json" {
        let entries: Vec<serde_json::Value> = kept
            .iter()
            .map(|item| {
                serde_json::json!({
                    "date": stamp(item.date),
                    "channel": item.channel,
                    "message_id": item.id,
                    "text": item.text,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&entries).unwrap());
        return;
    }

    for item in kept {
        let mut text = item.text.clone();
        if max_chars > 0 && text.chars().count() > max_chars {
            text = text.chars().take(max_chars).collect::<String>() + "...";
        }
        if format == "headlines" {
            let headline = text.lines().next().unwrap_or_default();
            println!("{} [{}] {}", stamp(item.date), item.channel, headline);
            continue;
        }
        println!("---");
        println!("{} [{}] (id: {})", stamp(item.date), item.channel, item.id);
        println!("{text}");
    }
}

// Keep the unused-import warning away until download lands.
#[allow(dead_code)]
fn _unused(mut writer: impl Write) {
    let _ = writer.write_all(b"");
}
