use std::future::Future;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::api::Api;
use crate::api::devices::{self, Rejection, Reply};
use crate::config::Config;
use crate::error::Context;

pub const BATCH: usize = 500;
/// Per flush, so a backlog on a slow link never holds the other daemon jobs up.
pub const MAX_BATCHES: usize = 4;
pub const MAX_RECORDS: usize = 20_000;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Flushed {
    /// Records in batches the server could not take as sent (400, 413, 422).
    pub dropped: usize,
    /// Stopped at [`MAX_BATCHES`] with records still queued.
    pub more: bool,
    pub sent: usize,
    /// The drain stopped on any other refusal (the device, the login, a
    /// proxy); the batch stays queued for a later tick.
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

    pub fn clear(&self) -> Result<()> {
        match std::fs::remove_file(&self.path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(e).ctx(format!("removing {}", self.path.display()))
            }
            _ => Ok(()),
        }
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

    fn rewrite(&self, lines: &[String]) -> Result<()> {
        let mut body = lines.join("\n");
        if !body.is_empty() {
            body.push('\n');
        }
        crate::atomic::replace(&self.path, body.as_bytes())
            .ctx(format!("replacing {}", self.path.display()))
    }
}

/// Drain the queue through `send`, one [`BATCH`] at a time, reading the file
/// once and rewriting it once. The server dedupes on `(device_id,
/// message_id)`, so a batch resent after a crash is harmless. A batch leaves
/// the queue once accepted or refused as malformed; any other refusal or an
/// `Err` stops the drain with it still queued.
pub async fn flush<F, Fut>(queue: &Queue, mut send: F) -> Result<Flushed>
where
    F: FnMut(Vec<UsageRecord>) -> Fut,
    Fut: Future<Output = Result<Reply<()>>>,
{
    let lines = queue.lines();
    let mut out = Flushed::default();
    let mut done = 0;
    let mut failed = None;
    for chunk in lines.chunks(BATCH).take(MAX_BATCHES) {
        // A corrupt line is skipped, then dropped with its batch.
        let records: Vec<UsageRecord> = chunk
            .iter()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        let count = records.len();
        let reply = if count > 0 {
            match send(records).await {
                Ok(reply) => reply,
                Err(e) => {
                    failed = Some(e);
                    break;
                }
            }
        } else {
            Reply::Ok(())
        };
        match reply {
            Reply::Ok(()) => out.sent += count,
            Reply::Rejected(r) if malformed(&r) => {
                tracing::warn!("server rejected {count} usage records ({r}); dropped them");
                out.dropped += count;
            }
            Reply::Rejected(r) => {
                out.stalled = Some(r);
                break;
            }
        }
        done += chunk.len();
    }
    if done > 0 {
        queue.rewrite(&lines[done..])?;
    }
    if let Some(e) = failed {
        return Err(e);
    }
    out.more = out.stalled.is_none() && lines.len() > done;
    Ok(out)
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

/// A batch the server can't take as sent; resending it unchanged never helps.
fn malformed(r: &Rejection) -> bool {
    matches!(r.status, 400 | 413 | 422)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::devices::{DEVICE_OWNED_ELSEWHERE, DEVICE_UNKNOWN};
    use crate::error::Error;

    fn head(q: &Queue) -> UsageRecord {
        serde_json::from_str(&q.lines()[0]).unwrap()
    }

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
        assert_eq!(head(&q).message_id, "msg_5");
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
        assert!(!sent.more);
    }

    #[tokio::test]
    async fn one_flush_stops_after_max_batches() {
        let dir = tempfile::tempdir().unwrap();
        let q = Queue::at(dir.path().join("q.jsonl"), 5_000);
        q.push(&records(0..2_100)).unwrap();
        let send = |_| async { Ok(Reply::Ok(())) };
        let first = flush(&q, send).await.unwrap();
        assert_eq!((first.sent, first.more), (BATCH * MAX_BATCHES, true));
        let second = flush(&q, send).await.unwrap();
        assert_eq!((second.sent, second.more), (100, false));
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
        assert_eq!(head(&q).message_id, "msg_500");
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
                more: false,
                sent: 200,
                stalled: None
            }
        );
        assert_eq!(q.len(), 0);
    }

    #[tokio::test]
    async fn a_refused_login_or_a_proxy_404_keeps_the_batch_queued() {
        for status in [401, 403, 404] {
            let dir = tempfile::tempdir().unwrap();
            let q = Queue::at(dir.path().join("q.jsonl"), 5_000);
            q.push(&records(0..700)).unwrap();
            let out = flush(&q, |_| async move {
                Ok(Reply::Rejected(Rejection::new(status, None)))
            })
            .await
            .unwrap();
            assert_eq!((out.sent, out.dropped), (0, 0), "{status}");
            assert!(out.stalled.is_some() && !out.more);
            assert_eq!(q.len(), 700);
        }
    }

    #[tokio::test]
    async fn one_flush_reads_and_rewrites_the_file_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("q.jsonl");
        let q = Queue::at(path.clone(), 5_000);
        q.push(&records(0..1_234)).unwrap();
        let mut seen = Vec::new();
        flush(&q, |_| {
            // Unchanged until the drain ends: nothing rewrote it between batches.
            seen.push(std::fs::read_to_string(&path).unwrap().lines().count());
            async move { Ok(Reply::Ok(())) }
        })
        .await
        .unwrap();
        assert_eq!(seen, vec![1_234, 1_234, 1_234]);
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
