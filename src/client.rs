//! Blocking TCP client for lowlat's gateway.
//!
//! One `LowlatClient` owns one socket and one session id at the exchange. The
//! read side is polled with a short timeout so a maker loop can interleave
//! "quote, poll, repeat" without threads.

use std::io::{ErrorKind, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::wire::{self, FrameDecoder, ServerMsg};

pub fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

pub struct LowlatClient {
    stream: TcpStream,
    decoder: FrameDecoder,
    next_client_order_id: u64,
}

impl LowlatClient {
    pub fn connect(host: &str, port: u16) -> std::io::Result<Self> {
        let stream = TcpStream::connect((host, port))?;
        stream.set_nodelay(true)?;
        // A short read timeout turns `poll` into a non-blocking drain.
        stream.set_read_timeout(Some(Duration::from_millis(50)))?;
        Ok(LowlatClient {
            stream,
            decoder: FrameDecoder::new(),
            next_client_order_id: 0,
        })
    }

    fn send(&mut self, payload: &[u8]) -> std::io::Result<()> {
        let framed = wire::frame(payload);
        self.stream.write_all(&framed)
    }

    /// Post a limit order. Returns the client order id assigned.
    pub fn new_order(
        &mut self,
        product_id: i32,
        side: i32,
        ord_type: i32,
        tif: i32,
        price: i64,
        quantity: i64,
    ) -> std::io::Result<u64> {
        self.next_client_order_id += 1;
        let id = self.next_client_order_id;
        self.send(&wire::new_order(
            id, product_id, side, ord_type, tif, price, quantity,
        ))?;
        Ok(id)
    }

    /// Cancel by exchange order id. Returns the client order id assigned.
    pub fn cancel(&mut self, product_id: i32, exchange_order_id: u64) -> std::io::Result<u64> {
        self.next_client_order_id += 1;
        let id = self.next_client_order_id;
        self.send(&wire::cancel(id, product_id, exchange_order_id))?;
        Ok(id)
    }

    pub fn heartbeat(&mut self, nonce: u64) -> std::io::Result<()> {
        self.send(&wire::heartbeat(nonce, now_ns()))
    }

    /// Drain everything currently readable. Never blocks longer than the read
    /// timeout. Returns `Err` only on a real socket failure or clean EOF.
    pub fn poll(&mut self) -> std::io::Result<Vec<ServerMsg>> {
        let mut buf = [0u8; 1 << 16];
        match self.stream.read(&mut buf) {
            Ok(0) => Err(std::io::Error::new(
                ErrorKind::UnexpectedEof,
                "exchange closed the connection",
            )),
            Ok(n) => Ok(self.decoder.push(&buf[..n])),
            Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {
                Ok(Vec::new())
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }
}
