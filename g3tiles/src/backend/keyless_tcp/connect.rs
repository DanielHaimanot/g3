/*
 * SPDX-License-Identifier: Apache-2.0
 * Copyright 2024-2025 ByteDance and/or its affiliates.
 */

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use arc_swap::ArcSwapOption;
use async_trait::async_trait;
use rustls_pki_types::ServerName;
use tokio::io::{ReadHalf, WriteHalf};
use tokio::net::{TcpStream, tcp};
use tokio::sync::broadcast;
use tokio::time::Instant;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

use g3_io_ext::AsyncStream;
use g3_std_ext::time::DurationExt;
use g3_types::collection::{SelectiveVec, WeightedValue};
use g3_types::net::RustlsClientConfig;

use crate::config::backend::keyless_tcp::KeylessTcpBackendConfig;
use crate::module::keyless::{
    KeylessBackendStats, KeylessForwardRequest, KeylessUpstreamConnect,
    KeylessUpstreamDurationRecorder, MultiplexedUpstreamConnection,
};

pub(super) struct KeylessTcpUpstreamConnector {
    config: Arc<KeylessTcpBackendConfig>,
    stats: Arc<KeylessBackendStats>,
    duration_recorder: Arc<KeylessUpstreamDurationRecorder>,
    peer_addrs: Arc<ArcSwapOption<SelectiveVec<WeightedValue<SocketAddr>>>>,
}

impl KeylessTcpUpstreamConnector {
    pub(super) fn new(
        config: Arc<KeylessTcpBackendConfig>,
        site_stats: Arc<KeylessBackendStats>,
        duration_recorder: Arc<KeylessUpstreamDurationRecorder>,
        peer_addrs_container: Arc<ArcSwapOption<SelectiveVec<WeightedValue<SocketAddr>>>>,
    ) -> Self {
        KeylessTcpUpstreamConnector {
            config,
            stats: site_stats,
            duration_recorder,
            peer_addrs: peer_addrs_container,
        }
    }

    async fn connect(&self) -> anyhow::Result<(TcpStream, SocketAddr)> {
        let Some(peer) = self.peer_addrs.load().as_ref().map(|peers| {
            let v = peers.pick_random();
            *v.inner()
        }) else {
            return Err(anyhow!("no peer address available"));
        };

        self.stats.add_conn_attempt();

        let sock = g3_socket::tcp::new_socket_to(
            peer.ip(),
            &Default::default(),
            &self.config.tcp_keepalive,
            &Default::default(),
            true,
        )?;

        let stream = sock
            .connect(peer)
            .await
            .map_err(|e| anyhow!("failed to connect to peer {peer}: {e}"))?;
        self.stats.add_conn_established();

        Ok((stream, peer))
    }
}

#[async_trait]
impl KeylessUpstreamConnect for KeylessTcpUpstreamConnector {
    type Connection = MultiplexedUpstreamConnection<tcp::OwnedReadHalf, tcp::OwnedWriteHalf>;

    async fn new_connection(
        &self,
        req_receiver: flume::Receiver<KeylessForwardRequest>,
        quit_notifier: broadcast::Receiver<()>,
        _idle_timeout: Duration,
    ) -> anyhow::Result<Self::Connection> {
        let start = Instant::now();
        let (stream, _peer) = self.connect().await?;
        let _ = self
            .duration_recorder
            .connect
            .record(start.elapsed().as_nanos_u64());
        let (clt_r, clt_w) = stream.into_split();

        Ok(MultiplexedUpstreamConnection::new(
            self.config.connection_config,
            self.stats.clone(),
            self.duration_recorder.clone(),
            clt_r,
            clt_w,
            req_receiver,
            quit_notifier,
        ))
    }
}

pub(super) struct KeylessTlsUpstreamConnector {
    tcp: KeylessTcpUpstreamConnector,
    tls: RustlsClientConfig,
}

impl KeylessTlsUpstreamConnector {
    pub(super) fn new(tcp: KeylessTcpUpstreamConnector, tls: RustlsClientConfig) -> Self {
        KeylessTlsUpstreamConnector { tcp, tls }
    }
}

#[async_trait]
impl KeylessUpstreamConnect for KeylessTlsUpstreamConnector {
    type Connection = MultiplexedUpstreamConnection<
        ReadHalf<TlsStream<TcpStream>>,
        WriteHalf<TlsStream<TcpStream>>,
    >;

    async fn new_connection(
        &self,
        req_receiver: flume::Receiver<KeylessForwardRequest>,
        quit_notifier: broadcast::Receiver<()>,
        _idle_timeout: Duration,
    ) -> anyhow::Result<Self::Connection> {
        let start = Instant::now();
        let (tcp_stream, peer) = self.tcp.connect().await?;

        let tls_name = self
            .tcp
            .config
            .tls_name
            .clone()
            .unwrap_or_else(|| ServerName::IpAddress(peer.ip().into()));
        let tls_connector = TlsConnector::from(self.tls.driver.clone());
        match tokio::time::timeout(
            self.tls.handshake_timeout,
            tls_connector.connect(tls_name, tcp_stream),
        )
        .await
        {
            Ok(Ok(tls_stream)) => {
                let _ = self
                    .tcp
                    .duration_recorder
                    .connect
                    .record(start.elapsed().as_nanos_u64());
                let (clt_r, clt_w) = tls_stream.into_split();

                Ok(MultiplexedUpstreamConnection::new(
                    self.tcp.config.connection_config,
                    self.tcp.stats.clone(),
                    self.tcp.duration_recorder.clone(),
                    clt_r,
                    clt_w,
                    req_receiver,
                    quit_notifier,
                ))
            }
            Ok(Err(e)) => Err(anyhow!("tls handshake failed: {e}")),
            Err(_) => Err(anyhow!("tls handshake timeout")),
        }
    }
}
