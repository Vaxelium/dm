//! The direct message service. Direct messages are kept here, not on the chat servers, so two people
//! can message each other without sharing a server. Each request carries the sender's session, which
//! this service checks with the auth service.
//!
//! Usage: `dm [ADDR] [DB_PATH] [AUTH_ADDR]`

mod db;
mod service;
#[cfg(test)]
mod tests;

use std::sync::{Arc, Mutex};

use common::{AuthReply, AuthRequest, DmReply, DmRequest, DEFAULT_AUTH_ADDR, DEFAULT_DM_ADDR};
use db::Db;
use service::Identity;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

const DEFAULT_DB_PATH: &str = "dm.db";

type SharedDb = Arc<Mutex<Db>>;

fn main() -> anyhow::Result<()> {
    common::env::load();
    tokio::runtime::Runtime::new()?.block_on(run())
}

async fn run() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let addr = args.next().unwrap_or_else(|| DEFAULT_DM_ADDR.to_string());
    let db_path = args.next().unwrap_or_else(|| DEFAULT_DB_PATH.to_string());
    let auth_addr = args.next().unwrap_or_else(|| DEFAULT_AUTH_ADDR.to_string());

    let db = Db::open(&db_path)?;
    // The service's own certificate. It is made the first time the service runs.
    let (cert, key) = match db.tls_identity()? {
        Some(identity) => identity,
        None => {
            let identity = common::tls::new_identity()?;
            db.store_tls_identity(&identity.0, &identity.1)?;
            identity
        }
    };
    let acceptor = common::tls::acceptor(&cert, &key)?;
    let db: SharedDb = Arc::new(Mutex::new(db));
    let auth_addr = Arc::new(auth_addr);

    let listener = TcpListener::bind(&addr).await?;
    println!("Vaxelium direct message service listening on {addr} (TLS), storing messages in {db_path}");

    loop {
        let (socket, peer) = listener.accept().await?;
        let db = db.clone();
        let auth_addr = auth_addr.clone();
        let acceptor = acceptor.clone();
        tokio::spawn(async move {
            let socket = match acceptor.accept(socket).await {
                Ok(socket) => socket,
                Err(e) => {
                    eprintln!("{peer}: TLS handshake failed: {e}");
                    return;
                }
            };
            if let Err(e) = handle_connection(socket, db, &auth_addr).await {
                if !common::tls::is_quiet_close(&e) {
                    eprintln!("{peer}: {e}");
                }
            }
        });
    }
}

/// Reads requests from one connection, one JSON line each, and answers each in turn.
async fn handle_connection<S>(socket: S, db: SharedDb, auth_addr: &str) -> anyhow::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (reader, mut writer) = tokio::io::split(socket);
    let mut lines = BufReader::new(reader).lines();
    while let Some(line) = lines.next_line().await? {
        let reply = match serde_json::from_str::<DmRequest>(&line) {
            Ok(request) => answer(&db, auth_addr, request).await,
            Err(e) => DmReply::Error { text: format!("bad request: {e}") },
        };
        let mut out = serde_json::to_string(&reply)?;
        out.push('\n');
        writer.write_all(out.as_bytes()).await?;
    }
    Ok(())
}

/// Checks the sender's session, looks up the other person when the request names one, and does the request.
async fn answer(db: &SharedDb, auth_addr: &str, request: DmRequest) -> DmReply {
    let token = token_of(&request).to_string();
    let me = match verify(db, auth_addr, &token).await {
        Ok(me) => me,
        Err(text) => return DmReply::Error { text },
    };
    let other = match other_name_of(&request) {
        Some(id) => profile_name(db, auth_addr, id).await,
        None => None,
    };
    let db = db.lock().unwrap();
    service::apply(&db, &me, request, other)
}

fn token_of(request: &DmRequest) -> &str {
    match request {
        DmRequest::Open { token, .. }
        | DmRequest::List { token }
        | DmRequest::History { token, .. }
        | DmRequest::Send { token, .. }
        | DmRequest::Edit { token, .. }
        | DmRequest::Delete { token, .. }
        | DmRequest::React { token, .. }
        | DmRequest::Pin { token, .. }
        | DmRequest::Read { token, .. }
        | DmRequest::Poll { token, .. } => token,
    }
}

/// The account a request names, when it names one (the conversation to open or the message's recipient).
fn other_name_of(request: &DmRequest) -> Option<&str> {
    match request {
        DmRequest::Open { with, .. } => Some(with),
        DmRequest::Send { to, .. } => Some(to),
        _ => None,
    }
}

/// Who the token belongs to, as the auth service says. A session it does not know is refused.
async fn verify(db: &SharedDb, auth_addr: &str, token: &str) -> Result<Identity, String> {
    match auth_call(db, auth_addr, &AuthRequest::Verify { token: token.to_string() }).await {
        Ok(AuthReply::Verified { id, name }) => Ok(Identity { id, name }),
        Ok(AuthReply::Error { text }) => Err(text),
        Ok(_) => Err("unexpected reply from the auth service".into()),
        Err(text) => Err(text),
    }
}

/// The display name of an account, or `None` when there is no such account.
async fn profile_name(db: &SharedDb, auth_addr: &str, id: &str) -> Option<String> {
    let request = AuthRequest::ProfileCard { id: id.to_string(), token: String::new() };
    match auth_call(db, auth_addr, &request).await {
        Ok(AuthReply::ProfileCard { name, .. }) => Some(name),
        _ => None,
    }
}

/// Asks the auth service something over TLS. Its certificate is checked against the one remembered.
async fn auth_call(db: &SharedDb, auth_addr: &str, request: &AuthRequest) -> Result<AuthReply, String> {
    let (stream, fingerprint) = common::tls::connect_async(auth_addr)
        .await
        .map_err(|e| format!("auth service unavailable: {e}"))?;
    pin_ok(db, auth_addr, &fingerprint)?;
    let mut stream = stream;
    let mut line = serde_json::to_string(request).map_err(|e| e.to_string())?;
    line.push('\n');
    stream.write_all(line.as_bytes()).await.map_err(|e| format!("auth service unavailable: {e}"))?;
    let mut lines = BufReader::new(stream).lines();
    let reply = lines
        .next_line()
        .await
        .map_err(|e| format!("auth service unavailable: {e}"))?
        .ok_or_else(|| "auth service closed the connection".to_string())?;
    serde_json::from_str::<AuthReply>(&reply).map_err(|_| "unexpected reply from the auth service".to_string())
}

/// The auth service's certificate is remembered the first time it is seen. A change is refused.
fn pin_ok(db: &SharedDb, auth_addr: &str, fingerprint: &str) -> Result<(), String> {
    let db = db.lock().unwrap();
    let remembered = db.pin(&format!("auth:{auth_addr}")).map_err(|e| e.to_string())?;
    match common::tls::check_pin(remembered.as_deref(), fingerprint) {
        Ok(true) => db.set_pin(&format!("auth:{auth_addr}"), fingerprint).map_err(|e| e.to_string()),
        Ok(false) => Ok(()),
        Err(e) => Err(e),
    }
}
