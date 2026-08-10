use std::{
    fs,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};

use crossbeam::channel::Sender;
use tracing::{error, info};

use crate::{
    exr::{ExrImage, load_exr},
    orchestrator::RollingCache,
};

pub fn load_playback(
    path: impl Into<PathBuf>,
    tx: Sender<(ExrImage, usize, usize)>,
    cache: Arc<RollingCache>,
    playhead: Arc<AtomicU64>,
    lookahead: u64,
    reverse: Arc<AtomicBool>,
) -> anyhow::Result<Arc<Vec<String>>> {
    let mut frame_paths = fs::read_dir(path.into())?
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            if path.extension()?.to_str()? == "exr" {
                Some(path.to_str()?.to_string())
            } else {
                None
            }
        })
        .collect::<Vec<String>>();
    frame_paths.sort();
    let frame_count = frame_paths.len();
    let frame_paths = Arc::new(frame_paths);

    let num_workers = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(12);
    info!(?num_workers, "Parallelism");

    for i in 0..num_workers {
        let tx = tx.clone();
        let cache = cache.clone();
        let playhead = playhead.clone();
        let reverse = reverse.clone();
        let frame_paths = frame_paths.clone();
        thread::Builder::new()
            .name(format!("Playback loader {i}"))
            .spawn(move || -> anyhow::Result<()> {
                loop {
                    let ph = playhead.load(Ordering::Relaxed);
                    let target = loop {
                        let candidate = if reverse.load(Ordering::Relaxed) {
                            let window_end = ph.saturating_sub(lookahead);
                            (window_end..=ph).rev().find(|f| !cache.contains(*f))
                        } else {
                            let window_end =
                                (ph + lookahead).min(frame_count.saturating_sub(1) as u64);
                            (ph..=window_end).find(|f| !cache.contains(*f))
                        };
                        match candidate {
                            Some(idx) if cache.try_reserve(idx) => break Some(idx),
                            Some(_) => continue,
                            None => break None,
                        }
                    };

                    let Some(idx) = target else {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    };

                    let start = std::time::Instant::now();
                    match load_exr(&frame_paths[idx as usize]) {
                        Ok(exr) => {
                            if let Err(e) = tx.send((exr, idx as usize, frame_count)) {
                                error!("send error for frame {idx}: {e}");
                            }
                        }
                        Err(e) => {
                            error!("DECODE FAILED for frame {idx}: {e:?}");
                            cache.remove_pending(idx);
                        }
                    }
                    info!(idx, ms = start.elapsed().as_millis(), "decoded");
                }
            })?;
    }
    Ok(frame_paths)
}

pub fn spawn_thumb_backfill(
    frame_paths: Arc<Vec<String>>,
    thumb_tx: Sender<(u64, Arc<ExrImage>)>,
    cache: Arc<RollingCache>,
) {
    let frame_count = frame_paths.len();
    let num_workers = 2;

    for i in 0..num_workers {
        let frame_paths = frame_paths.clone();
        let thumb_tx = thumb_tx.clone();
        let cache = cache.clone();

        thread::Builder::new()
            .name(format!("Thumb backfill {i}"))
            .spawn(move || {
                for idx in (i..frame_count).step_by(num_workers) {
                    if cache.contains(idx as u64) {
                        continue;
                    }

                    match load_exr(&frame_paths[idx]) {
                        Ok(exr) => {
                            let _ = thumb_tx.send((idx as u64, Arc::new(exr)));
                        }
                        Err(e) => {
                            error!("backfill decode failed for frame {idx}: {e:?}");
                        }
                    }

                    thread::sleep(Duration::from_millis(1));
                }
            })
            .ok();
    }
}
