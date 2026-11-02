//! Datagram kinds and connection-handshake packets.
//!
//! Handshake (challenge/response stops address spoofing from claiming slots):
//! client --ConnectRequest(client_salt, name)--> server
//! client <--Challenge(client_salt, server_salt)-- server
//! client --ChallengeResponse(token = client_salt ^ server_salt)--> server
//! client <--Accepted(token, client_id, max_clients)-- server
//! after which both sides exchange `KIND_DATA` packets tagged with `token`.

use crate::wire::{DecodeError, DecodeResult, Reader, Writer};

pub const PROTOCOL_MAGIC: u32 = u32::from_le_bytes(*b"NOL1");
pub const PROTOCOL_VERSION: u16 = 1;

pub const KIND_CONNECT_REQUEST: u8 = 1;
pub const KIND_CHALLENGE: u8 = 2;
pub const KIND_CHALLENGE_RESPONSE: u8 = 3;
pub const KIND_ACCEPTED: u8 = 4;
pub const KIND_DENIED: u8 = 5;
pub const KIND_DISCONNECT: u8 = 6;
pub const KIND_DATA: u8 = 7;
/// Data packet whose ack fields must be ignored (sender has not received anything yet).
pub const KIND_DATA_NOACK: u8 = 8;

/// kind + token + seq + ack + ack_bits
pub const DATA_HEADER_LEN: usize = 1 + 8 + 2 + 2 + 4;
/// Connect requests are padded so a server reply is never larger than the request
/// (no UDP amplification).
pub const CONNECT_REQUEST_LEN: usize = 128;
pub const MAX_NAME_LEN: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum DenyReason {
    ServerFull = 1,
    VersionMismatch = 2,
    Rejected = 3,
}

impl DenyReason {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::ServerFull,
            2 => Self::VersionMismatch,
            _ => Self::Rejected,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Control {
    ConnectRequest { version: u16, client_salt: u64, name: String },
    Challenge { client_salt: u64, server_salt: u64 },
    ChallengeResponse { token: u64 },
    Accepted { token: u64, client_id: u16, max_clients: u16 },
    Denied { client_salt: u64, reason: DenyReason },
    Disconnect { token: u64 },
}

impl Control {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::with_capacity(CONNECT_REQUEST_LEN);
        match self {
            Control::ConnectRequest { version, client_salt, name } => {
                let name = truncate_utf8(name, MAX_NAME_LEN);
                w.u8(KIND_CONNECT_REQUEST).u32(PROTOCOL_MAGIC).u16(*version).u64(*client_salt);
                w.str(name);
                w.buf.resize(CONNECT_REQUEST_LEN, 0);
            }
            Control::Challenge { client_salt, server_salt } => {
                w.u8(KIND_CHALLENGE).u64(*client_salt).u64(*server_salt);
            }
            Control::ChallengeResponse { token } => {
                w.u8(KIND_CHALLENGE_RESPONSE).u64(*token);
                w.buf.resize(CONNECT_REQUEST_LEN, 0);
            }
            Control::Accepted { token, client_id, max_clients } => {
                w.u8(KIND_ACCEPTED).u64(*token).u16(*client_id).u16(*max_clients);
            }
            Control::Denied { client_salt, reason } => {
                w.u8(KIND_DENIED).u64(*client_salt).u8(*reason as u8);
            }
            Control::Disconnect { token } => {
                w.u8(KIND_DISCONNECT).u64(*token);
            }
        }
        w.finish()
    }

    /// Decode a control packet. Data packets return `Ok(None)`.
    pub fn decode(data: &[u8]) -> DecodeResult<Option<Control>> {
        let mut r = Reader::new(data);
        let c = match r.u8()? {
            KIND_CONNECT_REQUEST => {
                if data.len() < CONNECT_REQUEST_LEN || r.u32()? != PROTOCOL_MAGIC {
                    return Err(DecodeError);
                }
                let version = r.u16()?;
                let client_salt = r.u64()?;
                let name = r.str()?;
                if name.len() > MAX_NAME_LEN {
                    return Err(DecodeError);
                }
                Control::ConnectRequest { version, client_salt, name: name.to_owned() }
            }
            KIND_CHALLENGE => Control::Challenge { client_salt: r.u64()?, server_salt: r.u64()? },
            KIND_CHALLENGE_RESPONSE => {
                if data.len() < CONNECT_REQUEST_LEN {
                    return Err(DecodeError);
                }
                Control::ChallengeResponse { token: r.u64()? }
            }
            KIND_ACCEPTED => Control::Accepted {
                token: r.u64()?,
                client_id: r.u16()?,
                max_clients: r.u16()?,
            },
            KIND_DENIED => Control::Denied {
                client_salt: r.u64()?,
                reason: DenyReason::from_u8(r.u8()?),
            },
            KIND_DISCONNECT => Control::Disconnect { token: r.u64()? },
            KIND_DATA | KIND_DATA_NOACK => return Ok(None),
            _ => return Err(DecodeError),
        };
        Ok(Some(c))
    }
}

/// Token carried by a data packet, without decoding the rest.
pub fn data_token(data: &[u8]) -> Option<u64> {
    match data.first() {
        Some(&KIND_DATA) | Some(&KIND_DATA_NOACK) if data.len() >= DATA_HEADER_LEN => {
            Some(u64::from_le_bytes(data[1..9].try_into().unwrap()))
        }
        _ => None,
    }
}

fn truncate_utf8(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_roundtrip() {
        let all = [
            Control::ConnectRequest { version: 1, client_salt: 42, name: "wizard".into() },
            Control::Challenge { client_salt: 1, server_salt: 2 },
            Control::ChallengeResponse { token: 3 },
            Control::Accepted { token: 3, client_id: 2, max_clients: 8 },
            Control::Denied { client_salt: 9, reason: DenyReason::ServerFull },
            Control::Disconnect { token: 3 },
        ];
        for c in all {
            assert_eq!(Control::decode(&c.encode()).unwrap(), Some(c));
        }
    }

    #[test]
    fn requests_are_not_smaller_than_replies() {
        let req = Control::ConnectRequest { version: 1, client_salt: 0, name: String::new() };
        let ch = Control::Challenge { client_salt: 0, server_salt: 0 };
        assert!(req.encode().len() >= ch.encode().len());
    }

    #[test]
    fn long_name_truncated_on_char_boundary() {
        let name = "é".repeat(40); // 80 bytes
        let enc = Control::ConnectRequest { version: 1, client_salt: 0, name }.encode();
        match Control::decode(&enc).unwrap() {
            Some(Control::ConnectRequest { name, .. }) => assert_eq!(name.len(), 64),
            other => panic!("{other:?}"),
        }
    }
}
