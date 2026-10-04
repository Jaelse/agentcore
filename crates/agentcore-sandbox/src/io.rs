use tokio::io::{AsyncRead, AsyncReadExt};

/// Read up to `max` bytes, then keep draining (so the writer never blocks on a
/// full pipe) while discarding the rest.
pub(crate) async fn read_limited<R: AsyncRead + Unpin>(
    mut reader: R,
    max: usize,
) -> std::io::Result<(Vec<u8>, bool)> {
    let mut kept = Vec::new();
    let mut truncated = false;
    let mut buf = [0u8; 8192];
    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            return Ok((kept, truncated));
        }
        let room = max.saturating_sub(kept.len());
        if n > room {
            truncated = true;
        }
        kept.extend_from_slice(&buf[..n.min(room)]);
    }
}

/// Run a child to completion collecting bounded stdout/stderr.
/// `on_timeout` runs before the direct child is killed, so backends can take
/// down the whole process tree.
pub(crate) async fn collect(
    mut child: tokio::process::Child,
    timeout: std::time::Duration,
    max: usize,
    on_timeout: impl FnOnce(),
) -> std::io::Result<crate::ExecOutput> {
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out = tokio::spawn(async move {
        match stdout {
            Some(s) => read_limited(s, max).await,
            None => Ok((Vec::new(), false)),
        }
    });
    let err = tokio::spawn(async move {
        match stderr {
            Some(s) => read_limited(s, max).await,
            None => Ok((Vec::new(), false)),
        }
    });
    let (status, timed_out) = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(status) => (Some(status?), false),
        Err(_) => {
            on_timeout();
            let _ = child.kill().await;
            (None, true)
        }
    };
    // Grandchildren may keep the pipes open after the child exits; don't let
    // them hold the tool call hostage.
    let grace = std::time::Duration::from_secs(2);
    let finish = |task: tokio::task::JoinHandle<std::io::Result<(Vec<u8>, bool)>>| async move {
        let abort = task.abort_handle();
        match tokio::time::timeout(grace, task).await {
            Ok(joined) => joined.map_err(std::io::Error::other).and_then(|r| r),
            Err(_) => {
                abort.abort();
                Ok((Vec::new(), true))
            }
        }
    };
    let (stdout, t1) = finish(out).await?;
    let (stderr, t2) = finish(err).await?;
    Ok(crate::ExecOutput {
        exit_code: status.and_then(|s| s.code()),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        truncated: t1 || t2,
        timed_out,
    })
}
