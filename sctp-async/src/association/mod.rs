impl Association {
    async fn read_loop(
        name: String,
        bytes_received: Arc<AtomicUsize>,
        net_conn: Arc<dyn Conn + Send + Sync>,
        mut close_loop_ch: broadcast::Receiver<()>,
        association_internal: Arc<Mutex<AssociationInternal>>,
    ) {
        log::debug!("[{}] read_loop entered", name);

        let mut buffer = vec![0u8; RECEIVE_MTU];
        let mut done = false;
        let mut n;
        while !done {
            tokio::select! {
                _ = close_loop_ch.recv() => break,
                result = net_conn.recv(&mut buffer) => {
                    match result {
                        Ok(m) => {
                            n=m;
                        }
                        Err(err) => {
                            log::warn!("[{}] failed to read packets on net_conn: {}", name, err);
                            break;
                        }
                    }
                }
            };

            // Make a buffer sized to what we read, then copy the data we
            // read from the underlying transport. We do this because the
            // user data is passed to the reassembly queue without
            // copying.
            log::debug!("[{}] recving {} bytes", name, n);
            let inbound = Bytes::from(buffer[..n].to_vec());
            bytes_received.fetch_add(n, Ordering::SeqCst);

            {
                let mut ai = association_internal.lock().await;
                if let Err(err) = ai.handle_inbound(&inbound).await {
                    log::warn!("[{}] failed to handle_inbound: {:?}", name, err);
                    done = true;
                }
            }
        }

        {
            let mut ai = association_internal.lock().await;
            if let Err(err) = ai.close().await {
                log::warn!("[{}] failed to close association: {:?}", name, err);
            }
        }

        log::debug!("[{}] read_loop exited", name);
    }

    async fn write_loop(
        name: String,
        bytes_sent: Arc<AtomicUsize>,
        net_conn: Arc<dyn Conn + Send + Sync>,
        mut close_loop_ch: broadcast::Receiver<()>,
        association_internal: Arc<Mutex<AssociationInternal>>,
        mut awake_write_loop_ch: mpsc::Receiver<()>,
    ) {
        log::debug!("[{}] write_loop entered", name);
        let mut done = false;
        while !done {
            //log::debug!("[{}] gather_outbound begin", name);
            let (raw_packets, mut ok) = {
                let mut ai = association_internal.lock().await;
                ai.gather_outbound().await
            };
            //log::debug!("[{}] gather_outbound done with {}", name, raw_packets.len());

            for raw in &raw_packets {
                log::debug!("[{}] sending {} bytes", name, raw.len());
                if let Err(err) = net_conn.send(raw).await {
                    log::warn!("[{}] failed to write packets on net_conn: {}", name, err);
                    ok = false;
                    break;
                } else {
                    bytes_sent.fetch_add(raw.len(), Ordering::SeqCst);
                }
                //log::debug!("[{}] sending {} bytes done", name, raw.len());
            }

            if !ok {
                break;
            }

            //log::debug!("[{}] wait awake_write_loop_ch", name);
            tokio::select! {
                _ = awake_write_loop_ch.recv() =>{}
                _ = close_loop_ch.recv() => {
                    done = true;
                }
            };
            //log::debug!("[{}] wait awake_write_loop_ch done", name);
        }

        {
            let mut ai = association_internal.lock().await;
            if let Err(err) = ai.close().await {
                log::warn!("[{}] failed to close association: {:?}", name, err);
            }
        }

        log::debug!("[{}] write_loop exited", name);
    }
}
