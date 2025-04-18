// Copyright (C) 2024, Cloudflare, Inc.
// All rights reserved.
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions are
// met:
//
//     * Redistributions of source code must retain the above copyright notice,
//       this list of conditions and the following disclaimer.
//
//     * Redistributions in binary form must reproduce the above copyright
//       notice, this list of conditions and the following disclaimer in the
//       documentation and/or other materials provided with the distribution.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS
// IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO,
// THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR
// PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR
// CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL,
// EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO,
// PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR
// PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF
// LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING
// NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
// SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

//! Responsible for creating a [quiche::Connection] and managing I/O.

use std::slice::Iter;
use std::time::Duration;
use std::time::Instant;

use crate::frame::H3iFrame;
use crate::quiche;

use crate::actions::h3::Action;
use crate::actions::h3::StreamEventType;
use crate::actions::h3::WaitType;
use crate::actions::h3::WaitingFor;
use crate::client::build_quiche_connection;
use crate::client::execute_action;
use crate::client::parse_streams;
use crate::client::ClientError;
use crate::client::ConnectionCloseDetails;
use crate::client::MAX_DATAGRAM_SIZE;
use crate::config::Config;

use super::Client;
use super::CloseTriggerFrames;
use super::ConnectionSummary;
use super::StreamMap;
use super::StreamParserMap;

#[derive(Default)]
struct SyncClient {
    streams: StreamMap,
    stream_parsers: StreamParserMap,
}

impl SyncClient {
    fn new(close_trigger_frames: Option<CloseTriggerFrames>) -> Self {
        Self {
            streams: StreamMap::new(close_trigger_frames),
            ..Default::default()
        }
    }
}

impl Client for SyncClient {
    fn stream_parsers_mut(&mut self) -> &mut StreamParserMap {
        &mut self.stream_parsers
    }

    fn handle_response_frame(&mut self, stream_id: u64, frame: H3iFrame) {
        self.streams.insert(stream_id, frame);
    }
}

/// Connect to a server and execute provided actions.
///
/// Constructs a socket and [quiche::Connection] based on the provided `args`,
/// then iterates over `actions`.
///
/// If `close_trigger_frames` is specified, h3i will close the connection
/// immediately upon receiving all of the supplied frames rather than waiting
/// for the idle timeout. See [`CloseTriggerFrames`] for details.
///
/// Returns a [ConnectionSummary] on success, [ClientError] on failure.
pub fn connect(
    args: Config, actions: &[Action],
    close_trigger_frames: Option<CloseTriggerFrames>,
) -> std::result::Result<ConnectionSummary, ClientError> {
    let mut buf = [0; 65535];
    let mut out = [0; MAX_DATAGRAM_SIZE];

    // Setup the event loop.
    let mut poll = mio::Poll::new().unwrap();
    let mut events = mio::Events::with_capacity(1024);

    // Resolve server address.
    let peer_addr = if let Some(addr) = &args.connect_to {
        addr.parse().expect("--connect-to is expected to be a string containing an IPv4 or IPv6 address with a port. E.g. 192.0.2.0:443")
    } else {
        let x = format!("https://{}", args.host_port);
        *url::Url::parse(&x)
            .unwrap()
            .socket_addrs(|| None)
            .unwrap()
            .first()
            .unwrap()
    };

    // Bind to INADDR_ANY or IN6ADDR_ANY depending on the IP family of the
    // server address. This is needed on macOS and BSD variants that don't
    // support binding to IN6ADDR_ANY for both v4 and v6.
    let bind_addr = match peer_addr {
        std::net::SocketAddr::V4(_) => format!("0.0.0.0:{}", args.source_port),
        std::net::SocketAddr::V6(_) => format!("[::]:{}", args.source_port),
    };

    // Create the UDP socket backing the QUIC connection, and register it with
    // the event loop.
    let mut socket =
        mio::net::UdpSocket::bind(bind_addr.parse().unwrap()).unwrap();
    poll.registry()
        .register(&mut socket, mio::Token(0), mio::Interest::READABLE)
        .unwrap();

    let Ok(local_addr) = socket.local_addr() else {
        return Err(ClientError::Other("invalid socket".to_string()));
    };

    let mut conn = build_quiche_connection(args, peer_addr, local_addr)
        .map_err(|_| ClientError::HandshakeFail)?;

    let mut app_proto_selected = false;

    let (write, send_info) = conn.send(&mut out).expect("initial send failed");

    while let Err(e) = socket.send_to(&out[..write], send_info.to) {
        if e.kind() == std::io::ErrorKind::WouldBlock {
            log::debug!(
                "{} -> {}: send() would block",
                socket.local_addr().unwrap(),
                send_info.to
            );
            continue;
        }

        return Err(ClientError::Other(format!("send() failed: {e:?}")));
    }

    let app_data_start = std::time::Instant::now();

    let mut action_iter = actions.iter();
    let mut wait_duration = None;
    let mut wait_instant = None;

    let mut client = SyncClient::new(close_trigger_frames);
    let mut waiting_for = WaitingFor::default();

    loop {
        let actual_sleep = match (wait_duration, conn.timeout()) {
            (Some(wait), Some(timeout)) => {
                #[allow(clippy::comparison_chain)]
                if timeout < wait {
                    // shave some off the wait time so it doesn't go longer
                    // than user really wanted.
                    let new = wait - timeout;
                    wait_duration = Some(new);
                    Some(timeout)
                } else if wait < timeout {
                    Some(wait)
                } else {
                    // same, so picking either doesn't matter
                    Some(timeout)
                }
            },
            (None, Some(timeout)) => Some(timeout),
            (Some(wait), None) => Some(wait),
            _ => None,
        };

        log::debug!("actual sleep is {:?}", actual_sleep);
        poll.poll(&mut events, actual_sleep).unwrap();

        // If the event loop reported no events, run a belt and braces check on
        // the quiche connection's timeouts.
        if events.is_empty() {
            log::debug!("timed out");

            conn.on_timeout();
        }

        // Read incoming UDP packets from the socket and feed them to quiche,
        // until there are no more packets to read.
        for event in &events {
            let socket = match event.token() {
                mio::Token(0) => &socket,

                _ => unreachable!(),
            };

            let local_addr = socket.local_addr().unwrap();
            'read: loop {
                let (len, from) = match socket.recv_from(&mut buf) {
                    Ok(v) => v,

                    Err(e) => {
                        // There are no more UDP packets to read on this socket.
                        // Process subsequent events.
                        if e.kind() == std::io::ErrorKind::WouldBlock {
                            break 'read;
                        }

                        return Err(ClientError::Other(format!(
                            "{local_addr}: recv() failed: {e:?}"
                        )));
                    },
                };

                let recv_info = quiche::RecvInfo {
                    to: local_addr,
                    from,
                };

                // Process potentially coalesced packets.
                let _read = match conn.recv(&mut buf[..len], recv_info) {
                    Ok(v) => v,

                    Err(e) => {
                        log::debug!("{}: recv failed: {:?}", local_addr, e);
                        continue 'read;
                    },
                };
            }
        }

        log::debug!("done reading");

        if conn.is_closed() {
            log::info!(
                "connection closed with error={:?} did_idle_timeout={}, stats={:?} path_stats={:?}",
                conn.peer_error(),
                conn.is_timed_out(),
                conn.stats(),
                conn.path_stats().collect::<Vec<quiche::PathStats>>(),
            );

            if !conn.is_established() {
                log::info!(
                    "connection timed out after {:?}",
                    app_data_start.elapsed(),
                );

                return Err(ClientError::HandshakeFail);
            }

            break;
        }

        // Create a new application protocol session once the QUIC connection is
        // established.
        if (conn.is_established() || conn.is_in_early_data()) &&
            !app_proto_selected
        {
            app_proto_selected = true;
        }

        if app_proto_selected {
            check_duration_and_do_actions(
                &mut wait_duration,
                &mut wait_instant,
                &mut action_iter,
                &mut conn,
                &mut waiting_for,
                client.stream_parsers_mut(),
            );

            let mut wait_cleared = false;
            for response in parse_streams(&mut conn, &mut client) {
                let stream_id = response.stream_id;

                if let StreamEventType::Finished = response.event_type {
                    waiting_for.clear_waits_on_stream(stream_id);
                } else {
                    waiting_for.remove_wait(response);
                }

                wait_cleared = true;
            }

            if client.streams.all_close_trigger_frames_seen() {
                client.streams.close_due_to_trigger_frames(&mut conn);
            }

            if wait_cleared {
                check_duration_and_do_actions(
                    &mut wait_duration,
                    &mut wait_instant,
                    &mut action_iter,
                    &mut conn,
                    &mut waiting_for,
                    client.stream_parsers_mut(),
                );
            }
        }

        // Provides as many CIDs as possible.
        while conn.scids_left() > 0 {
            let (scid, reset_token) = generate_cid_and_reset_token();

            if conn.new_scid(&scid, reset_token, false).is_err() {
                break;
            }
        }

        // Generate outgoing QUIC packets and send them on the UDP socket, until
        // quiche reports that there are no more packets to be sent.
        let sockets = vec![&socket];

        for socket in sockets {
            let local_addr = socket.local_addr().unwrap();

            for peer_addr in conn.paths_iter(local_addr) {
                loop {
                    let (write, send_info) = match conn.send_on_path(
                        &mut out,
                        Some(local_addr),
                        Some(peer_addr),
                    ) {
                        Ok(v) => v,

                        Err(quiche::Error::Done) => {
                            break;
                        },

                        Err(e) => {
                            log::error!(
                                "{} -> {}: send failed: {:?}",
                                local_addr,
                                peer_addr,
                                e
                            );

                            conn.close(false, 0x1, b"fail").ok();
                            break;
                        },
                    };

                    if let Err(e) = socket.send_to(&out[..write], send_info.to) {
                        if e.kind() == std::io::ErrorKind::WouldBlock {
                            log::debug!(
                                "{} -> {}: send() would block",
                                local_addr,
                                send_info.to
                            );
                            break;
                        }

                        return Err(ClientError::Other(format!(
                            "{} -> {}: send() failed: {:?}",
                            local_addr, send_info.to, e
                        )));
                    }
                }
            }
        }

        if conn.is_closed() {
            log::info!(
                "connection closed, {:?} {:?}",
                conn.stats(),
                conn.path_stats().collect::<Vec<quiche::PathStats>>()
            );

            if !conn.is_established() {
                log::info!(
                    "connection timed out after {:?}",
                    app_data_start.elapsed(),
                );

                return Err(ClientError::HandshakeFail);
            }

            break;
        }
    }

    Ok(ConnectionSummary {
        stream_map: client.streams,
        stats: Some(conn.stats()),
        path_stats: conn.path_stats().collect(),
        conn_close_details: ConnectionCloseDetails::new(&conn),
    })
}

fn check_duration_and_do_actions(
    wait_duration: &mut Option<Duration>, wait_instant: &mut Option<Instant>,
    action_iter: &mut Iter<Action>, conn: &mut quiche::Connection,
    waiting_for: &mut WaitingFor, stream_parsers: &mut StreamParserMap,
) {
    match wait_duration.as_ref() {
        None => {
            if let Some(idle_wait) =
                handle_actions(action_iter, conn, waiting_for, stream_parsers)
            {
                *wait_duration = Some(idle_wait);
                *wait_instant = Some(Instant::now());

                // TODO: the wait period could still be larger than the
                // negotiated idle timeout.
                // We could in theory check quiche's idle_timeout value if
                // it was public.
                log::info!(
                    "waiting for {:?} before executing more actions",
                    idle_wait
                );
            }
        },

        Some(period) => {
            let now = Instant::now();
            let then = wait_instant.unwrap();
            log::debug!(
                "checking if actions wait period elapsed {:?} > {:?}",
                now.duration_since(then),
                wait_duration
            );
            if now.duration_since(then) >= *period {
                log::debug!("yup!");
                *wait_duration = None;

                if let Some(idle_wait) =
                    handle_actions(action_iter, conn, waiting_for, stream_parsers)
                {
                    *wait_duration = Some(idle_wait);
                }
            }
        },
    }
}

/// Generate a new pair of Source Connection ID and reset token.
pub fn generate_cid_and_reset_token() -> (quiche::ConnectionId<'static>, u128) {
    let mut scid = [0; quiche::MAX_CONN_ID_LEN];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut scid);
    let scid = scid.to_vec().into();
    let mut reset_token = [0; 16];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut reset_token);
    let reset_token = u128::from_be_bytes(reset_token);
    (scid, reset_token)
}

/// Makes a buffered writer for a qlog.
pub fn make_qlog_writer(
    dir: &std::ffi::OsStr, role: &str, id: &str,
) -> std::io::BufWriter<std::fs::File> {
    let mut path = std::path::PathBuf::from(dir);
    let filename = format!("{role}-{id}.sqlog");
    path.push(filename);

    match std::fs::File::create(&path) {
        Ok(f) => std::io::BufWriter::new(f),

        Err(e) => panic!(
            "Error creating qlog file attempted path was {:?}: {}",
            path, e
        ),
    }
}

fn handle_actions<'a, I>(
    iter: &mut I, conn: &mut quiche::Connection, waiting_for: &mut WaitingFor,
    stream_parsers: &mut StreamParserMap,
) -> Option<Duration>
where
    I: Iterator<Item = &'a Action>,
{
    if !waiting_for.is_empty() {
        log::debug!(
            "won't fire an action due to waiting for responses: {:?}",
            waiting_for
        );
        return None;
    }

    // Send actions
    for action in iter {
        match action {
            Action::FlushPackets => return None,
            Action::Wait { wait_type } => match wait_type {
                WaitType::WaitDuration(period) => return Some(*period),
                WaitType::StreamEvent(response) => {
                    log::info!(
                        "waiting for {:?} before executing more actions",
                        response
                    );
                    waiting_for.add_wait(response);
                    return None;
                },
            },
            action => execute_action(action, conn, stream_parsers),
        }
    }

    None
}
