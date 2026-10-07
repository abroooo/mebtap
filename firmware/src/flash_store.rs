//! `SampleStore` backed by a file on internal flash (LittleFS/SPIFFS), stored as
//! newline-delimited JSON. SD-card swap-in later keeps this same trait.
//!
//! BENCH: mount the VFS partition (esp_idf_svc littlefs/spiffs) before first use
//! so the configured path is writable. The file I/O itself is plain `std::fs`.

use std::collections::VecDeque;
use std::fs;
use std::io::Write;

use collector::{Sample, SampleStore};

pub struct FlashStore {
    path: String,
    buf: VecDeque<Sample>,
}

impl FlashStore {
    pub fn mount(path: &str) -> anyhow::Result<Self> {
        // BENCH: mount LittleFS/SPIFFS here, e.g.
        //   esp_idf_svc::fs::littlefs::Littlefs::new(partition)?;
        // then ensure the parent dir of `path` exists on the VFS.
        let buf = load(path).unwrap_or_default();
        Ok(Self { path: path.to_string(), buf })
    }

    /// Put samples back at the front after a failed uplink (store-and-forward).
    pub fn requeue(&mut self, samples: &[Sample]) {
        for s in samples.iter().rev() {
            self.buf.push_front(s.clone());
        }
        self.persist();
    }

    pub fn pending(&self) -> usize {
        self.buf.len()
    }

    fn persist(&self) {
        if let Ok(mut f) = fs::File::create(&self.path) {
            for s in &self.buf {
                if let Ok(line) = serde_json::to_string(s) {
                    let _ = writeln!(f, "{line}");
                }
            }
        }
    }
}

impl SampleStore for FlashStore {
    fn append(&mut self, samples: &[Sample]) {
        self.buf.extend(samples.iter().cloned());
        self.persist();
    }

    fn drain_batch(&mut self, max: usize) -> Vec<Sample> {
        let n = max.min(self.buf.len());
        let out: Vec<Sample> = self.buf.drain(0..n).collect();
        self.persist();
        out
    }

    fn pending(&self) -> usize {
        self.buf.len()
    }
}

fn load(path: &str) -> anyhow::Result<VecDeque<Sample>> {
    let data = fs::read_to_string(path)?;
    let mut buf = VecDeque::new();
    for line in data.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(s) = serde_json::from_str::<Sample>(line) {
            buf.push_back(s);
        }
    }
    Ok(buf)
}
