//! Hand-rolled, protobuf-wire-compatible codec for lowlat's `ClientMessage` /
//! `ServerMessage` (a Rust mirror of lowlat's `exchange.wire.FastWire`).
//!
//! Wire contract (see `lowlat/src/main/proto/exchange.proto`):
//!   * a frame is `big-endian int32 length | protobuf payload`;
//!   * `ClientMessage.new_order` = field 1, `.cancel` = field 2, `.heartbeat` = field 3;
//!   * `ServerMessage.ack` = 1, `.exec` = 2, `.trade` = 3, `.book` = 4, `.heartbeat` = 5.
//!
//! Prices are fixed point in 0.01 EUR/MWh, quantities in 0.1 MW, and prices must
//! sit on the 0.10 EUR/MWh tick grid or the exchange rejects them.

pub const PRICE_SCALE: i64 = 100; // 0.01 EUR/MWh
pub const QTY_SCALE: i64 = 10; // 0.1 MW
pub const TICK: i64 = 10; // 0.10 EUR/MWh

pub const SIDE_BUY: i32 = 1;
pub const SIDE_SELL: i32 = 2;

pub const ORD_LIMIT: i32 = 1;
pub const ORD_MARKET: i32 = 2;

pub const TIF_GTC: i32 = 1;
pub const TIF_IOC: i32 = 2;
pub const TIF_FOK: i32 = 3;

pub const ST_ACCEPTED: i32 = 1;
pub const ST_REJECTED: i32 = 2;
pub const ST_PARTIAL: i32 = 3;
pub const ST_FILLED: i32 = 4;
pub const ST_CANCELLED: i32 = 5;
pub const ST_EXPIRED: i32 = 6;

/// A status after which an order no longer rests in the book. `ACCEPTED` and
/// `PARTIAL` are *not* terminal — the remainder is still live.
pub fn is_terminal_status(status: i32) -> bool {
    matches!(status, ST_REJECTED | ST_FILLED | ST_CANCELLED | ST_EXPIRED)
}

/// EUR/MWh -> scaled price (nearest 0.01).
pub fn eur_to_price(eur_per_mwh: f64) -> i64 {
    (eur_per_mwh * PRICE_SCALE as f64).round() as i64
}

/// Scaled price -> EUR/MWh.
pub fn price_to_eur(price: i64) -> f64 {
    price as f64 / PRICE_SCALE as f64
}

/// MW -> scaled quantity (nearest 0.1).
pub fn mw_to_qty(mw: f64) -> i64 {
    (mw * QTY_SCALE as f64).round() as i64
}

/// Scaled quantity -> MW.
pub fn qty_to_mw(qty: i64) -> f64 {
    qty as f64 / QTY_SCALE as f64
}

// ---------------------------------------------------------------- primitives

fn write_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn tag(out: &mut Vec<u8>, field: u32, wire_type: u32) {
    write_varint(out, ((field as u64) << 3) | wire_type as u64);
}

fn field_int64(out: &mut Vec<u8>, field: u32, value: i64) {
    tag(out, field, 0);
    write_varint(out, value as u64);
}

fn field_int32(out: &mut Vec<u8>, field: u32, value: i32) {
    // protobuf int32 negatives are sign-extended to 64 bits (10-byte varint).
    tag(out, field, 0);
    write_varint(out, value as i64 as u64);
}

fn field_bool(out: &mut Vec<u8>, field: u32, value: bool) {
    tag(out, field, 0);
    out.push(if value { 1 } else { 0 });
}

fn field_bytes(out: &mut Vec<u8>, field: u32, bytes: &[u8]) {
    tag(out, field, 2);
    write_varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

// ------------------------------------------------------------------ encoders

/// `ClientMessage{ new_order = NewOrder{...} }` (unframed).
pub fn new_order(
    client_order_id: u64,
    product_id: i32,
    side: i32,
    ord_type: i32,
    tif: i32,
    price: i64,
    quantity: i64,
) -> Vec<u8> {
    let mut nested = Vec::with_capacity(48);
    field_int64(&mut nested, 1, client_order_id as i64);
    field_int32(&mut nested, 2, product_id);
    field_int32(&mut nested, 3, side);
    field_int32(&mut nested, 4, ord_type);
    field_int32(&mut nested, 5, tif);
    field_int64(&mut nested, 6, price);
    field_int64(&mut nested, 7, quantity);

    let mut msg = Vec::with_capacity(nested.len() + 4);
    field_bytes(&mut msg, 1, &nested);
    msg
}

/// `ClientMessage{ cancel = CancelOrder{...} }` (unframed).
pub fn cancel(client_order_id: u64, product_id: i32, exchange_order_id: u64) -> Vec<u8> {
    let mut nested = Vec::with_capacity(32);
    field_int64(&mut nested, 1, client_order_id as i64);
    field_int32(&mut nested, 2, product_id);
    field_int64(&mut nested, 3, exchange_order_id as i64);

    let mut msg = Vec::with_capacity(nested.len() + 4);
    field_bytes(&mut msg, 2, &nested);
    msg
}

/// `ClientMessage{ heartbeat = Heartbeat{...} }` (unframed).
pub fn heartbeat(client_nonce: u64, send_time_ns: i64) -> Vec<u8> {
    let mut nested = Vec::with_capacity(24);
    field_int64(&mut nested, 1, client_nonce as i64);
    field_int64(&mut nested, 2, send_time_ns);

    let mut msg = Vec::with_capacity(nested.len() + 4);
    field_bytes(&mut msg, 3, &nested);
    msg
}

/// Prefix an unframed payload with the big-endian int32 length.
pub fn frame(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&(payload.len() as i32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

// ------------------------------------------------------------------ decoders

#[derive(Debug, Clone, PartialEq)]
pub struct Fill {
    pub trade_id: u64,
    pub exchange_order_id: u64,
    pub maker_order_id: u64,
    pub price: i64,
    pub quantity: i64,
    pub ts_ns: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ServerMsg {
    Ack {
        client_order_id: u64,
        exchange_order_id: u64,
        accepted: bool,
        reason: String,
        recv_ns: i64,
    },
    Exec {
        client_order_id: u64,
        exchange_order_id: u64,
        product_id: i32,
        status: i32,
        leaves_quantity: i64,
        filled_quantity: i64,
        fill: Option<Fill>,
        ts_ns: i64,
    },
    Trade {
        trade_id: u64,
        product_id: i32,
        price: i64,
        quantity: i64,
        aggressor_side: i32,
        ts_ns: i64,
    },
    Book {
        product_id: i32,
        bid_price: i64,
        bid_quantity: i64,
        ask_price: i64,
        ask_quantity: i64,
        ts_ns: i64,
    },
    Heartbeat,
    Unknown,
}

struct Cur<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cur<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Cur { buf, pos: 0 }
    }
    fn done(&self) -> bool {
        self.pos >= self.buf.len()
    }
    fn varint(&mut self) -> Option<u64> {
        let mut value: u64 = 0;
        let mut shift = 0;
        loop {
            let byte = *self.buf.get(self.pos)?;
            self.pos += 1;
            value |= ((byte & 0x7f) as u64) << shift;
            if byte & 0x80 == 0 {
                return Some(value);
            }
            shift += 7;
            if shift > 63 {
                return None;
            }
        }
    }
    fn skip(&mut self, wire_type: u32) -> Option<()> {
        match wire_type {
            0 => {
                self.varint()?;
            }
            1 => self.pos += 8,
            2 => {
                let len = self.varint()? as usize;
                self.pos += len;
            }
            5 => self.pos += 4,
            _ => return None,
        }
        Some(())
    }
    fn len_delimited(&mut self) -> Option<&'a [u8]> {
        let len = self.varint()? as usize;
        let start = self.pos;
        let end = start + len;
        if end > self.buf.len() {
            return None;
        }
        self.pos = end;
        Some(&self.buf[start..end])
    }
}

fn parse_fill(body: &[u8]) -> Fill {
    let mut f = Fill {
        trade_id: 0,
        exchange_order_id: 0,
        maker_order_id: 0,
        price: 0,
        quantity: 0,
        ts_ns: 0,
    };
    let mut c = Cur::new(body);
    while !c.done() {
        let tag = match c.varint() {
            Some(t) => t,
            None => break,
        };
        let field = (tag >> 3) as u32;
        let wt = (tag & 7) as u32;
        if wt != 0 {
            if c.skip(wt).is_none() {
                break;
            }
            continue;
        }
        let value = match c.varint() {
            Some(v) => v as i64,
            None => break,
        };
        match field {
            1 => f.trade_id = value as u64,
            3 => f.maker_order_id = value as u64,
            4 => f.price = value,
            5 => f.quantity = value,
            6 => f.ts_ns = value,
            _ => {}
        }
    }
    f
}

/// Parse one unframed `ServerMessage` payload.
pub fn parse_server(payload: &[u8]) -> ServerMsg {
    let mut c = Cur::new(payload);
    while !c.done() {
        let tag = match c.varint() {
            Some(t) => t,
            None => break,
        };
        let field = (tag >> 3) as u32;
        let wt = (tag & 7) as u32;
        if wt != 2 {
            if c.skip(wt).is_none() {
                break;
            }
            continue;
        }
        let body = match c.len_delimited() {
            Some(b) => b,
            None => break,
        };
        match field {
            1 => return parse_ack(body),
            2 => return parse_exec(body),
            3 => return parse_trade(body),
            4 => return parse_book(body),
            5 => return ServerMsg::Heartbeat,
            _ => {}
        }
    }
    ServerMsg::Unknown
}

fn parse_ack(body: &[u8]) -> ServerMsg {
    let mut client_order_id = 0u64;
    let mut exchange_order_id = 0u64;
    let mut accepted = false;
    let mut reason = String::new();
    let mut recv_ns = 0i64;
    let mut c = Cur::new(body);
    while !c.done() {
        let tag = match c.varint() {
            Some(t) => t,
            None => break,
        };
        let field = (tag >> 3) as u32;
        let wt = (tag & 7) as u32;
        if field == 4 && wt == 2 {
            if let Some(bytes) = c.len_delimited() {
                reason = String::from_utf8_lossy(bytes).into_owned();
            }
            continue;
        }
        if wt != 0 {
            if c.skip(wt).is_none() {
                break;
            }
            continue;
        }
        let value = match c.varint() {
            Some(v) => v as i64,
            None => break,
        };
        match field {
            1 => client_order_id = value as u64,
            2 => exchange_order_id = value as u64,
            3 => accepted = value != 0,
            5 => recv_ns = value,
            _ => {}
        }
    }
    ServerMsg::Ack {
        client_order_id,
        exchange_order_id,
        accepted,
        reason,
        recv_ns,
    }
}

fn parse_exec(body: &[u8]) -> ServerMsg {
    let mut client_order_id = 0u64;
    let mut exchange_order_id = 0u64;
    let mut product_id = 0i32;
    let mut status = 0i32;
    let mut leaves = 0i64;
    let mut filled = 0i64;
    let mut fill = None;
    let mut ts_ns = 0i64;
    let mut c = Cur::new(body);
    while !c.done() {
        let tag = match c.varint() {
            Some(t) => t,
            None => break,
        };
        let field = (tag >> 3) as u32;
        let wt = (tag & 7) as u32;
        if field == 7 && wt == 2 {
            if let Some(sub) = c.len_delimited() {
                fill = Some(parse_fill(sub));
            }
            continue;
        }
        if wt != 0 {
            if c.skip(wt).is_none() {
                break;
            }
            continue;
        }
        let value = match c.varint() {
            Some(v) => v as i64,
            None => break,
        };
        match field {
            1 => client_order_id = value as u64,
            2 => exchange_order_id = value as u64,
            3 => product_id = value as i32,
            4 => status = value as i32,
            5 => leaves = value,
            6 => filled = value,
            8 => ts_ns = value,
            _ => {}
        }
    }
    ServerMsg::Exec {
        client_order_id,
        exchange_order_id,
        product_id,
        status,
        leaves_quantity: leaves,
        filled_quantity: filled,
        fill,
        ts_ns,
    }
}

fn parse_trade(body: &[u8]) -> ServerMsg {
    let mut m = ServerMsg::Trade {
        trade_id: 0,
        product_id: 0,
        price: 0,
        quantity: 0,
        aggressor_side: 0,
        ts_ns: 0,
    };
    let mut c = Cur::new(body);
    while !c.done() {
        let tag = match c.varint() {
            Some(t) => t,
            None => break,
        };
        let field = (tag >> 3) as u32;
        let wt = (tag & 7) as u32;
        if wt != 0 {
            if c.skip(wt).is_none() {
                break;
            }
            continue;
        }
        let value = match c.varint() {
            Some(v) => v as i64,
            None => break,
        };
        if let ServerMsg::Trade {
            trade_id,
            product_id,
            price,
            quantity,
            aggressor_side,
            ts_ns,
        } = &mut m
        {
            match field {
                1 => *trade_id = value as u64,
                2 => *product_id = value as i32,
                3 => *price = value,
                4 => *quantity = value,
                5 => *aggressor_side = value as i32,
                6 => *ts_ns = value,
                _ => {}
            }
        }
    }
    m
}

fn parse_book(body: &[u8]) -> ServerMsg {
    let mut m = ServerMsg::Book {
        product_id: 0,
        bid_price: 0,
        bid_quantity: 0,
        ask_price: 0,
        ask_quantity: 0,
        ts_ns: 0,
    };
    let mut c = Cur::new(body);
    while !c.done() {
        let tag = match c.varint() {
            Some(t) => t,
            None => break,
        };
        let field = (tag >> 3) as u32;
        let wt = (tag & 7) as u32;
        if wt != 0 {
            if c.skip(wt).is_none() {
                break;
            }
            continue;
        }
        let value = match c.varint() {
            Some(v) => v as i64,
            None => break,
        };
        if let ServerMsg::Book {
            product_id,
            bid_price,
            bid_quantity,
            ask_price,
            ask_quantity,
            ts_ns,
        } = &mut m
        {
            match field {
                1 => *product_id = value as i32,
                2 => *bid_price = value,
                3 => *bid_quantity = value,
                4 => *ask_price = value,
                5 => *ask_quantity = value,
                6 => *ts_ns = value,
                _ => {}
            }
        }
    }
    m
}

/// Incremental frame decoder: feed socket bytes, get whole messages back.
#[derive(Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
}

impl FrameDecoder {
    pub fn new() -> Self {
        FrameDecoder { buf: Vec::new() }
    }

    pub fn push(&mut self, data: &[u8]) -> Vec<ServerMsg> {
        self.buf.extend_from_slice(data);
        let mut out = Vec::new();
        loop {
            if self.buf.len() < 4 {
                break;
            }
            let len = i32::from_be_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]);
            if len < 0 {
                break;
            }
            let len = len as usize;
            if self.buf.len() < 4 + len {
                break;
            }
            let payload = self.buf[4..4 + len].to_vec();
            self.buf.drain(0..4 + len);
            out.push(parse_server(&payload));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_order_roundtrips_through_our_own_decoder_shape() {
        // Encode a NewOrder as a *server-agnostic* check on field layout: the
        // outer tag must be field 1, wire type 2, followed by the nested length.
        let payload = new_order(7, 3, SIDE_BUY, ORD_LIMIT, TIF_GTC, eur_to_price(50.0), 10);
        assert_eq!(payload[0], (1 << 3) | 2, "ClientMessage.new_order tag");
        // The nested NewOrder starts after tag(1) + len varint.
        let nested_len = payload[1] as usize;
        assert_eq!(payload.len(), 2 + nested_len);
    }

    #[test]
    fn frame_prefix_is_big_endian_length() {
        let payload = heartbeat(1, 2);
        let framed = frame(&payload);
        assert_eq!(&framed[0..4], &(payload.len() as i32).to_be_bytes());
        assert_eq!(&framed[4..], &payload[..]);
    }

    #[test]
    fn price_helpers_roundtrip_on_the_tick_grid() {
        assert_eq!(eur_to_price(230.0), 23000);
        assert_eq!(price_to_eur(23000), 230.0);
        assert_eq!(eur_to_price(-50.0), -5000);
        assert_eq!(mw_to_qty(1.0), 10);
        assert_eq!(qty_to_mw(10), 1.0);
    }

    #[test]
    fn decodes_an_ack_like_fastwire_emits_it() {
        // Build the bytes the Java ServerEncoder.ack would produce:
        // ServerMessage{ack = OrderAck{client=5, exch=9, accepted=true, recv=1234}}
        let mut nested = Vec::new();
        field_int64(&mut nested, 1, 5);
        field_int64(&mut nested, 2, 9);
        field_bool(&mut nested, 3, true);
        field_int64(&mut nested, 5, 1234);
        let mut msg = Vec::new();
        field_bytes(&mut msg, 1, &nested);

        match parse_server(&msg) {
            ServerMsg::Ack {
                client_order_id,
                exchange_order_id,
                accepted,
                recv_ns,
                ..
            } => {
                assert_eq!(client_order_id, 5);
                assert_eq!(exchange_order_id, 9);
                assert!(accepted);
                assert_eq!(recv_ns, 1234);
            }
            other => panic!("expected ack, got {other:?}"),
        }
    }

    #[test]
    fn decodes_a_trade_print() {
        let mut nested = Vec::new();
        field_int64(&mut nested, 1, 42); // trade_id
        field_int32(&mut nested, 2, 1); // product
        field_int64(&mut nested, 3, 23000); // price
        field_int64(&mut nested, 4, 10); // qty
        field_int32(&mut nested, 5, SIDE_SELL); // aggressor
        field_int64(&mut nested, 6, 99); // ts
        let mut msg = Vec::new();
        field_bytes(&mut msg, 3, &nested);

        match parse_server(&msg) {
            ServerMsg::Trade {
                trade_id,
                price,
                quantity,
                aggressor_side,
                ..
            } => {
                assert_eq!(trade_id, 42);
                assert_eq!(price, 23000);
                assert_eq!(quantity, 10);
                assert_eq!(aggressor_side, SIDE_SELL);
            }
            other => panic!("expected trade, got {other:?}"),
        }
    }

    #[test]
    fn accepted_and_partial_orders_are_not_terminal() {
        // Regression: an ACCEPTED execution report was once mistaken for a
        // terminal status, so resting orders were dropped from client-side
        // tracking and never cancelled.
        assert!(!is_terminal_status(ST_ACCEPTED));
        assert!(!is_terminal_status(ST_PARTIAL));
        assert!(is_terminal_status(ST_REJECTED));
        assert!(is_terminal_status(ST_FILLED));
        assert!(is_terminal_status(ST_CANCELLED));
        assert!(is_terminal_status(ST_EXPIRED));
    }

    #[test]
    fn frame_decoder_handles_split_and_coalesced_frames() {
        // ServerMessage.heartbeat is field 5 (ClientMessage.heartbeat is field 3).
        fn server_heartbeat_frame() -> Vec<u8> {
            let nested: Vec<u8> = Vec::new();
            let mut msg = Vec::new();
            field_bytes(&mut msg, 5, &nested);
            frame(&msg)
        }
        let a = server_heartbeat_frame();
        let b = server_heartbeat_frame();
        let mut dec = FrameDecoder::new();
        // split the first frame across two pushes
        assert!(dec.push(&a[0..3]).is_empty());
        let mut msgs = dec.push(&a[3..]);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs.pop(), Some(ServerMsg::Heartbeat));
        // two frames coalesced in one push
        let mut both = b.clone();
        both.extend_from_slice(&a);
        let msgs = dec.push(&both);
        assert_eq!(msgs, vec![ServerMsg::Heartbeat, ServerMsg::Heartbeat]);
    }
}
