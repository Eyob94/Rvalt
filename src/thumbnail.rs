use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    thread,
};

use crossbeam::channel::{Receiver, Sender};
use parking_lot::RwLock;

use crate::exr::ExrImage;

pub fn thumbworker(thumb_rx: Receiver<(u64, Arc<ExrImage>)>, upload_tx: Sender<(u64, ExrImage)>) {
    for _ in 0..4 {
        let thumb_rx = thumb_rx.clone();
        let upload_tx = upload_tx.clone();
        thread::spawn(move || {
            while let Ok((frame, img)) = thumb_rx.recv() {
                let (r, g, b, a) = generate_thumb_planar(&img, 96, 48);
                let small = ExrImage {
                    width: 96,
                    height: 48,
                    r,
                    g,
                    b,
                    a,
                };
                let _ = upload_tx.send((frame, small));
            }
        });
    }
}

#[derive(Default, Clone)]
pub struct ThumbHandle {
    pub texture: egui::TextureId,
}

#[derive(Default)]
pub struct ThumbCache {
    thumbs: RwLock<HashMap<u64, ThumbHandle>>,
    pending: RwLock<HashSet<u64>>,
}

impl ThumbCache {
    pub fn get(&self, frame: u64) -> Option<ThumbHandle> {
        self.thumbs.read().get(&frame).cloned()
    }
    pub fn insert(&self, frame: u64, handle: ThumbHandle) {
        self.thumbs.write().insert(frame, handle);
        self.pending.write().remove(&frame);
    }
}

pub fn downsample_half_planar(
    src: &[half::f16],
    width: usize,
    height: usize,
) -> (Vec<half::f16>, usize, usize) {
    let nw = (width / 2).max(1);
    let nh = (height / 2).max(1);

    let mut dst = vec![half::f16::ZERO; nw * nh];

    for y in 0..nh {
        let sy0 = y * 2;
        let sy1 = (sy0 + 1).min(height - 1);
        for x in 0..nw {
            let sx0 = x * 2;
            let sx1 = (sx0 + 1).min(width - 1);

            let v = src[sy0 * width + sx0].to_f32()
                + src[sy0 * width + sx1].to_f32()
                + src[sy1 * width + sx0].to_f32()
                + src[sy1 * width + sx1].to_f32();

            dst[y * nw + x] = half::f16::from_f32(v * 0.25);
        }
    }

    (dst, nw, nh)
}

fn downsample_box_planar(
    src: &[half::f16],
    src_w: usize,
    src_h: usize,
    dst_w: usize,
    dst_h: usize,
) -> Vec<half::f16> {
    let mut dst = vec![half::f16::ZERO; dst_w * dst_h];
    let x_ratio = src_w as f32 / dst_w as f32;
    let y_ratio = src_h as f32 / dst_h as f32;

    for dy in 0..dst_h {
        let sy0 = (dy as f32 * y_ratio) as usize;
        let sy1 = (((dy + 1) as f32 * y_ratio) as usize)
            .max(sy0 + 1)
            .min(src_h);
        for dx in 0..dst_w {
            let sx0 = (dx as f32 * x_ratio) as usize;
            let sx1 = (((dx + 1) as f32 * x_ratio) as usize)
                .max(sx0 + 1)
                .min(src_w);

            let mut acc = 0f32;
            let mut count = 0usize;
            for sy in sy0..sy1 {
                for sx in sx0..sx1 {
                    acc += src[sy * src_w + sx].to_f32();
                    count += 1;
                }
            }
            dst[dy * dst_w + dx] = half::f16::from_f32(acc / count as f32);
        }
    }
    dst
}

fn generate_thumb_planar(
    img: &ExrImage,
    target_w: usize,
    target_h: usize,
) -> (
    Vec<half::f16>,
    Vec<half::f16>,
    Vec<half::f16>,
    Vec<half::f16>,
) {
    let downsample_channel = |src: &[half::f16], mut w: usize, mut h: usize| -> Vec<half::f16> {
        let (mut buf, nw, nh) = downsample_half_planar(src, w, h);
        w = nw;
        h = nh;

        while w / 2 >= target_w && h / 2 >= target_h {
            let (nbuf, next_w, next_h) = downsample_half_planar(&buf, w, h);
            buf = nbuf;
            w = next_w;
            h = next_h;
        }

        if w != target_w || h != target_h {
            downsample_box_planar(&buf, w, h, target_w, target_h)
        } else {
            buf
        }
    };

    let r = downsample_channel(&img.r, img.width, img.height);
    let g = downsample_channel(&img.g, img.width, img.height);
    let b = downsample_channel(&img.b, img.width, img.height);
    let a = downsample_channel(&img.a, img.width, img.height);

    (r, g, b, a)
}
