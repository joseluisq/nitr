use hyper::server::conn::http1;
use hyper_util::rt::TokioIo;
use mlua::AnyUserData;
use std::path::Path;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use nitr::service::Svc;
use nitr::userdata::UserData;
use nitr::{Context, Result, Runtime};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result {
    let listen_addr = "127.0.0.1:3000";
    let listener = TcpListener::bind(listen_addr)
        .await
        .with_context(|| format!("Unable to listen on {listen_addr}"))?;

    // TODO: use configuration instead
    let conf_src = Path::new("scripts/config.lua");
    let http_src = Path::new("scripts/handler.lua");

    println!("Listening on http://{listen_addr}");

    let mut rt = Runtime::new().await?;

    // TODO: register globals via config
    rt.register_globals(
        UserData::NONE
            | UserData::DEBUG
            | UserData::FETCH
            | UserData::TEMPLATE
            | UserData::JSON
            | UserData::DATABASE,
    )
    .await?;

    let db = rt
        .get_global::<AnyUserData>(UserData::DATABASE)
        .with_context(|| "Failed to get Lua database handler")?;

    rt.register_cfg_fn(conf_src, db).await?;
    rt.register_http_fn(http_src).await?;

    let rt = Arc::new(Mutex::new(rt));

    loop {
        let (stream, peer_addr) = match listener.accept().await {
            Ok(x) => x,
            Err(err) => {
                eprintln!("Failed to accept connection: {err}");
                continue;
            }
        };

        let svc = Svc::new(rt.clone(), peer_addr);

        tokio::task::spawn(async move {
            if let Err(err) = http1::Builder::new()
                .serve_connection(TokioIo::new(stream), svc)
                .await
            {
                eprintln!("Error serving connection: {err}",);
            }
        });
    }
}
