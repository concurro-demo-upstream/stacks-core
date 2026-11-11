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

//! HTTP response decoding and synchronous requests for signer RPC clients.

use std::io;
use std::io::{Read, Write};

use hashbrown::HashMap;
use stacks_common::codec::MAX_MESSAGE_LEN;
use stacks_common::deps_common::httparse;
use stacks_common::util::chunked_encoding::*;

use crate::error::RPCError;

pub const MAX_HTTP_HEADERS: usize = 32;
pub const MAX_HTTP_HEADER_LEN: usize = 4096;

/// The validated head (status line and headers) of an HTTP response
struct HttpHead {
    /// HTTP status code
    status: u32,
    /// HTTP minor version (0 for HTTP/1.0, 1 for HTTP/1.1)
    version: u8,
    /// Response headers, keyed by lower-cased name
    headers: HashMap<String, String>,
}

/// Parse and validate the head of an HTTP response.
///
/// Returns `Ok(None)` if `payload` does not yet contain a complete head, and otherwise the head
/// and the offset into `payload` at which the body starts.
/// If `require_ok` is set, a status code other than 200 is returned as
/// RPCError::HttpError(..) before the version and headers are validated.
fn parse_http_head(
    payload: &[u8],
    require_ok: bool,
) -> Result<Option<(HttpHead, usize)>, RPCError> {
    // realistically, there won't be more than 32 headers
    let mut headers_buf = [httparse::EMPTY_HEADER; MAX_HTTP_HEADERS];
    let mut resp = httparse::Response::new(&mut headers_buf);

    let body_offset = match resp.parse(payload) {
        Ok(httparse::Status::Complete(body_offset)) => body_offset,
        Ok(httparse::Status::Partial) => return Ok(None),
        Err(_) => {
            return Err(RPCError::Deserialize(
                "Failed to decode HTTP headers".to_string(),
            ));
        }
    };
    let Some(status) = resp.code else {
        return Err(RPCError::MalformedResponse(
            "No HTTP status code returned".to_string(),
        ));
    };
    if require_ok && status != 200 {
        return Err(RPCError::HttpError(status.into()));
    }
    let Some(version) = resp.version else {
        return Err(RPCError::MalformedResponse(
            "No HTTP version given".to_string(),
        ));
    };
    if version != 0 && version != 1 {
        return Err(RPCError::MalformedResponse(format!(
            "Unrecognized HTTP code {version}"
        )));
    }
    let mut headers: HashMap<String, String> = HashMap::new();
    for i in 0..resp.headers.len() {
        let value = String::from_utf8(resp.headers[i].value.to_vec()).map_err(|_e| {
            RPCError::MalformedResponse("Invalid HTTP header value: not utf-8".to_string())
        })?;
        if !value.is_ascii() {
            return Err(RPCError::MalformedResponse(
                "Invalid HTTP response: header value is not ASCII-US".to_string(),
            ));
        }
        if value.len() > MAX_HTTP_HEADER_LEN {
            return Err(RPCError::MalformedResponse(
                "Invalid HTTP response: header value is too big".to_string(),
            ));
        }

        let key = resp.headers[i].name.to_string().to_lowercase();
        if headers.contains_key(&key) {
            return Err(RPCError::MalformedResponse(format!(
                "Invalid HTTP respuest: duplicate header \"{key}\""
            )));
        }
        headers.insert(key, value);
    }
    Ok(Some((
        HttpHead {
            status: status.into(),
            version,
            headers,
        },
        body_offset,
    )))
}

/// Decode the HTTP response payload into its headers and body.  Only used by tests; sessions use
/// `read_http_response()`.
/// Return the offset into payload where the body starts, and a table of headers.
///
/// If the payload contains a status code other than 200, then RPCError::HttpError(..) will be
/// returned with the status code.
/// If the payload is missing necessary data, then RPCError::MalformedResponse(..) will be
/// returned, with a human-readable reason string.
/// If the payload does not contain a full HTTP header list, then RPCError::Deserialize(..) will be
/// returned.  This can happen if there are more than MAX_HTTP_HEADERS in the payload, for example.
#[cfg(test)]
pub fn decode_http_response(payload: &[u8]) -> Result<(HashMap<String, String>, usize), RPCError> {
    match parse_http_head(payload, true)? {
        Some((head, body_offset)) => Ok((head.headers, body_offset)),
        None => Err(RPCError::Deserialize(
            "Failed to decode HTTP headers".to_string(),
        )),
    }
}

/// Decode an HTTP body, given the headers.
#[cfg(test)]
pub fn decode_http_body(headers: &HashMap<String, String>, mut buf: &[u8]) -> io::Result<Vec<u8>> {
    let chunked = if let Some(val) = headers.get("transfer-encoding") {
        val == "chunked"
    } else {
        false
    };

    let body = if chunked {
        // chunked encoding
        let ptr = &mut buf;
        let mut fd = HttpChunkedTransferReader::from_reader(ptr, MAX_MESSAGE_LEN.into());
        let mut decoded_body = vec![];
        fd.read_to_end(&mut decoded_body)?;
        decoded_body
    } else {
        // body is just as-is
        buf.to_vec()
    };

    Ok(body)
}

/// A single HTTP response read off of a socket by `read_http_response()`
#[derive(Debug)]
pub struct HttpResponse {
    /// HTTP status code
    pub status: u32,
    /// Response body, decoded if it was chunked
    pub body: Vec<u8>,
    /// Whether the connection can carry another request: the response was HTTP/1.1 without
    /// `Connection: close`, its body was fully framed, and no bytes followed it.
    pub reusable: bool,
}

/// Does the `Connection` header ask for the connection to be closed?
fn connection_close(headers: &HashMap<String, String>) -> bool {
    headers.get("connection").is_some_and(|v| {
        v.split(',')
            .any(|token| token.trim().eq_ignore_ascii_case("close"))
    })
}

/// Read exactly one HTTP response from `sock`, without reading past its end when the body is
/// framed by `Transfer-Encoding: chunked` or `Content-Length`, so the socket can carry another
/// request.  Reads from `sock` are unbuffered.  A response with neither is read until EOF.  Any status code is accepted; it is
/// reported in the result.
///
/// The head is limited to `MAX_HTTP_HEADERS * MAX_HTTP_HEADER_LEN` bytes and the body to
/// `MAX_MESSAGE_LEN` bytes.
///
/// # Errors
///
/// - `RPCError::IO` with `UnexpectedEof` if the peer closed before sending any response byte.
/// - `RPCError::MalformedResponse` if the head is oversized, the response is truncated, or the
///   `Content-Length` is invalid or exceeds `MAX_MESSAGE_LEN`.
/// - Other `RPCError`s as for `decode_http_response()`, and any IO error from `sock`.
///
/// A chunked or unframed response that ends right after its head (with no body bytes) yields an
/// empty body and is not reusable.
pub fn read_http_response<S: Read>(sock: &mut S) -> Result<HttpResponse, RPCError> {
    // Read the head one byte at a time so that we never consume bytes past the end of this
    // response's head; heads are small, so this costs at most MAX_HTTP_HEADERS * MAX_HTTP_HEADER_LEN
    // single-byte reads.
    let max_head_len = MAX_HTTP_HEADERS * MAX_HTTP_HEADER_LEN;
    let mut buf: Vec<u8> = vec![];
    let head = loop {
        if buf.len() >= max_head_len {
            return Err(RPCError::MalformedResponse(
                "HTTP response head is too big".to_string(),
            ));
        }
        let mut byte = [0u8; 1];
        match sock.read(&mut byte) {
            Ok(1) => buf.push(byte[0]),
            Ok(_) => {
                if buf.is_empty() {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "connection closed before HTTP response",
                    )
                    .into());
                }
                return Err(RPCError::MalformedResponse(
                    "Truncated HTTP response head".to_string(),
                ));
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
        if buf.ends_with(b"\r\n\r\n") || buf.ends_with(b"\n\n") {
            match parse_http_head(&buf, false)? {
                Some((head, _)) => break head,
                None => {
                    return Err(RPCError::MalformedResponse(
                        "Incomplete HTTP response head".to_string(),
                    ))
                }
            }
        }
    };

    let reader = &mut *sock;
    let mut framed = false;
    let body = if head
        .headers
        .get("transfer-encoding")
        .is_some_and(|v| v.eq_ignore_ascii_case("chunked"))
    {
        let mut state = HttpChunkedTransferReaderState::new(MAX_MESSAGE_LEN.into());
        let mut body = vec![];
        let mut total_consumed = 0;
        let mut tmp = [0u8; 4096];
        loop {
            let (decoded, consumed) = state.do_read(reader, &mut tmp)?;
            body.extend_from_slice(&tmp[..decoded]);
            total_consumed += consumed;
            if state.is_eof() {
                framed = true;
                break;
            }
            if decoded == 0 && consumed == 0 {
                break;
            }
        }
        if !framed && total_consumed > 0 {
            return Err(RPCError::MalformedResponse(
                "Truncated chunked HTTP body".to_string(),
            ));
        }
        body
    } else if let Some(len) = head.headers.get("content-length") {
        let len: usize = len.trim().parse().map_err(|_| {
            RPCError::MalformedResponse("Invalid HTTP response: bad Content-Length".to_string())
        })?;
        if len > usize::try_from(MAX_MESSAGE_LEN).unwrap_or(usize::MAX) {
            return Err(RPCError::MalformedResponse(
                "Invalid HTTP response: Content-Length is too big".to_string(),
            ));
        }
        let mut body = vec![];
        reader.by_ref().take(len as u64).read_to_end(&mut body)?;
        if body.len() < len {
            return Err(RPCError::MalformedResponse(
                "Truncated HTTP body".to_string(),
            ));
        }
        framed = true;
        body
    } else {
        let limit = u64::from(MAX_MESSAGE_LEN);
        let mut body = vec![];
        reader.by_ref().take(limit + 1).read_to_end(&mut body)?;
        if body.len() as u64 > limit {
            return Err(RPCError::MalformedResponse(
                "HTTP body exceeds maximum expected length".to_string(),
            ));
        }
        body
    };

    let reusable = framed && head.version == 1 && !connection_close(&head.headers);
    if body.is_empty() {
        debug!("No HTTP body");
        debug!("Headers: {:?}", &head.headers);
    }
    Ok(HttpResponse {
        status: head.status,
        body,
        reusable,
    })
}

/// Send an HTTP request through the given read/write handle and read the one response to it.
/// If `keep_alive` is false, the request carries `Connection: close`.
///
/// Any status code is accepted; see `read_http_response()` for the response contract and errors.
pub fn send_http_request<S: Read + Write>(
    sock: &mut S,
    host: &str,
    verb: &str,
    path: &str,
    content_type: Option<&str>,
    payload: &[u8],
    keep_alive: bool,
) -> Result<HttpResponse, RPCError> {
    let content_length_hdr = if !payload.is_empty() {
        format!("Content-Length: {}\r\n", payload.len())
    } else {
        "".to_string()
    };
    let connection_hdr = if keep_alive {
        ""
    } else {
        "Connection: close\r\n"
    };

    let req_txt = if let Some(content_type) = content_type {
        format!(
            "{verb} {path} HTTP/1.1\r\nHost: {host}\r\n{connection_hdr}Content-Type: {content_type}\r\n{content_length_hdr}User-Agent: libsigner/0.1\r\nAccept: */*\r\n\r\n"
        )
    } else {
        format!(
            "{verb} {path} HTTP/1.1\r\nHost: {host}\r\n{connection_hdr}{content_length_hdr}User-Agent: libsigner/0.1\r\nAccept: */*\r\n\r\n"
        )
    };
    debug!("HTTP request\n{}", &req_txt);

    sock.write_all(req_txt.as_bytes())?;
    sock.write_all(payload)?;

    read_http_response(sock)
}

/// Run an HTTP request, synchronously, through the given read/write handle
/// Return the HTTP reply, decoded if it was chunked
///
/// The request carries `Connection: close`.  A status code other than 200 is returned as
/// RPCError::HttpError(..).
#[cfg(test)]
pub fn run_http_request<S: Read + Write>(
    sock: &mut S,
    host: &str,
    verb: &str,
    path: &str,
    content_type: Option<&str>,
    payload: &[u8],
) -> Result<Vec<u8>, RPCError> {
    let resp = send_http_request(sock, host, verb, path, content_type, payload, false)?;
    if resp.status != 200 {
        return Err(RPCError::HttpError(resp.status));
    }
    Ok(resp.body)
}
