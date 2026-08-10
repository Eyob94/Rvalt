use std::env;

use crossbeam::channel::{Receiver, Sender};
use egui::{RichText, Sense, Ui};
use strum::IntoEnumIterator;
use tracing::{error, info};

use crate::{
    exr::{ChannelMode, ExrGpuImage},
    ocio::ColorState,
    orchestrator::{Message, State},
    thumbnail::ThumbCache,
};

pub struct UiState {
    pub(crate) tx: Sender<Message>,
    pub(crate) rx: Receiver<State>,
    pub(crate) thumb_cache: ThumbCache,
    pub(crate) thumb_size: Option<(f32, f32)>,
    playback_state: State,
    scroll_offset: f32,
    channel_mode: ChannelMode,
}

impl UiState {
    pub fn new(tx: Sender<Message>, rx: Receiver<State>) -> Self {
        Self {
            tx,
            rx,
            scroll_offset: 0.0,
            playback_state: State::new(),
            channel_mode: ChannelMode::default(),
            thumb_cache: ThumbCache::default(),
            thumb_size: None,
        }
    }
}

#[derive(Default)]
pub struct Config {
    pub lut_bake: bool,
    pub channel_mode: Option<u32>,
}

pub fn ui(
    ctx: &mut Ui,
    color_state: &mut ColorState,
    exr_gpu_img: &mut Option<ExrGpuImage>,
    config: &mut Config,
    ui_state: &mut Option<UiState>,
) {
    let Some(ui_state) = ui_state else {
        error!("No user config found");
        return;
    };

    if let Ok(playback) = ui_state.rx.try_recv() {
        info!(?playback, "Received");
        ui_state.playback_state = playback
    };
    egui::MenuBar::new().ui(ctx, |ui| {
        ui.menu_button("File", |ui| {
            if ui.button("Open Dir").clicked() {
                let home_dir = env::home_dir();
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("all", &["*"])
                    .set_directory(home_dir.unwrap_or_default())
                    .pick_folder()
                    && let Err(e) = ui_state.tx.send(Message::DirChosen(path))
                {
                    error!(?e, "error sending chosen dir");
                }
            }
        });
    });

    egui::Panel::bottom("playback")
        .frame(
            egui::Frame::default()
                .fill(egui::Color32::from_rgb(24, 24, 24))
                .inner_margin(egui::Margin::symmetric(12, 8)),
        )
        .default_size(40.0)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                timeline_strip(ui, ui_state);
            });

            ui.horizontal(|ui| {
                if ui
                    .add(egui::Button::new(egui_phosphor::regular::CARET_LEFT))
                    .clicked()
                {
                    let _ = ui_state.tx.send(Message::Reverse);
                }

                let play_pause = if ui_state.playback_state.paused {
                    egui_phosphor::regular::PLAY
                } else {
                    egui_phosphor::regular::PAUSE
                };

                if ui.add(egui::Button::new(play_pause)).clicked() {
                    let _ = ui_state.tx.send(Message::Pause);
                }
                if ui
                    .add(egui::Button::new(egui_phosphor::regular::CARET_RIGHT))
                    .clicked()
                {
                    let _ = ui_state.tx.send(Message::Forward);
                }

                ui.separator();

                if ui
                    .add(egui::Button::new(egui_phosphor::regular::SKIP_BACK_CIRCLE))
                    .clicked()
                {
                    let _ = ui_state.tx.send(Message::FirstFrame);
                }

                if ui
                    .add(egui::Button::new(egui_phosphor::regular::SKIP_BACK))
                    .clicked()
                {
                    let _ = ui_state.tx.send(Message::GoBack(1));
                }

                if ui
                    .add(egui::Button::new(egui_phosphor::regular::SKIP_FORWARD))
                    .clicked()
                {
                    let _ = ui_state.tx.send(Message::GoForward(1));
                }

                if ui
                    .add(egui::Button::new(
                        egui_phosphor::regular::SKIP_FORWARD_CIRCLE,
                    ))
                    .clicked()
                {
                    let _ = ui_state.tx.send(Message::LastFrame);
                }
                ui.separator();
                let mut multiplier = ui_state.playback_state.multiplier;
                ui.add_sized(
                    [20.0, 20.0],
                    egui::Slider::new(&mut multiplier, 0.1..=16.0).show_value(false),
                );

                if multiplier != ui_state.playback_state.multiplier {
                    let _ = ui_state.tx.send(Message::Multiplier(multiplier));
                }

                ui.separator();

                let mut frame_rate = ui_state.playback_state.frame_rate;
                let frame_rates = [23.976, 24.0, 25.0, 30.0, 45.0, 60.0];
                egui::ComboBox::from_label("fps")
                    .selected_text(frame_rate.to_string())
                    .width(40.0)
                    .show_ui(ui, |ui| {
                        for fr in frame_rates {
                            ui.selectable_value(&mut frame_rate, fr, fr.to_string());
                        }
                    });

                if frame_rate != ui_state.playback_state.frame_rate {
                    let _ = ui_state.tx.send(Message::FrameRate(frame_rate));
                }

                ui.separator();

                if frame_rate != 0.0 {
                    ui.label(RichText::new(frame_to_timecode(
                        ui_state.playback_state.current_frame,
                        frame_rate,
                    )));
                    ui.label("/");
                    ui.label(RichText::new(frame_to_timecode(
                        ui_state.playback_state.total_frames,
                        frame_rate,
                    )));
                }
            })
        });
    egui::Panel::left("config")
        .frame(egui::Frame::default())
        .default_size(148.0)
        .show(ctx, |ui| {
            ui.vertical_centered(|ui| ui.heading("Config"));
            let mut selected_display = color_state.selected_display.clone();
            egui::ComboBox::from_label("Displays")
                .selected_text(&selected_display)
                .show_ui(ui, |ui| {
                    for display in &color_state.available_displays {
                        ui.selectable_value(&mut selected_display, display.clone(), display);
                    }
                });

            if selected_display != color_state.selected_display {
                color_state.change_display(selected_display)
            }

            let mut selected_view = color_state.selected_view.clone();

            egui::ComboBox::from_label("Views")
                .selected_text(&selected_view)
                .show_ui(ui, |ui| {
                    for view in &color_state.available_views {
                        ui.selectable_value(&mut selected_view, view.clone(), view);
                    }
                });

            if selected_view != color_state.selected_view {
                color_state.change_view(selected_view);
            }

            if ui.button("Apply").clicked() {
                config.lut_bake = true
            }

            ui.add_space(40.0);

            let mut selected_channel_mode = ui_state.channel_mode;

            egui::ComboBox::from_label("Channel Mode")
                .selected_text(selected_channel_mode.to_string())
                .show_ui(ui, |ui| {
                    for mode in ChannelMode::iter() {
                        ui.selectable_value(&mut selected_channel_mode, mode, mode.to_string());
                    }
                });

            if selected_channel_mode != ui_state.channel_mode {
                ui_state.channel_mode = selected_channel_mode;
                config.channel_mode = Some(selected_channel_mode as u32)
            }
        });

    egui::CentralPanel::default()
        .frame(egui::Frame::default().fill(egui::Color32::from_rgb(32, 32, 32)))
        .show(ctx, |ui| {
            if ui_state.playback_state.open_dir.is_none() {
                ui.centered_and_justified(|ui| {
                    ui.heading("Choose a sequence directory");
                });

                return;
            }

            if let Some(gpu_img) = &exr_gpu_img {
                ui.label(
                    egui::RichText::new(format!(
                        "Resolution: {}x{}",
                        gpu_img.width, gpu_img.height
                    ))
                    .color(egui::Color32::WHITE),
                );

                let available = ui.available_size();
                let aspect_ratio = gpu_img.width as f32 / gpu_img.height as f32;
                let mut display_size = egui::vec2(available.x, available.x / aspect_ratio);
                if display_size.y > available.y {
                    display_size = egui::vec2(available.y * aspect_ratio, available.y);
                }
                ui.centered_and_justified(|ui| {
                    ui.add(egui::Image::from_texture(egui::load::SizedTexture::new(
                        gpu_img.egui_tex_id,
                        display_size,
                    )));
                });
            } else {
                ui.label("Loading sequence...");
            }
        });
}

fn frame_to_timecode(frame: u64, fps: f64) -> String {
    let total_seconds = frame as f64 / fps;

    let hours = (total_seconds / 3600.0) as u64;
    let minutes = ((total_seconds % 3600.0) / 60.0) as u64;
    let seconds = total_seconds % 60.0;

    format!("{:02}:{:02}:{:05.3}", hours, minutes, seconds)
}

fn timeline_strip(ui: &mut Ui, ui_state: &mut UiState) -> egui::Response {
    let Some((thumb_w, thumb_h)) = ui_state.thumb_size else {
        let (_, response) =
            ui.allocate_exact_size([ui.available_width(), 40.0].into(), Sense::click_and_drag());
        return response;
    };

    let total_frames = ui_state.playback_state.total_frames;
    let current_frame = ui_state.playback_state.current_frame;

    let (rect, response) = ui.allocate_exact_size(
        [ui.available_width(), thumb_h].into(),
        Sense::click_and_drag(),
    );

    let playhead_pos = current_frame as f32 * thumb_w;
    let margin = thumb_w * 6.0; 
    if playhead_pos < ui_state.scroll_offset + margin {
        ui_state.scroll_offset = (playhead_pos - margin).max(0.0);
    } else if playhead_pos > ui_state.scroll_offset + rect.width() - margin {
        ui_state.scroll_offset = playhead_pos - rect.width() + margin;
    }

    let painter = ui.painter_at(rect);

    painter.rect_filled(rect, 0.0, egui::Color32::from_rgb(18, 18, 18));

    let first_visible = (ui_state.scroll_offset / thumb_w).floor().max(0.0) as u64;
    let visible_count = (rect.width() / thumb_w).ceil() as u64 + 1;
    let last_visible = (first_visible + visible_count).min(total_frames);

    for frame in first_visible..last_visible {
        let x = rect.left() + (frame as f32 * thumb_w) - ui_state.scroll_offset;
        if x + thumb_w < rect.left() || x > rect.right() {
            continue;
        }

        let thumb_rect =
            egui::Rect::from_min_size(egui::pos2(x, rect.top()), egui::vec2(thumb_w, thumb_h));

        if let Some(handle) = ui_state.thumb_cache.get(frame) {
            painter.image(
                handle.texture,
                thumb_rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        } else {
            painter.rect_filled(thumb_rect, 0.0, egui::Color32::from_rgb(40, 40, 40));
        }
    }

    let playhead_x = rect.left() + (current_frame as f32 * thumb_w) - ui_state.scroll_offset;
    if playhead_x >= rect.left() && playhead_x <= rect.right() {
        painter.line_segment(
            [
                egui::pos2(playhead_x, rect.top()),
                egui::pos2(playhead_x, rect.bottom()),
            ],
            egui::Stroke::new(2.0, egui::Color32::from_rgb(255, 80, 80)),
        );
    }

    if (response.clicked() || response.dragged())
        && let Some(pos) = response.interact_pointer_pos()
    {
        let clicked_frame = ((pos.x - rect.left() + ui_state.scroll_offset) / thumb_w) as u64;
        let clicked_frame = clicked_frame.min(total_frames.saturating_sub(1));
        let _ = ui_state.tx.send(Message::FrameChange(clicked_frame));
    }

    response
}
