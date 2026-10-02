//! A minimal MQTT 3.1.1 broker stub for tests: accepts one client, answers
//! CONNECT/SUBSCRIBE/PING and records every PUBLISH (topic, retain, payload).
#![allow(dead_code)]

use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Clone, Debug, PartialEq)]
pub struct Published {
    pub topic: String,
    pub retain: bool,
    pub payload: Vec<u8>,
}

pub struct FakeBroker {
    pub port: u16,
    pub published: Arc<Mutex<Vec<Published>>>,
    pub connects: Arc<Mutex<usize>>,
    /// topic filters clients subscribed to
    pub subscribed: Arc<Mutex<Vec<String>>>,
    /// messages pushed to connected clients (see `send`)
    inject: tokio::sync::broadcast::Sender<Vec<u8>>,
}

async fn read_packet_r(s: &mut tokio::net::tcp::OwnedReadHalf) -> Option<(u8, Vec<u8>)> {
    let first = s.read_u8().await.ok()?;
    let mut len = 0usize;
    let mut mult = 1;
    loop {
        let b = s.read_u8().await.ok()?;
        len += (b & 0x7f) as usize * mult;
        if b & 0x80 == 0 { break; }
        mult *= 128;
    }
    let mut body = vec![0u8; len];
    s.read_exact(&mut body).await.ok()?;
    Some((first, body))
}

impl FakeBroker {
    pub async fn start() -> FakeBroker {
        Self::start_on(0).await
    }
    pub async fn start_on(port: u16) -> FakeBroker {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let published = Arc::new(Mutex::new(Vec::new()));
        let connects = Arc::new(Mutex::new(0));
        let subscribed = Arc::new(Mutex::new(Vec::new()));
        let (inject, _) = tokio::sync::broadcast::channel::<Vec<u8>>(16);
        let (p2, c2, s2, i2) = (published.clone(), connects.clone(), subscribed.clone(), inject.clone());
        tokio::spawn(async move {
            loop {
                let Ok((s, _)) = listener.accept().await else { return };
                let (p, c, sub) = (p2.clone(), c2.clone(), s2.clone());
                let mut inj = i2.subscribe();
                let (mut rd, wr) = s.into_split();
                let wr = Arc::new(tokio::sync::Mutex::new(wr));
                let wr2 = wr.clone();
                tokio::spawn(async move {
                    while let Ok(pkt) = inj.recv().await {
                        if wr2.lock().await.write_all(&pkt).await.is_err() { return; }
                    }
                });
                tokio::spawn(async move {
                    while let Some((first, body)) = read_packet_r(&mut rd).await {
                        let mut s = wr.lock().await;
                        match first >> 4 {
                            1 => { *c.lock().unwrap() += 1; let _ = s.write_all(&[0x20, 2, 0, 0]).await; }
                            3 => {
                                let qos = (first >> 1) & 3;
                                let tlen = u16::from_be_bytes([body[0], body[1]]) as usize;
                                let topic = String::from_utf8_lossy(&body[2..2 + tlen]).to_string();
                                let mut pos = 2 + tlen;
                                if qos > 0 {
                                    let id = [body[pos], body[pos + 1]];
                                    pos += 2;
                                    let _ = s.write_all(&[0x40, 2, id[0], id[1]]).await;
                                }
                                p.lock().unwrap().push(Published { topic, retain: first & 1 == 1, payload: body[pos..].to_vec() });
                            }
                            8 => {
                                // SUBSCRIBE: packet id, then (topic, qos) pairs
                                let mut pos = 2;
                                while pos + 2 <= body.len() {
                                    let tlen = u16::from_be_bytes([body[pos], body[pos + 1]]) as usize;
                                    sub.lock().unwrap().push(String::from_utf8_lossy(&body[pos + 2..pos + 2 + tlen]).to_string());
                                    pos += 3 + tlen;
                                }
                                let _ = s.write_all(&[0x90, 3, body[0], body[1], 0]).await;
                            }
                            12 => { let _ = s.write_all(&[0xD0, 0]).await; }
                            14 => return,
                            _ => {}
                        }
                    }
                });
            }
        });
        FakeBroker { port, published, connects, subscribed, inject }
    }

    /// Publish (QoS 0) to every connected client.
    pub async fn send(&self, topic: &str, payload: &[u8]) {
        let mut body = (topic.len() as u16).to_be_bytes().to_vec();
        body.extend_from_slice(topic.as_bytes());
        body.extend_from_slice(payload);
        let mut pkt = vec![0x30];
        let mut len = body.len();
        loop {
            let mut b = (len % 128) as u8;
            len /= 128;
            if len > 0 { b |= 0x80; }
            pkt.push(b);
            if len == 0 { break; }
        }
        pkt.extend_from_slice(&body);
        let _ = self.inject.send(pkt);
    }

    pub async fn wait_subscribed(&self, topic: &str, secs: u64) {
        for _ in 0..secs * 20 {
            if self.subscribed.lock().unwrap().iter().any(|t| t == topic) { return; }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("nobody subscribed to {topic}");
    }

    pub fn topics(&self) -> Vec<String> {
        self.published.lock().unwrap().iter().map(|p| p.topic.clone()).collect()
    }
    pub fn last(&self, topic: &str) -> Option<Published> {
        self.published.lock().unwrap().iter().rev().find(|p| p.topic == topic).cloned()
    }
    pub async fn wait_for(&self, topic: &str, secs: u64) -> Published {
        for _ in 0..secs * 20 {
            if let Some(p) = self.last(topic) { return p; }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("no publish on {topic}; got {:?}", self.topics());
    }
}
