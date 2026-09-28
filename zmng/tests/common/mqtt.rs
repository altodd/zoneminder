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
}

async fn read_packet(s: &mut tokio::net::TcpStream) -> Option<(u8, Vec<u8>)> {
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
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let published = Arc::new(Mutex::new(Vec::new()));
        let connects = Arc::new(Mutex::new(0));
        let (p2, c2) = (published.clone(), connects.clone());
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = listener.accept().await else { return };
                let (p, c) = (p2.clone(), c2.clone());
                tokio::spawn(async move {
                    while let Some((first, body)) = read_packet(&mut s).await {
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
                            8 => { let _ = s.write_all(&[0x90, 3, body[0], body[1], 0]).await; }
                            12 => { let _ = s.write_all(&[0xD0, 0]).await; }
                            14 => return,
                            _ => {}
                        }
                    }
                });
            }
        });
        FakeBroker { port, published, connects }
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
