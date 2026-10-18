//! Three independent extractors: different songs run in parallel, while each
//! process reuses Python imports, HTTP sessions and YouTube extractor state.
use glyph_server_core::CommandError;
use std::sync::{atomic::{AtomicUsize, Ordering}, OnceLock};
use tokio::{io::{AsyncBufReadExt, AsyncWriteExt, BufReader}, process::{Child, ChildStdin, ChildStdout}, sync::Mutex};

const WORKERS: usize = 3;
static NEXT: AtomicUsize = AtomicUsize::new(0);
type Slot = Mutex<Option<Worker>>;
fn pool() -> &'static [Slot; WORKERS] {
    static POOL: OnceLock<[Slot; WORKERS]> = OnceLock::new();
    POOL.get_or_init(|| std::array::from_fn(|_| Mutex::new(None)))
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
}

impl Worker {
    async fn start(program: &str) -> Result<Self, CommandError> {
        let python = std::env::var("GLYPH_PYTHON_PATH").unwrap_or_else(|_| "python3".into());
        let mut child = tokio::process::Command::new(python)
            .args(["-u", "-c", include_str!("ytdlp_worker.py"), program])
            .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null()).kill_on_drop(true)
            .spawn().map_err(|_| CommandError::Unavailable)?;
        let stdin = child.stdin.take().ok_or(CommandError::Failed)?;
        let stdout = BufReader::new(child.stdout.take().ok_or(CommandError::Failed)?);
        let mut worker = Self { _child: child, stdin, stdout, program: program.into(), requests: 0 };
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
        self.requests += 1;
        Ok(bytes)
    }
}

pub async fn resolve(program: &str, id: &str) -> Result<Vec<u8>, CommandError> {
    let start = NEXT.fetch_add(1, Ordering::Relaxed) % WORKERS;
    let mut selected = None;
    for offset in 0..WORKERS {
        if let Ok(slot) = pool()[(start + offset) % WORKERS].try_lock() { selected = Some(slot); break; }
    }
    let mut slot = match selected { Some(slot) => slot, None => pool()[start].lock().await };
    // Move ownership out before awaiting: cancellation or timeout must kill
    // the process, otherwise a later song could consume an old response.
    let mut worker = match slot.take() {
        Some(worker) if worker.program == program && worker.requests < 100 => worker,
        _ => Worker::start(program).await?,
    };
    let result = worker.resolve(id).await;
    if result.is_ok() { *slot = Some(worker); }
    result
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
