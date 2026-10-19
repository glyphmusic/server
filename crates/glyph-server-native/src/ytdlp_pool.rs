//! Three independent extractors: different songs run in parallel, while each
//! process reuses HTTP sessions and extractor state. GLYPH_RDLP_PATH opts in
//! to native r-dlp; the command runner retains its yt-dlp fallback.
use glyph_server_core::CommandError;
use std::sync::{atomic::{AtomicUsize, Ordering}, OnceLock};
use tokio::{io::{AsyncBufReadExt, AsyncWriteExt, BufReader}, process::{Child, ChildStdin, ChildStdout}, sync::{Mutex, MutexGuard}};

const WORKERS: usize = 3;
static NEXT: AtomicUsize = AtomicUsize::new(0);
type Slot = Mutex<Option<Worker>>;
fn pool() -> &'static [Slot; WORKERS] {
    static POOL: OnceLock<[Slot; WORKERS]> = OnceLock::new();
    POOL.get_or_init(|| std::array::from_fn(|_| Mutex::new(None)))
}

// Register with every slot instead of pinning a queued request to one busy
// process. Dropping the losing lock futures also removes their waiters.
async fn available_slot(slots: &[Slot], start: usize) -> MutexGuard<'_, Option<Worker>> {
    let waits = (0..slots.len())
        .map(|offset| Box::pin(slots[(start + offset) % slots.len()].lock()))
        .collect::<Vec<_>>();
    futures::future::select_all(waits).await.0
}

fn recycle(native: bool, requests: u32) -> bool {
    !native && requests >= 100
}

pub fn enabled() -> bool {
    cfg!(unix) && std::env::var("YTDLP_SERVICE_TOKEN").is_ok_and(|token| token.len() >= 32)
        && std::env::var("GLYPH_YTDLP_PERSISTENT").as_deref() != Ok("0")
}

struct Worker {
    _child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    program: String,
    requests: u32,
    native: bool,
}

impl Worker {
    async fn start(program: &str) -> Result<Self, CommandError> {
        let native_path = std::env::var("GLYPH_RDLP_PATH").ok().filter(|p| !p.trim().is_empty());
        let native = native_path.is_some();
        let mut command = if let Some(path) = native_path {
            let mut command = tokio::process::Command::new(path);
            command.args(["serve", "--warmup"]);
            command
        } else {
            let python = std::env::var("GLYPH_PYTHON_PATH").unwrap_or_else(|_| "python3".into());
            let mut command = tokio::process::Command::new(python);
            command.args(["-u", "-c", include_str!("ytdlp_worker.py"), program]);
            command
        };
        let mut child = command
            .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null()).kill_on_drop(true)
            .spawn().map_err(|_| CommandError::Unavailable)?;
        let stdin = child.stdin.take().ok_or(CommandError::Failed)?;
        let stdout = BufReader::new(child.stdout.take().ok_or(CommandError::Failed)?);
        let mut worker = Self { _child: child, stdin, stdout, program: program.into(), requests: 0, native };
        let ready = worker.line().await?;
        if ready.get("ready").and_then(|v| v.as_bool()) != Some(true) { return Err(CommandError::Failed); }
        Ok(worker)
    }

    async fn line(&mut self) -> Result<serde_json::Value, CommandError> {
        let mut bytes = Vec::new();
        loop {
            let buffer = self.stdout.fill_buf().await.map_err(|_| CommandError::Failed)?;
            if buffer.is_empty() { return Err(CommandError::Failed); }
            let count = buffer.iter().position(|b| *b == b'\n').map(|n| n+1).unwrap_or(buffer.len());
            if bytes.len() + count > 65536 { return Err(CommandError::Failed); }
            bytes.extend_from_slice(&buffer[..count]);
            let complete = buffer[count-1] == b'\n';
            self.stdout.consume(count);
            if complete { break; }
        }
        serde_json::from_slice(&bytes).map_err(|_| CommandError::Failed)
    }

    async fn resolve(&mut self, id: &str) -> Result<Vec<u8>, CommandError> {
        let request = format!("{}\n", serde_json::json!({"id": id}));
        self.stdin.write_all(request.as_bytes()).await.map_err(|_| CommandError::Failed)?;
        self.stdin.flush().await.map_err(|_| CommandError::Failed)?;
        let response = self.line().await?;
        if response.get("id").and_then(|v| v.as_str()) != Some(id) { return Err(CommandError::Failed); }
        let url = response.get("url").and_then(|v| v.as_str()).ok_or(CommandError::Failed)?;
        let bytes = format!("{url}\n").into_bytes();
        if !super::platform::usable_audio_url(&bytes) { return Err(CommandError::Failed); }
        self.requests = self.requests.saturating_add(1);
        Ok(bytes)
    }
}

pub async fn resolve(program: &str, id: &str) -> Result<Vec<u8>, CommandError> {
    let start = NEXT.fetch_add(1, Ordering::Relaxed) % WORKERS;
    let mut slot = available_slot(pool(), start).await;
    // Move ownership out before awaiting: cancellation or timeout must kill
    // the process, otherwise a later song could consume an old response.
    let mut worker = match slot.take() {
        Some(worker) if worker.program == program && !recycle(worker.native, worker.requests) => worker,
        _ => Worker::start(program).await?,
    };
    let result = worker.resolve(id).await;
    if result.is_ok() { *slot = Some(worker); }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_python_workers_recycle_at_request_limit() {
        assert!(!recycle(false, 99));
        assert!(recycle(false, 100));
        assert!(!recycle(true, 100));
        assert!(!recycle(true, u32::MAX));
    }

    #[tokio::test]
    async fn queued_request_uses_whichever_worker_finishes_first() {
        let slots: [Slot; 2] = std::array::from_fn(|_| Mutex::new(None));
        let first = slots[0].lock().await;
        let second = slots[1].lock().await;
        let request = available_slot(&slots, 0);
        tokio::pin!(request);
        assert!(futures::poll!(&mut request).is_pending());
        drop(second);
        let selected = tokio::time::timeout(std::time::Duration::from_secs(1), request).await.unwrap();
        assert!(slots[1].try_lock().is_err());
        drop(selected);
        drop(first);
        assert!(slots.iter().all(|slot| slot.try_lock().is_ok()));
    }

    #[tokio::test]
    async fn cancelling_waiter_does_not_reserve_a_worker() {
        let slots: [Slot; 2] = std::array::from_fn(|_| Mutex::new(None));
        let first = slots[0].lock().await;
        let second = slots[1].lock().await;
        {
            let request = available_slot(&slots, 0);
            tokio::pin!(request);
            assert!(futures::poll!(&mut request).is_pending());
        }
        drop(first);
        drop(second);
        assert!(slots.iter().all(|slot| slot.try_lock().is_ok()));
    }
}

pub fn warm(program: String) {
    if !enabled() { return; }
    for slot in pool() {
        let program = program.clone();
        tokio::spawn(async move {
            let Ok(mut slot) = slot.try_lock() else { return };
            if slot.is_none() {
                if let Ok(Ok(worker)) = tokio::time::timeout(std::time::Duration::from_secs(10), Worker::start(&program)).await {
                    *slot = Some(worker);
                }
            }
        });
    }
}

/// Management-only reload after a verified resolver update. Existing requests
/// finish before their worker is dropped; scheduling/warmup stays unchanged.
pub async fn reload() {
    for slot in pool() { slot.lock().await.take(); }
}
