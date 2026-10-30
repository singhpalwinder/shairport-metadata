//! Port of utils/controlLights.py over a single persistent MQTT connection.
//! Device state topics (zigbee2mqtt/<dev>) stay subscribed and every device
//! is asked for its state on (re)connect, so the latest state of each light is
//! always cached and a snapshot is just a copy of that cache. The snapshot is
//! mirrored to SNAPSHOT_PATH so it survives a restart mid-session.

use crate::config::{Config, LIGHT_TOPICS, SNAPSHOT_PATH};
use rumqttc::{Client, Event, MqttOptions, Outgoing, Packet, QoS, SubscribeFilter};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Default)]
struct State {
    curr: HashMap<String, Value>, // device -> last payload from zigbee2mqtt/<device>
    subscribed_at: Option<Instant>, // when state topics were subscribed on this connection
}

/// How long after subscribing to give devices to answer the initial /get.
const PRIME_WAIT: Duration = Duration::from_secs(1);

pub struct Lights {
    client: Client,
    shared: Arc<(Mutex<State>, Condvar)>,
    snapshots: HashMap<String, Map<String, Value>>,
    have_snapshot: bool, // set even if no device reported, so we don't retry every event
    stopping: Arc<AtomicBool>,
    done: mpsc::Receiver<()>,
}

type Snapshots = HashMap<String, Map<String, Value>>;

fn load_snapshot() -> Option<Snapshots> {
    let s = std::fs::read_to_string(SNAPSHOT_PATH).ok()?;
    serde_json::from_str(&s).ok()
}

fn device(topic: &str) -> &str {
    topic
        .strip_prefix("zigbee2mqtt/")
        .and_then(|t| t.split('/').next())
        .unwrap_or(topic)
}

fn devices() -> impl Iterator<Item = &'static str> {
    LIGHT_TOPICS.iter().map(|t| device(t))
}

impl Lights {
    pub fn connect(cfg: &Config) -> Self {
        let mut opts = MqttOptions::new(format!("shairport-metadata-rs-{}", std::process::id()), &cfg.mqtt_host, cfg.mqtt_port);
        opts.set_credentials(&cfg.mqtt_username, &cfg.mqtt_password);
        opts.set_keep_alive(Duration::from_secs(30));

        let (client, mut connection) = Client::new(opts, 64);
        let shared = Arc::new((Mutex::new(State::default()), Condvar::new()));

        let sub_client = client.clone();
        let st = shared.clone();
        let stopping = Arc::new(AtomicBool::new(false));
        let stop = stopping.clone();
        let (done_tx, done) = mpsc::channel();
        thread::Builder::new()
            .name("mqtt".into())
            .spawn(move || {
                for ev in connection.iter() {
                    match ev {
                        Ok(Event::Incoming(Packet::ConnAck(_))) => {
                            println!("MQTT connected");
                            st.0.lock().unwrap().subscribed_at = None;
                            let filters = devices()
                                .map(|d| SubscribeFilter::new(format!("zigbee2mqtt/{d}"), QoS::AtMostOnce));
                            let _ = sub_client.try_subscribe_many(filters);
                        }
                        Ok(Event::Incoming(Packet::SubAck(_))) => {
                            let get = json!({"state": "", "brightness": "", "color": "", "color_temp": ""});
                            for dev in devices() {
                                let _ = sub_client.try_publish(
                                    format!("zigbee2mqtt/{dev}/get"),
                                    QoS::AtMostOnce,
                                    false,
                                    get.to_string(),
                                );
                            }
                            let (lock, cv) = &*st;
                            lock.lock().unwrap().subscribed_at = Some(Instant::now());
                            cv.notify_all();
                        }
                        Ok(Event::Incoming(Packet::Publish(p))) => {
                            let Ok(payload) = serde_json::from_slice::<Value>(&p.payload) else { continue };
                            let dev = device(&p.topic).to_string();
                            st.0.lock().unwrap().curr.insert(dev, payload);
                        }
                        // requests are sent in order, so everything queued before
                        // the disconnect has gone out
                        Ok(Event::Outgoing(Outgoing::Disconnect)) => break,
                        Ok(_) => {}
                        Err(e) => {
                            if stop.load(Ordering::Relaxed) {
                                break;
                            }
                            eprintln!("MQTT connection error: {e}; retrying");
                            thread::sleep(Duration::from_secs(2));
                        }
                    }
                }
                let _ = done_tx.send(());
            })
            .expect("spawn mqtt thread");

        let snapshots = load_snapshot();
        if snapshots.is_some() {
            println!("Loaded light snapshot from {SNAPSHOT_PATH}");
        }
        Self {
            client,
            shared,
            have_snapshot: snapshots.is_some(),
            snapshots: snapshots.unwrap_or_default(),
            stopping,
            done,
        }
    }

    /// Flush queued publishes and disconnect, waiting at most `timeout`.
    pub fn shutdown(self, timeout: Duration) {
        self.stopping.store(true, Ordering::Relaxed);
        let _ = self.client.try_disconnect();
        if self.done.recv_timeout(timeout).is_err() {
            eprintln!("MQTT did not flush within {timeout:?}");
        }
    }

    fn publish(&self, topic: &str, payload: &Value) {
        if let Err(e) = self.client.try_publish(topic, QoS::AtMostOnce, false, payload.to_string()) {
            eprintln!("MQTT publish to {topic} failed: {e}");
        }
    }

    pub fn has_snapshot(&self) -> bool {
        self.have_snapshot
    }

    /// Remember the current state of every light for restore_states().
    /// Only waits (up to `timeout`) right after startup/reconnect, while the
    /// initial state reports are still coming in.
    pub fn snapshot_states(&mut self, timeout: Duration) {
        let (lock, cv) = &*self.shared;
        let deadline = Instant::now() + timeout;
        let mut s = lock.lock().unwrap();
        loop {
            let ready_at = s.subscribed_at.map(|t| t + PRIME_WAIT);
            let now = Instant::now();
            if ready_at.is_some_and(|r| now >= r) || now >= deadline {
                break;
            }
            let until = ready_at.map_or(deadline, |r| r.min(deadline));
            s = cv.wait_timeout(s, until - now).unwrap().0;
        }

        self.have_snapshot = true;
        self.snapshots.clear();
        for dev in devices() {
            let Some(p) = s.curr.get(dev) else { continue };
            let mut snap = Map::new();
            snap.insert("state".into(), p.get("state").cloned().unwrap_or(json!("OFF")));
            if let Some(b) = p.get("brightness") {
                snap.insert("brightness".into(), b.clone());
            }
            match p.get("color") {
                Some(c) if c.as_object().is_some_and(|o| !o.is_empty()) => {
                    snap.insert("color".into(), c.clone());
                }
                _ => {
                    if let Some(ct) = p.get("color_temp") {
                        snap.insert("color_temp".into(), ct.clone());
                    }
                }
            }
            self.snapshots.insert(dev.to_string(), snap);
        }
        if let Err(e) = std::fs::write(SNAPSHOT_PATH, serde_json::to_string(&self.snapshots).unwrap()) {
            eprintln!("Failed to save light snapshot: {e}");
        }
    }

    /// Push the saved snapshot back; OFF devices only get turned off.
    pub fn restore_states(&mut self) {
        self.have_snapshot = false;
        let _ = std::fs::remove_file(SNAPSHOT_PATH);
        if self.snapshots.is_empty() {
            println!("No snapshots to restore.");
            return;
        }
        for topic in LIGHT_TOPICS {
            let Some(snap) = self.snapshots.get(device(topic)) else { continue };
            let on = snap
                .get("state")
                .and_then(Value::as_str)
                .is_some_and(|s| s.eq_ignore_ascii_case("ON"));
            let payload = if on {
                let mut p = Map::new();
                p.insert("state".into(), json!("ON"));
                for k in ["brightness", "color", "color_temp"] {
                    if let Some(v) = snap.get(k) {
                        p.insert(k.into(), v.clone());
                    }
                }
                Value::Object(p)
            } else {
                json!({"state": "OFF"})
            };
            self.publish(topic, &payload);
        }
        self.snapshots.clear();
    }

    pub fn set_color(&self, (r, g, b): (u8, u8, u8)) {
        let payload = json!({"state": "ON", "brightness": 255, "color": {"r": r, "g": g, "b": b}});
        println!("setting playbars to: {payload}");
        for topic in LIGHT_TOPICS {
            self.publish(topic, &payload);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::device;

    #[test]
    fn device_from_topic() {
        assert_eq!(device("zigbee2mqtt/Desk/set"), "Desk");
        assert_eq!(device("zigbee2mqtt/bedLeft"), "bedLeft");
    }
}
