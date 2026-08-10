use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crossbeam::{
    channel::{Receiver, Sender, bounded, tick, unbounded},
    select,
};
use parking_lot::RwLock;
use tracing::info;

use crate::{exr::ExrImage, playback::{load_playback, spawn_thumb_backfill}, thumbnail::thumbworker};

pub enum Message {
    DirChosen(PathBuf),
    FrameChange(u64),
    TotalFrames(u64),
    GoBack(u64),
    GoForward(u64),
    FrameRate(f64),
    Multiplier(f64),
    FirstFrame,
    LastFrame,
    Pause,
    Reverse,
    Forward,
}

#[derive(Debug, Default, Clone)]
pub struct State {
    pub paused: bool,
    pub current_frame: u64,
    pub total_frames: u64,
    pub frame_rate: f64,
    pub open_dir: Option<PathBuf>,
    pub reverse: bool,
    pub multiplier: f64,
}

impl State {
    pub fn new() -> Self {
        Self {
            paused: false,
            current_frame: 0,
            total_frames: 0,
            frame_rate: 23.976,
            open_dir: None,
            reverse: false,
            multiplier: 1.0,
        }
    }
}

pub struct RollingCache {
    frames: RwLock<HashMap<u64, Arc<ExrImage>>>,
    pending: RwLock<HashSet<u64>>,
    capacity: usize,
    lookahead: u64,
    behind: u64,
    thumb_tx: Sender<(u64, Arc<ExrImage>)>,
}

impl RollingCache {
    pub fn new(thumb_tx: Sender<(u64, Arc<ExrImage>)>) -> Self {
        Self {
            frames: RwLock::new(HashMap::new()),
            pending: RwLock::new(HashSet::new()),
            capacity: 0,
            lookahead: 0,
            behind: 0,
            thumb_tx,
        }
    }
}

impl RollingCache {
    pub fn get(&self, frame: u64) -> Option<Arc<ExrImage>> {
        let frames = self.frames.read();

        frames.get(&frame).cloned()
    }
    pub fn insert(&self, frame: u64, img: ExrImage, playhead: u64) {
        info!(?frame, ?playhead, "Inserting");
        let mut frames = self.frames.write();

        let img = Arc::new(img);
        frames.insert(frame, img.clone());

        if frames.len() > self.capacity {
            frames.retain(|&k, _| k + self.behind >= playhead && k <= playhead + self.lookahead);
        }

        let _  = self.thumb_tx.send((frame, img));

        drop(frames);

        self.pending.write().remove(&frame);
    }

    pub fn contains(&self, frame: u64) -> bool {
        self.frames.read().contains_key(&frame) || self.pending.read().contains(&frame)
    }

    pub fn try_reserve(&self, frame: u64) -> bool {
        {
            let frames = self.frames.read();
            if frames.contains_key(&frame) {
                return false;
            }
        }
        let mut pending = self.pending.write();
        if pending.contains(&frame) {
            false
        } else {
            pending.insert(frame);
            true
        }
    }

    pub fn sweep_pending(&self, playhead: u64) {
        let mut pending = self.pending.write();
        pending.retain(|&f| f + self.behind >= playhead && f <= playhead + self.lookahead);
    }

    pub fn remove_pending(&self, frame: u64) {
        self.pending.write().remove(&frame);
    }
}

pub fn orchestrate(
    msg_rx: Receiver<Message>,
    frame_tx: Sender<Arc<ExrImage>>,
    info_tx: Sender<State>,
    upload_tx: Sender<(u64, ExrImage)>,
) -> JoinHandle<anyhow::Result<()>> {
    thread::spawn(move || -> anyhow::Result<()> {
        info!("Running orchestrator");
        let mut playback_info = State::new();
        let (frame_data_tx, frame_rx) = bounded(16);
        let (thumb_tx, thumb_rx) = unbounded();
        thread::spawn(move || {
            thumbworker(thumb_rx, upload_tx);
        });
        let cache_pool = Arc::new(RollingCache {
            capacity: 80,
            behind: 40,
            lookahead: 40,
            ..RollingCache::new(thumb_tx.clone())
        });
        let poll_tick = tick(Duration::from_millis(2));
        let mut last_advance = Instant::now();

        let (inner_msg_tx, inner_msg_rx) = unbounded();

        let playhead = Arc::new(AtomicU64::new(0));
        let reverse = Arc::new(AtomicBool::new(false));
        {
            let playhead = playhead.clone();
            let cache_pool = cache_pool.clone();
            let frame_rx = frame_rx.clone();
            thread::spawn(move || -> anyhow::Result<()> {
                let mut total_frames = 0;

                loop {
                    let frame_data: (ExrImage, usize, usize) = frame_rx.recv()?;
                    if total_frames != frame_data.2 {
                        let _ = inner_msg_tx.send(Message::TotalFrames(frame_data.2 as u64));
                        total_frames = frame_data.2;
                    }
                    let ph = playhead.load(Ordering::Relaxed);
                    info!(num = frame_data.1, ?ph, "Received");
                    cache_pool.insert(frame_data.1 as u64, frame_data.0, ph);
                }
            });
        }

        let mut force_display = false;

        loop {
            select! {
                recv(msg_rx)-> msg => {
                    let msg = msg?;
                    let frame_tx = frame_data_tx.clone();
                    match msg {
                        Message::DirChosen(path) => {
                            let frame_paths = load_playback(
                                &path,
                                frame_tx,
                                cache_pool.clone(),
                                playhead.clone(),
                                cache_pool.lookahead,
                                reverse.clone()
                            )?;


                            spawn_thumb_backfill(frame_paths, thumb_tx.clone(), cache_pool.clone());
                            playback_info.open_dir = Some(path);
                        },
                        Message::FrameChange(frame_number) => {
                            playhead.swap(frame_number, Ordering::Relaxed);
                            playback_info.current_frame = frame_number;
                            last_advance = Instant::now();
                            force_display = true;
                            playback_info.paused = true;
                            cache_pool.sweep_pending(frame_number);
                        }

                        Message::TotalFrames(frames) => playback_info.total_frames = frames,
                        Message::GoBack(frame_count) => {
                            let cf = playhead.fetch_sub(frame_count, Ordering::Relaxed);
                            playback_info.current_frame = cf - frame_count;
                            last_advance = Instant::now();
                            force_display = true;
                            playback_info.paused = true;
                            cache_pool.sweep_pending(cf - frame_count);
                        }
                        Message::GoForward(frame_count) => {
                            let cf = playhead.fetch_add(frame_count, Ordering::Relaxed);
                            playback_info.current_frame = cf + frame_count;
                            last_advance = Instant::now();
                            force_display = true;
                            playback_info.paused = true;
                            cache_pool.sweep_pending(cf + frame_count);
                        }
                        Message::Pause => {
                            playback_info.multiplier = 1.0;
                            playback_info.paused = !playback_info.paused;
                        }
                        Message::Reverse => {
                            reverse.swap(true, Ordering::Relaxed);
                            if playback_info.reverse {
                                playback_info.multiplier *= 2.0;
                            } else {
                                playback_info.multiplier = 1.0;
                            }
                            playback_info.reverse = true;
                            playback_info.paused = false;
                        }
                        Message::Forward => {
                            reverse.swap(false, Ordering::Relaxed);
                            if !playback_info.reverse {
                                playback_info.multiplier *= 2.0;
                            } else {
                                playback_info.multiplier = 1.0;
                            }
                            playback_info.reverse = false;
                            playback_info.paused = false;
                        }
                        Message::FirstFrame => {
                            playhead.swap(0, Ordering::Relaxed);
                            playback_info.current_frame = 0;
                            last_advance = Instant::now();
                            force_display = true;
                            playback_info.paused = true;
                            cache_pool.sweep_pending(0);
                        }
                        Message::LastFrame => {
                            playhead.swap(playback_info.total_frames-1, Ordering::Relaxed);
                            playback_info.current_frame = playback_info.total_frames-1;
                            last_advance = Instant::now();
                            force_display = true;
                            playback_info.paused = true;
                            cache_pool.sweep_pending(playback_info.total_frames-1);
                        }
                        Message::FrameRate(fr) => {
                            playback_info.frame_rate = fr
                        }
                        Message::Multiplier(mp) => {
                            playback_info.multiplier = mp
                        }
                    };
                    let _ = info_tx.send(playback_info.clone());
                }
                recv(inner_msg_rx) -> msg => {
                    let msg = msg?;
                    match msg {
                        Message::TotalFrames(frames) => playback_info.total_frames = frames,
                        _ => {}
                    }
                }
                recv(poll_tick) -> _ => {

                    if playback_info.current_frame >= playback_info.total_frames  {
                        continue
                    }

                    let now = Instant::now();
                    let frame_interval = Duration::from_secs_f64(1.0 / (playback_info.frame_rate * playback_info.multiplier));
                    if !force_display && now.duration_since(last_advance) < frame_interval {
                        continue;
                    }

                    if playback_info.paused && !force_display {
                        continue
                    }

                    let ph = playhead.load(Ordering::Relaxed);

                    match cache_pool.get(ph) {
                        Some(frame) => {
                            let _ = frame_tx.send(frame);
                            if !force_display {
                                if playback_info.reverse{
                                    if ph > 0 {
                                        info!(%ph, "Playhead");
                                        playback_info.current_frame -= 1;
                                        playhead.swap(ph-1, Ordering::Relaxed);
                                    }
                                } else {
                                    playback_info.current_frame += 1;
                                    playhead.swap(ph+1, Ordering::Relaxed);
                                }
                            }
                            force_display = false;
                            info!(?playhead, frames = cache_pool.frames.read().len(), "Playback info");
                            let _ = info_tx.send(playback_info.clone());

                            last_advance += frame_interval;
                        }
                        None => {
                            last_advance = now;
                        }
                    }
                }

            }
        }
    })
}
