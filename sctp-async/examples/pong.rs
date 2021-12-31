use proto::{AssociationError, ReliabilityType, ServerConfig};
use sctp_async::{Connecting, Endpoint, NewAssociation, Stream};

use anyhow::{anyhow, Result};
use bytes::Bytes;
use clap::{App, AppSettings, Arg};
use futures_util::{StreamExt, TryFutureExt};
use std::time::Duration;
use tracing::{error, info, info_span};
use tracing_futures::Instrument as _;

// RUST_LOG=trace cargo run --color=always --package webrtc-sctp --example pong -- --host 0.0.0.0:5678

#[tokio::main]
async fn main() -> Result<()> {
    tracing::subscriber::set_global_default(
        tracing_subscriber::FmtSubscriber::builder()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .finish(),
    )
    .unwrap();

    let mut app = App::new("SCTP Pong")
        .version("0.1.0")
        .author("Rain Liu <yliu@webrtc.rs>")
        .about("An example of SCTP Server")
        .setting(AppSettings::DeriveDisplayOrder)
        .setting(AppSettings::SubcommandsNegateReqs)
        .arg(
            Arg::with_name("FULLHELP")
                .help("Prints more detailed help information")
                .long("fullhelp"),
        )
        .arg(
            Arg::with_name("host")
                .required_unless("FULLHELP")
                .takes_value(true)
                .long("host")
                .help("SCTP host name."),
        );

    let matches = app.clone().get_matches();

    if matches.is_present("FULLHELP") {
        app.print_long_help().unwrap();
        std::process::exit(0);
    }

    let host = matches.value_of("host").unwrap();

    let (endpoint, mut incoming) = Endpoint::server(ServerConfig::new(), host.parse().unwrap())?;
    eprintln!("listening on {}", endpoint.local_addr()?);

    while let Some(conn) = incoming.next().await {
        info!("association incoming");
        tokio::spawn(handle_association(conn).unwrap_or_else(move |e| {
            error!("association failed: {reason}", reason = e.to_string())
        }));
    }

    Ok(())
}

async fn handle_association(conn: Connecting) -> Result<()> {
    let NewAssociation {
        association,
        mut incoming_streams,
        ..
    } = conn.await?;
    let span = info_span!(
        "association",
        remote = %association.remote_addr(),
    );
    async {
        info!("established");

        // Each stream initiated by the client constitutes a new request.
        while let Some(stream) = incoming_streams.next().await {
            let stream = match stream {
                Err(AssociationError::ApplicationClosed { .. }) => {
                    info!("association closed");
                    return Ok(());
                }
                Err(e) => {
                    return Err(e);
                }
                Ok(s) => s,
            };
            tokio::spawn(
                handle_stream(stream)
                    .unwrap_or_else(move |e| error!("failed: {reason}", reason = e.to_string()))
                    .instrument(info_span!("request")),
            );
        }
        Ok(())
    }
    .instrument(span)
    .await?;
    Ok(())
}

async fn handle_stream(mut stream: Stream) -> Result<()> {
    // set unordered = true and 10ms threshold for dropping packets
    stream.set_reliability_params(true, ReliabilityType::Timed, 10)?;

    let mut buff = vec![0u8; 1024];
    while let Ok(Some(n)) = stream.read(&mut buff).await {
        let ping_msg = String::from_utf8(buff[..n].to_vec()).unwrap();
        println!("received: {}", ping_msg);

        let pong_msg = format!("pong [{}]", ping_msg);
        println!("sent: {}", pong_msg);
        stream.write(&Bytes::from(pong_msg)).await?;

        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    println!("finished ping-pong");

    // Gracefully terminate the stream
    stream
        .finish()
        .await
        .map_err(|e| anyhow!("failed to shutdown stream: {}", e))?;
    info!("complete");
    Ok(())
}
