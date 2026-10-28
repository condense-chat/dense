//! The offline usage queue: an append-only JSONL file under the daemon dir,
//! capped at [`MAX_RECORDS`] (oldest dropped first), drained to
//! `POST /v1/devices/usage` in batches of [`BATCH`]. The server dedupes on
//! `(device_id, message_id)`, so a batch resent after a crash is harmless.

use std::future::Future;
use std::io::Write;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::api::Api;
use crate::api::devices;
use crate::config::Config;
use crate::error::Context;

pub const BATCH: usize = 500;
pub const MAX_RECORDS: usize = 20_000;

pub struct Queue {
    cap: usize,
    path: PathBuf,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct UsageRecord {
    pub cache_read: u64,
    pub cache_write: u64,
    pub input_tokens: u64,
    pub message_id: String,
    pub model: String,
    /// RFC 3339 timestamp of the message.
    pub occurred_at: String,
    pub output_tokens: u64,
    pub session_ref: String,
}

impl Queue {
    pub fn at(path: PathBuf, cap: usize) -> Self {
        Self { cap, path }
    }

    pub fn len(&self) -> usize {
        self.lines().len()
    }

    pub fn open(cfg: &Config) -> Self {
        Self::at(cfg.daemon_dir().join("usage-queue.jsonl"), MAX_RECORDS)
    }

    /// Append `records`, then drop the oldest lines beyond the cap.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "producer lands with offline usage (IMIR-1034)")
    )]
    pub fn push(&self, records: &[UsageRecord]) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).ctx(format!("creating {}", parent.display()))?;
        }
        let mut buf = String::new();
        for r in records {
            buf.push_str(&serde_json::to_string(r).ctx("serializing usage record")?);
            buf.push('\n');
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .ctx(format!("opening {}", self.path.display()))?;
        file.write_all(buf.as_bytes())
            .ctx(format!("appending to {}", self.path.display()))?;
        drop(file);
        let lines = self.lines();
        if lines.len() > self.cap {
            self.rewrite(&lines[lines.len() - self.cap..])?;
        }
        Ok(())
    }

    fn drop_front(&self, n: usize) -> Result<()> {
        let lines = self.lines();
        self.rewrite(&lines[n.min(lines.len())..])
    }

    fn lines(&self) -> Vec<String> {
        std::fs::read_to_string(&self.path)
            .map(|s| {
                s.lines()
                    .filter(|l| !l.trim().is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The next up-to-`n` queued lines: the records that parse, and how many
    /// lines were consumed (a corrupt line is skipped, then dropped with them).
    fn peek(&self, n: usize) -> (Vec<UsageRecord>, usize) {
        let lines = self.lines();
        let take = n.min(lines.len());
        let records = lines[..take]
            .iter()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        (records, take)
    }

    fn rewrite(&self, lines: &[String]) -> Result<()> {
        let tmp = self.path.with_extension("jsonl.tmp");
        let mut body = lines.join("\n");
        if !body.is_empty() {
            body.push('\n');
        }
        std::fs::write(&tmp, body).ctx(format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path).ctx(format!("replacing {}", self.path.display()))
    }
}

/// Drain the queue through `send`, one [`BATCH`] at a time; a batch leaves
/// the queue only after `send` succeeds. Returns how many records were sent.
pub async fn flush<F, Fut>(queue: &Queue, mut send: F) -> Result<usize>
where
    F: FnMut(Vec<UsageRecord>) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let mut sent = 0;
    loop {
        let (records, consumed) = queue.peek(BATCH);
        if consumed == 0 {
            return Ok(sent);
        }
        let count = records.len();
        if count > 0 {
            send(records).await?;
        }
        queue.drop_front(consumed)?;
        sent += count;
    }
}

/// [`flush`] to `POST /v1/devices/usage`.
pub async fn flush_to(api: &Api, device_id: &str, queue: &Queue) -> Result<usize> {
    flush(queue, |batch| async move {
        let reply = devices::upload_usage(api, device_id, &batch).await?;
        tracing::debug!(
            accepted = reply.accepted,
            duplicates = reply.duplicates,
            "usage batch uploaded"
        );
        Ok(())
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;

    fn record(i: usize) -> UsageRecord {
        UsageRecord {
            cache_read: 0,
            cache_write: 0,
            input_tokens: i as u64,
            message_id: format!("msg_{i}"),
            model: "claude-x".into(),
            occurred_at: "2026-10-28T00:00:00Z".into(),
            output_tokens: 1,
            session_ref: "s".into(),
        }
    }

    fn records(range: std::ops::Range<usize>) -> Vec<UsageRecord> {
        range.map(record).collect()
    }

    #[test]
    fn push_caps_by_dropping_oldest() {
        let dir = tempfile::tempdir().unwrap();
        let q = Queue::at(dir.path().join("q.jsonl"), 10);
        q.push(&records(0..8)).unwrap();
        q.push(&records(8..15)).unwrap();
        assert_eq!(q.len(), 10);
        let (head, _) = q.peek(1);
        assert_eq!(head[0].message_id, "msg_5");
    }

    #[tokio::test]
    async fn flush_sends_bounded_batches_and_empties() {
        let dir = tempfile::tempdir().unwrap();
        let q = Queue::at(dir.path().join("q.jsonl"), 5_000);
        q.push(&records(0..1_234)).unwrap();
        let mut sizes = Vec::new();
        let sent = flush(&q, |b| {
            sizes.push(b.len());
            async { Ok(()) }
        })
        .await
        .unwrap();
        assert_eq!(sent, 1_234);
        assert_eq!(sizes, vec![500, 500, 234]);
        assert_eq!(q.len(), 0);
    }

    #[tokio::test]
    async fn failed_batch_stays_queued() {
        let dir = tempfile::tempdir().unwrap();
        let q = Queue::at(dir.path().join("q.jsonl"), 5_000);
        q.push(&records(0..700)).unwrap();
        let mut calls = 0;
        let res = flush(&q, |_| {
            calls += 1;
            let fail = calls == 2;
            async move {
                if fail {
                    Err(Error::msg("offline"))
                } else {
                    Ok(())
                }
            }
        })
        .await;
        assert!(res.is_err());
        assert_eq!(q.len(), 200);
        let (head, _) = q.peek(1);
        assert_eq!(head[0].message_id, "msg_500");
    }

    #[tokio::test]
    async fn corrupt_lines_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("q.jsonl");
        let q = Queue::at(path.clone(), 100);
        q.push(&records(0..2)).unwrap();
        let mut raw = std::fs::read_to_string(&path).unwrap();
        raw.push_str("{not json\n");
        std::fs::write(&path, raw).unwrap();
        q.push(&records(2..3)).unwrap();
        let mut got = Vec::new();
        flush(&q, |b| {
            got.extend(b);
            async { Ok(()) }
        })
        .await
        .unwrap();
        assert_eq!(got, records(0..3));
        assert_eq!(q.len(), 0);
    }
}
