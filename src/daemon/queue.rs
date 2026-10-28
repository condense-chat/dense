//! The offline usage queue: an append-only JSONL file under the daemon dir,
//! capped at [`MAX_RECORDS`] (oldest dropped first), drained to
//! `POST /v1/devices/usage` in batches of [`BATCH`]. The server dedupes on
//! `(device_id, message_id)`, so a batch resent after a crash is harmless; a
//! batch it rejects outright (non-retryable 4xx) is dropped, not retried.

use std::future::Future;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::api::Api;
use crate::api::devices::{self, DEVICE_OWNED_ELSEWHERE, DEVICE_UNKNOWN, Rejection, Reply};
use crate::config::Config;
use crate::error::Context;

pub const BATCH: usize = 500;
pub const MAX_RECORDS: usize = 20_000;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Flushed {
    /// Records in batches the server rejected for good.
    pub dropped: usize,
    pub sent: usize,
    /// The drain stopped on a refusal of the device itself (unknown, or bound
    /// to another user); the batch stays queued for after a heartbeat.
    pub stalled: Option<Rejection>,
}

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
            .read(true)
            .append(true)
            .open(&self.path)
            .ctx(format!("opening {}", self.path.display()))?;
        if !ends_with_newline(&mut file) {
            // A torn tail from an interrupted write: close it off so it is
            // dropped as one corrupt line instead of swallowing our first record.
            buf.insert(0, '\n');
        }
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

/// Drain the queue through `send`, one [`BATCH`] at a time. A batch leaves
/// the queue once `send` returns `Ok` — accepted, or rejected for good — and
/// stays (stopping the drain) on `Err` or a device-level refusal.
pub async fn flush<F, Fut>(queue: &Queue, mut send: F) -> Result<Flushed>
where
    F: FnMut(Vec<UsageRecord>) -> Fut,
    Fut: Future<Output = Result<Reply<()>>>,
{
    let mut out = Flushed::default();
    loop {
        let (records, consumed) = queue.peek(BATCH);
        if consumed == 0 {
            return Ok(out);
        }
        let count = records.len();
        let reply = if count > 0 {
            send(records).await?
        } else {
            Reply::Ok(())
        };
        if let Reply::Rejected(r) = &reply
            && (r.is(DEVICE_UNKNOWN) || r.is(DEVICE_OWNED_ELSEWHERE))
        {
            out.stalled = Some(r.clone());
            return Ok(out);
        }
        queue.drop_front(consumed)?;
        match reply {
            Reply::Ok(()) => out.sent += count,
            Reply::Rejected(r) => {
                tracing::warn!("server rejected {count} usage records ({r}); dropped them");
                out.dropped += count;
            }
        }
    }
}

/// [`flush`] to `POST /v1/devices/usage`.
pub async fn flush_to(api: &Api, device_id: &str, queue: &Queue) -> Result<Flushed> {
    flush(queue, |batch| async move {
        Ok(match devices::upload_usage(api, device_id, &batch).await? {
            Reply::Ok(reply) => {
                tracing::debug!(
                    accepted = reply.accepted,
                    duplicates = reply.duplicates,
                    "usage batch uploaded"
                );
                Reply::Ok(())
            }
            Reply::Rejected(r) => Reply::Rejected(r),
        })
    })
    .await
}

fn ends_with_newline(file: &mut std::fs::File) -> bool {
    let mut last = [0u8; 1];
    match file.seek(SeekFrom::End(-1)) {
        Ok(_) => file.read_exact(&mut last).is_err() || last[0] == b'\n',
        Err(_) => true,
    }
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
            async { Ok(Reply::Ok(())) }
        })
        .await
        .unwrap();
        assert_eq!(sent.sent, 1_234);
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
                    Ok(Reply::Ok(()))
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
            async { Ok(Reply::Ok(())) }
        })
        .await
        .unwrap();
        assert_eq!(got, records(0..3));
        assert_eq!(q.len(), 0);
    }

    #[tokio::test]
    async fn torn_tail_does_not_eat_the_next_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("q.jsonl");
        let q = Queue::at(path.clone(), 100);
        q.push(&records(0..1)).unwrap();
        let mut raw = std::fs::read_to_string(&path).unwrap();
        raw.push_str("{\"cache_read\":0,\"mess");
        std::fs::write(&path, raw).unwrap();
        q.push(&records(1..3)).unwrap();
        let mut got = Vec::new();
        flush(&q, |b| {
            got.extend(b);
            async { Ok(Reply::Ok(())) }
        })
        .await
        .unwrap();
        assert_eq!(got, records(0..3));
    }

    #[tokio::test]
    async fn rejected_batch_is_dropped_and_the_rest_drains() {
        let dir = tempfile::tempdir().unwrap();
        let q = Queue::at(dir.path().join("q.jsonl"), 5_000);
        q.push(&records(0..700)).unwrap();
        let mut calls = 0;
        let out = flush(&q, |_| {
            calls += 1;
            let first = calls == 1;
            async move {
                Ok(if first {
                    Reply::Rejected(Rejection::new(422, None))
                } else {
                    Reply::Ok(())
                })
            }
        })
        .await
        .unwrap();
        assert_eq!(
            out,
            Flushed {
                dropped: 500,
                sent: 200,
                stalled: None
            }
        );
        assert_eq!(q.len(), 0);
    }

    #[tokio::test]
    async fn device_refusal_keeps_the_batch_queued() {
        for code in [DEVICE_UNKNOWN, DEVICE_OWNED_ELSEWHERE] {
            let dir = tempfile::tempdir().unwrap();
            let q = Queue::at(dir.path().join("q.jsonl"), 5_000);
            q.push(&records(0..700)).unwrap();
            let mut calls = 0;
            let out = flush(&q, |_| {
                calls += 1;
                async move { Ok(Reply::Rejected(Rejection::new(404, Some(code)))) }
            })
            .await
            .unwrap();
            assert_eq!(calls, 1);
            assert_eq!(out.sent + out.dropped, 0);
            assert!(out.stalled.unwrap().is(code));
            assert_eq!(q.len(), 700);
        }
    }
}
