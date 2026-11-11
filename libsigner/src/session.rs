// Copyright (C) 2013-2020 Blockstack PBC, a public benefit corporation
// Copyright (C) 2020-2024 Stacks Open Internet Foundation
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

use std::net::TcpStream;
use std::time::Duration;
use std::{io, str};

use clarity::vm::types::QualifiedContractIdentifier;
use libstackerdb::{
    stackerdb_get_chunk_path, stackerdb_get_metadata_path, stackerdb_post_chunk_path, SlotMetadata,
    StackerDBChunkAckData, StackerDBChunkData, SIGNERS_STACKERDB_CHUNK_SIZE,
    STACKERDB_MAX_CHUNK_SIZE,
};
use stacks_common::codec::StacksMessageCodec;

use crate::error::RPCError;
use crate::http::send_http_request;

/// Trait for connecting to and querying a signer Stacker DB replica
pub trait SignerSession {
    /// connect to the replica
    fn connect(
        &mut self,
        host: String,
        stackerdb_contract_id: QualifiedContractIdentifier,
    ) -> Result<(), RPCError>;
    /// query the replica for a list of chunks
    fn list_chunks(&mut self) -> Result<Vec<SlotMetadata>, RPCError>;
    /// query the replica for zero or more chunks
    fn get_chunks(
        &mut self,
        slots_and_versions: &[(u32, u32)],
    ) -> Result<Vec<Option<Vec<u8>>>, RPCError>;
    /// query the replica for zero or more latest chunks
    fn get_latest_chunks(&mut self, slot_ids: &[u32]) -> Result<Vec<Option<Vec<u8>>>, RPCError>;
    /// Upload a chunk to the stacker DB instance
    fn put_chunk(&mut self, chunk: &StackerDBChunkData) -> Result<StackerDBChunkAckData, RPCError>;

    /// Get a single chunk with the given version
    /// Returns Ok(Some(..)) if the chunk exists
    /// Returns Ok(None) if the chunk with the given version does not exist
    /// Returns Err(..) on transport error
    fn get_chunk(&mut self, slot_id: u32, version: u32) -> Result<Option<Vec<u8>>, RPCError> {
        let mut chunks = self.get_chunks(&[(slot_id, version)])?;
        // check if chunks is empty because [0] and remove(0) panic on out-of-bounds
        if chunks.is_empty() {
            return Ok(None);
        }
        // swap_remove breaks the ordering of latest_chunks, but we don't care because we
        //  only want the first element anyways.
        Ok(chunks.swap_remove(0))
    }

    /// Get a single latest chunk.
    /// Returns Ok(Some(..)) if the slot exists
    /// Returns Ok(None) if not
    /// Returns Err(..) on transport error
    fn get_latest_chunk(&mut self, slot_id: u32) -> Result<Option<Vec<u8>>, RPCError> {
        let mut latest_chunks = self.get_latest_chunks(&[slot_id])?;
        // check if latest_chunks is empty because [0] and remove(0) panic on out-of-bounds
        if latest_chunks.is_empty() {
            return Ok(None);
        }
        // swap_remove breaks the ordering of latest_chunks, but we don't care because we
        //  only want the first element anyways.
        Ok(latest_chunks.swap_remove(0))
    }

    /// Get a single latest chunk from the StackerDB and deserialize into `T` using the
    /// StacksMessageCodec.
    fn get_latest<T: StacksMessageCodec>(&mut self, slot_id: u32) -> Result<Option<T>, RPCError> {
        let Some(latest_bytes) = self.get_latest_chunk(slot_id)? else {
            return Ok(None);
        };
        Some(
            T::consensus_deserialize(&mut latest_bytes.as_slice()).map_err(|e| {
                let msg = format!("StacksMessageCodec::consensus_deserialize failure: {e}");
                RPCError::Deserialize(msg)
            }),
        )
        .transpose()
    }
}

/// An open connection to a node, tagged with the host it was opened to
#[derive(Debug)]
struct CachedConnection {
    /// The host this socket is connected to.  The socket is only ever used while this equals
    /// the session's current `host`.
    host: String,
    /// The connected socket, with read and write timeouts set
    sock: TcpStream,
}

/// signer session for a stackerdb instance
#[derive(Debug)]
pub struct StackerDBSession {
    /// host we're talking to
    pub host: String,
    /// contract we're talking to
    pub stackerdb_contract_id: QualifiedContractIdentifier,
    /// Connection to the replica, kept between requests while the node keeps it open
    conn: Option<CachedConnection>,
    /// The timeout applied to HTTP read and write operations
    socket_timeout: Duration,
}

/// Is `sock` still open on the peer's side, with no unsolicited bytes waiting?
/// An idle keep-alive connection has nothing to read, so anything else (EOF, an error, or pending
/// data) means the connection must not be reused.
fn socket_is_idle(sock: &TcpStream) -> bool {
    if sock.set_nonblocking(true).is_err() {
        return false;
    }
    let mut probe = [0u8; 1];
    let idle = matches!(sock.peek(&mut probe), Err(e) if e.kind() == io::ErrorKind::WouldBlock);
    idle && sock.set_nonblocking(false).is_ok()
}

/// Did the peer close the connection, as opposed to a timeout or a bad response?
fn is_peer_closed(err: &RPCError) -> bool {
    matches!(
        err,
        RPCError::IO(e) if matches!(
            e.kind(),
            io::ErrorKind::BrokenPipe
                | io::ErrorKind::ConnectionReset
                | io::ErrorKind::ConnectionAborted
                | io::ErrorKind::UnexpectedEof
        )
    )
}

impl StackerDBSession {
    /// instantiate but don't connect
    pub fn new(
        host: &str,
        stackerdb_contract_id: QualifiedContractIdentifier,
        socket_timeout: Duration,
    ) -> StackerDBSession {
        StackerDBSession {
            host: host.to_owned(),
            stackerdb_contract_id,
            conn: None,
            socket_timeout,
        }
    }

    /// connect or reconnect to the node.
    /// Any cached connection is dropped first, so a failed connect leaves none behind.
    fn connect_or_reconnect(&mut self) -> Result<(), RPCError> {
        self.conn = None;
        debug!("connect to {}", &self.host);
        let sock = TcpStream::connect(&self.host)?;
        // Make sure we don't hang forever if for some reason our node does not
        // respond as expected such as failing to properly close the connection
        sock.set_read_timeout(Some(self.socket_timeout))?;
        sock.set_write_timeout(Some(self.socket_timeout))?;
        self.conn = Some(CachedConnection {
            host: self.host.clone(),
            sock,
        });
        Ok(())
    }

    /// Make sure `self.conn` holds a usable connection to the current `self.host`.
    /// A cached socket is kept only if it was opened to `self.host` and is still idle and open;
    /// otherwise it is dropped and a new connection is made.
    /// Returns whether an existing connection was reused.
    fn ensure_connection(&mut self) -> Result<bool, RPCError> {
        if let Some(conn) = self.conn.take() {
            if conn.host != self.host {
                debug!("Target host changed, reconnecting"; "old_host" => &conn.host, "new_host" => &self.host);
            } else if !socket_is_idle(&conn.sock) {
                debug!("Cached connection is no longer usable, reconnecting"; "host" => &self.host);
            } else {
                self.conn = Some(conn);
                return Ok(true);
            }
        }
        self.connect_or_reconnect()?;
        Ok(false)
    }

    /// Send one request on the cached connection.
    /// The connection is kept for the next request only if the response left it reusable;
    /// on any error it is dropped.
    fn send_on_connection(
        &mut self,
        verb: &str,
        path: &str,
        content_type: Option<&str>,
        payload: &[u8],
    ) -> Result<Vec<u8>, RPCError> {
        let Some(mut conn) = self.conn.take() else {
            return Err(RPCError::NotConnected);
        };
        let resp = send_http_request(
            &mut conn.sock,
            &self.host,
            verb,
            path,
            content_type,
            payload,
            true,
        )?;
        if resp.reusable {
            self.conn = Some(conn);
        }
        if resp.status != 200 {
            return Err(RPCError::HttpError(resp.status));
        }
        Ok(resp.body)
    }

    /// send an HTTP RPC request and receive a reply.
    /// Return the HTTP reply, decoded if it was chunked
    ///
    /// The connection to the node is reused across requests.  If a reused connection turns out to
    /// have been closed by the node, a GET request is retried once on a fresh connection.  A POST
    /// is never retried, since the node may have processed it before the connection failed, and
    /// timeouts, HTTP errors and malformed responses are never retried.
    fn rpc_request(
        &mut self,
        verb: &str,
        path: &str,
        content_type: Option<&str>,
        payload: &[u8],
    ) -> Result<Vec<u8>, RPCError> {
        let reused = self.ensure_connection()?;
        let res = self.send_on_connection(verb, path, content_type, payload);
        match res {
            Err(ref e) if reused && verb == "GET" && is_peer_closed(e) => {
                debug!("Reused connection was closed by the node, retrying"; "host" => &self.host, "error" => %e);
                self.connect_or_reconnect()?;
                self.send_on_connection(verb, path, content_type, payload)
            }
            res => res,
        }
    }
}

impl SignerSession for StackerDBSession {
    /// connect to the replica
    fn connect(
        &mut self,
        host: String,
        stackerdb_contract_id: QualifiedContractIdentifier,
    ) -> Result<(), RPCError> {
        self.host = host;
        self.stackerdb_contract_id = stackerdb_contract_id;
        self.connect_or_reconnect()
    }

    /// query the replica for a list of chunks
    fn list_chunks(&mut self) -> Result<Vec<SlotMetadata>, RPCError> {
        let bytes = self.rpc_request(
            "GET",
            &stackerdb_get_metadata_path(self.stackerdb_contract_id.clone()),
            None,
            &[],
        )?;
        let metadata: Vec<SlotMetadata> = serde_json::from_slice(&bytes)
            .map_err(|e| RPCError::Deserialize(format!("{:?}", e)))?;
        Ok(metadata)
    }

    /// query the replica for zero or more chunks
    fn get_chunks(
        &mut self,
        slots_and_versions: &[(u32, u32)],
    ) -> Result<Vec<Option<Vec<u8>>>, RPCError> {
        let mut payloads = vec![];
        for (slot_id, slot_version) in slots_and_versions.iter() {
            let path = stackerdb_get_chunk_path(
                self.stackerdb_contract_id.clone(),
                *slot_id,
                Some(*slot_version),
            );
            let chunk = match self.rpc_request("GET", &path, None, &[]) {
                Ok(body_bytes) => Some(body_bytes),
                Err(RPCError::HttpError(code)) => {
                    if code != 404 {
                        return Err(RPCError::HttpError(code));
                    }
                    None
                }
                Err(e) => {
                    return Err(e);
                }
            };
            payloads.push(chunk);
        }
        Ok(payloads)
    }

    /// query the replica for zero or more latest chunks
    fn get_latest_chunks(&mut self, slot_ids: &[u32]) -> Result<Vec<Option<Vec<u8>>>, RPCError> {
        let mut payloads = vec![];
        let limit = if self.stackerdb_contract_id.name.starts_with("signer") {
            SIGNERS_STACKERDB_CHUNK_SIZE
        } else {
            usize::try_from(STACKERDB_MAX_CHUNK_SIZE)
                .expect("infallible: StackerDB chunk size exceeds usize::MAX")
        };
        for slot_id in slot_ids.iter() {
            let path = stackerdb_get_chunk_path(self.stackerdb_contract_id.clone(), *slot_id, None);
            let chunk = match self.rpc_request("GET", &path, None, &[]) {
                Ok(body_bytes) => {
                    // Verify that the chunk is not too large
                    if body_bytes.len() > limit {
                        None
                    } else {
                        Some(body_bytes)
                    }
                }
                Err(RPCError::HttpError(code)) => {
                    if code != 404 {
                        return Err(RPCError::HttpError(code));
                    }
                    None
                }
                Err(e) => {
                    return Err(e);
                }
            };
            payloads.push(chunk);
        }
        Ok(payloads)
    }

    /// upload a chunk
    fn put_chunk(&mut self, chunk: &StackerDBChunkData) -> Result<StackerDBChunkAckData, RPCError> {
        let body =
            serde_json::to_vec(chunk).map_err(|e| RPCError::Deserialize(format!("{e:?}")))?;
        let path = stackerdb_post_chunk_path(self.stackerdb_contract_id.clone());
        let resp_bytes = self.rpc_request("POST", &path, Some("application/json"), &body)?;
        let ack: StackerDBChunkAckData = serde_json::from_slice(&resp_bytes)
            .map_err(|e| RPCError::Deserialize(format!("{e:?}")))?;
        Ok(ack)
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;
    use std::thread::JoinHandle;

    use super::*;

    const TIMEOUT: Duration = Duration::from_secs(5);

    /// Read one HTTP request (head and `Content-Length` body) off of `stream`.
    /// Returns the request line and the body, or None if the peer closed first.
    fn read_request(stream: &mut TcpStream) -> Option<(String, Vec<u8>)> {
        let mut head = vec![];
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            match stream.read(&mut byte) {
                Ok(1) => head.push(byte[0]),
                _ => return None,
            }
        }
        let head = String::from_utf8(head).unwrap();
        let len = head
            .lines()
            .find_map(|l| l.strip_prefix("Content-Length: "))
            .map(|v| v.parse::<usize>().unwrap())
            .unwrap_or(0);
        let mut body = vec![0u8; len];
        stream.read_exact(&mut body).unwrap();
        Some((head.lines().next().unwrap().to_string(), body))
    }

    /// Write a keep-alive reply framed by `Content-Length`.
    fn reply_sized(stream: &mut TcpStream, status: &str, extra_hdrs: &str, body: &str) {
        write!(
            stream,
            "HTTP/1.1 {status}\r\n{extra_hdrs}Content-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
    }

    /// Write a keep-alive reply framed by chunked transfer encoding.
    fn reply_chunked(stream: &mut TcpStream, body: &str) {
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{body}\r\n0\r\n\r\n",
            body.len()
        )
        .unwrap();
    }

    fn local_listener() -> (TcpListener, String) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind failed");
        let addr = listener.local_addr().unwrap().to_string();
        (listener, addr)
    }

    fn new_session(host: &str) -> StackerDBSession {
        StackerDBSession::new(host, QualifiedContractIdentifier::transient(), TIMEOUT)
    }

    /// Assert that no further connection is pending on `listener`.
    fn assert_no_more_connections(listener: &TcpListener) {
        listener.set_nonblocking(true).unwrap();
        thread::sleep(Duration::from_millis(100));
        match listener.accept() {
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            other => panic!("unexpected extra connection: {other:?}"),
        }
    }

    fn ack_body() -> &'static str {
        "{\"accepted\":true}"
    }

    #[test]
    fn socket_timeout_works_as_expected() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind failed");
        let addr = listener.local_addr().unwrap();

        let short_timeout = Duration::from_millis(200);
        thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                // Sleep long enough so the client should hit its timeout
                std::thread::sleep(short_timeout * 2);
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\n\r\n");
            }
        });

        let contract_id = QualifiedContractIdentifier::transient();
        let mut session = StackerDBSession::new(&addr.to_string(), contract_id, short_timeout);

        session.connect_or_reconnect().expect("connect failed");

        // This should fail due to the timeout
        let result = session.rpc_request("GET", "/", None, &[]);
        match result {
            Err(RPCError::IO(e)) => {
                assert_eq!(e.kind(), std::io::ErrorKind::WouldBlock);
            }
            other => panic!("expected timeout error, got {other:?}"),
        }
    }

    /// Several requests, answered with `Content-Length`, chunked and 404 replies, all travel over
    /// one connection, and the 404 still maps to `None`.
    #[test]
    fn sequential_requests_reuse_one_connection() {
        let (listener, addr) = local_listener();
        let server: JoinHandle<TcpListener> = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_request(&mut stream).unwrap();
            reply_sized(&mut stream, "200 OK", "", "[]");
            read_request(&mut stream).unwrap();
            reply_chunked(&mut stream, "abc");
            read_request(&mut stream).unwrap();
            reply_sized(&mut stream, "404 Not Found", "", "nope");
            read_request(&mut stream).unwrap();
            reply_sized(&mut stream, "200 OK", "", ack_body());
            listener
        });

        let mut session = new_session(&addr);
        assert!(session.list_chunks().unwrap().is_empty());
        assert_eq!(session.get_latest_chunk(0).unwrap(), Some(b"abc".to_vec()));
        assert_eq!(session.get_latest_chunk(1).unwrap(), None);
        let ack = session
            .put_chunk(&StackerDBChunkData::new(0, 1, vec![1, 2, 3]))
            .unwrap();
        assert!(ack.accepted);

        let listener = server.join().unwrap();
        assert_no_more_connections(&listener);
    }

    /// When the node closes the connection between requests, both a GET and a POST reconnect
    /// before sending anything, so the POST is received exactly once.
    #[test]
    fn reconnects_after_server_closes_between_requests() {
        let (listener, addr) = local_listener();
        let server: JoinHandle<usize> = thread::spawn(move || {
            let mut posts = 0;
            // three connections, each served once and then closed by the node
            for _ in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                let (line, _) = read_request(&mut stream).unwrap();
                if line.starts_with("POST") {
                    posts += 1;
                    reply_sized(&mut stream, "200 OK", "", ack_body());
                } else {
                    reply_sized(&mut stream, "200 OK", "", "[]");
                }
            }
            posts
        });

        let mut session = new_session(&addr);
        session.list_chunks().unwrap();
        thread::sleep(Duration::from_millis(100));
        session.list_chunks().unwrap();
        thread::sleep(Duration::from_millis(100));
        let ack = session
            .put_chunk(&StackerDBChunkData::new(0, 1, vec![1, 2, 3]))
            .unwrap();
        assert!(ack.accepted);
        assert_eq!(server.join().unwrap(), 1);
    }

    /// A POST whose response is lost (the node processed it, then closed) is not replayed.
    #[test]
    fn post_is_not_replayed_after_lost_response() {
        let (listener, addr) = local_listener();
        let server: JoinHandle<(usize, bool)> = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_request(&mut stream).unwrap();
            reply_sized(&mut stream, "200 OK", "", "[]");
            let (line, _) = read_request(&mut stream).unwrap();
            assert!(line.starts_with("POST"));
            // read the whole POST and close without replying
            drop(stream);
            listener.set_nonblocking(true).unwrap();
            thread::sleep(Duration::from_millis(300));
            (1, listener.accept().is_err())
        });

        let mut session = new_session(&addr);
        session.list_chunks().unwrap();
        let res = session.put_chunk(&StackerDBChunkData::new(0, 1, vec![1, 2, 3]));
        assert!(res.is_err(), "expected error, got {res:?}");
        let (posts, no_replay) = server.join().unwrap();
        assert_eq!(posts, 1);
        assert!(no_replay, "POST was replayed on a new connection");
    }

    /// A GET on a reused connection that the node drops mid-request is retried once; if the
    /// retry fails too, the error is returned and nothing more is sent.
    #[test]
    fn get_is_retried_once() {
        let (listener, addr) = local_listener();
        let server: JoinHandle<TcpListener> = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_request(&mut stream).unwrap();
            reply_sized(&mut stream, "200 OK", "", "[]");
            // drop the next GET on the reused connection, then drop its retry too
            read_request(&mut stream).unwrap();
            drop(stream);
            let (mut stream, _) = listener.accept().unwrap();
            read_request(&mut stream).unwrap();
            drop(stream);
            listener
        });

        let mut session = new_session(&addr);
        session.list_chunks().unwrap();
        assert!(session.list_chunks().is_err());
        let listener = server.join().unwrap();
        assert_no_more_connections(&listener);
    }

    /// A reply with `Connection: close` makes the next request use a new connection.
    #[test]
    fn connection_close_reply_forces_new_connection() {
        let (listener, addr) = local_listener();
        let server: JoinHandle<TcpListener> = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_request(&mut stream).unwrap();
            reply_sized(&mut stream, "200 OK", "Connection: close\r\n", "[]");
            // keep the connection open: the client must not wait on or reuse it
            let (mut stream2, _) = listener.accept().unwrap();
            read_request(&mut stream2).unwrap();
            reply_sized(&mut stream2, "200 OK", "", "[]");
            drop(stream);
            listener
        });

        let mut session = new_session(&addr);
        session.list_chunks().unwrap();
        session.list_chunks().unwrap();
        let listener = server.join().unwrap();
        assert_no_more_connections(&listener);
    }

    /// Serve one keep-alive request on `listener`, then report how many bytes the client sends on
    /// that connection afterwards before closing it (0 if it just closes).
    fn serve_once_then_count_stray_bytes(
        listener: TcpListener,
    ) -> JoinHandle<(usize, TcpListener)> {
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(TIMEOUT)).unwrap();
            read_request(&mut stream).unwrap();
            reply_sized(&mut stream, "200 OK", "", "[]");
            let mut rest = vec![];
            let _ = stream.read_to_end(&mut rest);
            (rest.len(), listener)
        })
    }

    /// Assigning the public `host` field directly redirects the next GET and POST to the new
    /// node, and the old node receives nothing further.
    #[test]
    fn direct_host_change_reconnects() {
        let (listener_a, addr_a) = local_listener();
        let (listener_b, addr_b) = local_listener();
        let server_a = serve_once_then_count_stray_bytes(listener_a);
        let server_b: JoinHandle<Vec<String>> = thread::spawn(move || {
            let (mut stream, _) = listener_b.accept().unwrap();
            let mut lines = vec![];
            let (line, _) = read_request(&mut stream).unwrap();
            lines.push(line);
            reply_sized(&mut stream, "200 OK", "", "[]");
            let (line, _) = read_request(&mut stream).unwrap();
            lines.push(line);
            reply_sized(&mut stream, "200 OK", "", ack_body());
            lines
        });

        let mut session = new_session(&addr_a);
        session.list_chunks().unwrap();
        session.host = addr_b;
        session.list_chunks().unwrap();
        session
            .put_chunk(&StackerDBChunkData::new(0, 1, vec![1, 2, 3]))
            .unwrap();

        let lines = server_b.join().unwrap();
        assert!(lines[0].starts_with("GET"));
        assert!(lines[1].starts_with("POST"));
        let (stray, listener_a) = server_a.join().unwrap();
        assert_eq!(stray, 0, "old node received more request bytes");
        assert_no_more_connections(&listener_a);
    }

    /// If connecting to a new target fails, nothing is cached and the old node is not used.
    #[test]
    fn failed_target_change_does_not_use_old_connection() {
        let (listener_a, addr_a) = local_listener();
        let (listener_b, addr_b) = local_listener();
        drop(listener_b);
        let server_a = serve_once_then_count_stray_bytes(listener_a);

        let mut session = new_session(&addr_a);
        session.list_chunks().unwrap();
        assert!(session
            .connect(addr_b, QualifiedContractIdentifier::transient())
            .is_err());
        assert!(session.list_chunks().is_err());

        let (stray, listener_a) = server_a.join().unwrap();
        assert_eq!(stray, 0, "old node received more request bytes");
        assert_no_more_connections(&listener_a);
    }
}
