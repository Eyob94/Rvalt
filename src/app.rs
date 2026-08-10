use std::sync::Arc;
use std::thread;

use crossbeam::channel::{Receiver, unbounded};
use egui_wgpu::RendererOptions;
use tracing::error;
use wgpu::CurrentSurfaceTexture;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Window, WindowId};

use crate::exr::{ExrGpuImage, ExrImage, ExrRenderer, GpuState};
use crate::ocio::{BakeMessage, ColorState, bake_lut};
use crate::orchestrator::{Message, orchestrate};
use crate::thumbnail::ThumbHandle;
use crate::ui::{self, Config, UiState};

#[derive(Default)]
pub struct App {
    window: Option<Arc<Window>>,
    gpu: Option<GpuState>,
    egui_ctx: egui::Context,
    egui_state: Option<egui_winit::State>,
    egui_renderer: Option<egui_wgpu::Renderer>,
    exr_renderer: Option<ExrRenderer>,
    exr_gpu_image: Option<ExrGpuImage>,
    color_state: ColorState,
    bake_receiver: Option<Receiver<BakeMessage>>,
    frame_receiver: Option<Receiver<Arc<ExrImage>>>,
    ui_state: Option<UiState>,
    thumb_receiver: Option<Receiver<(u64, ExrImage)>>,
}

impl App {
    pub fn new() -> Self {
        Self::default()
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }

        let window = Arc::new(
            event_loop
                .create_window(
                    Window::default_attributes()
                        .with_title("rvalt")
                        .with_maximized(true),
                )
                .expect("failed to create window"),
        );
        let size = window.inner_size();

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            flags: Default::default(),
            memory_budget_thresholds: Default::default(),
            backend_options: Default::default(),
            display: None,
        });

        let surface = instance
            .create_surface(window.clone())
            .expect("failed to create surface");

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
        .unwrap();

        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).unwrap();

        let surface_caps = surface.get_capabilities(&adapter);
        let surface_format = surface_caps
            .formats
            .iter()
            .find(|f| !f.is_srgb())
            .copied()
            .unwrap_or(surface_caps.formats[0]);

        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: surface_format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: surface_caps.present_modes[0],
            alpha_mode: surface_caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };

        surface.configure(&device, &surface_config);

        let egui_state = egui_winit::State::new(
            self.egui_ctx.clone(),
            egui::ViewportId::ROOT,
            &window,
            Some(window.scale_factor() as f32),
            None,
            None,
        );

        let egui_renderer =
            egui_wgpu::Renderer::new(&device, surface_format, RendererOptions::default());
        let exr_renderer = ExrRenderer::new(&device, &queue);

        let color_state = ColorState::load_config();
        self.color_state = color_state;

        let (frame_tx, frame_rx) = unbounded();
        let (msg_tx, msg_rx) = unbounded();
        let (info_tx, info_rx) = unbounded();
        let (thumb_tx, thumb_rx) = unbounded();

        self.thumb_receiver = Some(thumb_rx);

        thread::spawn(|| {
            if let Err(e) = orchestrate(msg_rx, frame_tx, info_tx, thumb_tx).join() {
                error!(?e, "Error running orchestrator");
            };
        });

        let mut fonts = egui::FontDefinitions::default();
        egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);

        self.egui_ctx.set_fonts(fonts);

        self.frame_receiver = Some(frame_rx);

        self.ui_state = Some(UiState::new(msg_tx, info_rx));

        self.window = Some(window.clone());
        self.gpu = Some(GpuState {
            device,
            queue,
            surface,
            surface_config,
        });
        self.egui_state = Some(egui_state);
        self.egui_renderer = Some(egui_renderer);
        self.exr_renderer = Some(exr_renderer);

        window.request_redraw();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let (Some(window), Some(gpu), Some(egui_state), Some(egui_renderer), Some(exr_renderer)) = (
            &self.window,
            &mut self.gpu,
            &mut self.egui_state,
            &mut self.egui_renderer,
            &mut self.exr_renderer,
        ) else {
            return;
        };

        let response = egui_state.on_window_event(window, &event);
        if response.consumed {
            window.request_redraw();
            return;
        }

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),

            WindowEvent::Resized(new_size) => {
                if new_size.width > 0 && new_size.height > 0 {
                    gpu.surface_config.width = new_size.width;
                    gpu.surface_config.height = new_size.height;
                    gpu.surface.configure(&gpu.device, &gpu.surface_config);
                }
            }

            WindowEvent::RedrawRequested => {
                if let Some(rx) = (&self.frame_receiver)
                    && let Ok(new_frame_data) = rx.try_recv()
                {
                    match &self.exr_gpu_image {
                        Some(gpu_img) => {
                            let write_channel = |texture: &wgpu::Texture, data: &[half::f16]| {
                                gpu.queue.write_texture(
                                    wgpu::TexelCopyTextureInfo {
                                        texture,
                                        mip_level: 0,
                                        origin: wgpu::Origin3d::ZERO,
                                        aspect: wgpu::TextureAspect::All,
                                    },
                                    bytemuck::cast_slice(data),
                                    wgpu::TexelCopyBufferLayout {
                                        offset: 0,
                                        bytes_per_row: Some(gpu_img.width * 2), // 1 channel, 2 bytes (f16)
                                        rows_per_image: Some(gpu_img.height),
                                    },
                                    wgpu::Extent3d {
                                        width: gpu_img.width,
                                        height: gpu_img.height,
                                        depth_or_array_layers: 1,
                                    },
                                );
                            };

                            write_channel(&gpu_img.r_texture, &new_frame_data.r);
                            write_channel(&gpu_img.g_texture, &new_frame_data.g);
                            write_channel(&gpu_img.b_texture, &new_frame_data.b);
                            write_channel(&gpu_img.a_texture, &new_frame_data.a);

                            exr_renderer.render_update(
                                &gpu.device,
                                &gpu.queue,
                                gpu_img,
                                &gpu_img.display_view,
                            );
                        }
                        None => {
                            let gpu_img = exr_renderer.upload_and_prepare(
                                &gpu.device,
                                &gpu.queue,
                                egui_renderer,
                                &new_frame_data,
                            );
                            if let Some(ui_state) = &mut self.ui_state {
                                const THUMB_W: f32 = 96.0;
                                let thumb_h =
                                    THUMB_W * gpu_img.height as f32 / gpu_img.width as f32;
                                ui_state.thumb_size = Some((THUMB_W, thumb_h));
                            };
                            self.exr_gpu_image = Some(gpu_img);
                        }
                    }
                }
                if let Some(rx) = &self.bake_receiver
                    && let Ok(bake_data) = rx.try_recv()
                {
                    let BakeMessage::Done {
                        size,
                        data,
                        min_exp,
                        max_exp,
                    } = bake_data
                    else {
                        return;
                    };
                    exr_renderer.set_color_lut(
                        &gpu.device,
                        &gpu.queue,
                        &data,
                        size,
                        min_exp,
                        max_exp,
                    );
                }

                if let Some(rx) = &self.thumb_receiver
                    && let Some(ui_state) = &self.ui_state
                {
while let Ok(small) = rx.try_recv() {
        let thumb_gpu_img = exr_renderer.upload_and_prepare(
            &gpu.device,
            &gpu.queue,
            egui_renderer,
            &small.1,
        );
        ui_state.thumb_cache.insert(
            small.0,
            ThumbHandle {
                texture: thumb_gpu_img.egui_tex_id,
            },
        );
    }
                }

                let frame = match gpu.surface.get_current_texture() {
                    CurrentSurfaceTexture::Success(f) => f,
                    CurrentSurfaceTexture::Lost | CurrentSurfaceTexture::Outdated => {
                        gpu.surface.configure(&gpu.device, &gpu.surface_config);
                        window.request_redraw();
                        return;
                    }
                    CurrentSurfaceTexture::Timeout | CurrentSurfaceTexture::Occluded => {
                        window.request_redraw();
                        return;
                    }
                    e => {
                        eprintln!("Fatal error: Out of GPU memory! {e:#?}");
                        event_loop.exit();
                        return;
                    }
                };

                let raw_input = egui_state.take_egui_input(window);
                let mut config = Config::default();

                let full_output = self.egui_ctx.run_ui(raw_input, |ctx| {
                    ui::ui(
                        ctx,
                        &mut self.color_state,
                        &mut self.exr_gpu_image,
                        &mut config,
                        &mut self.ui_state,
                    )
                });

                egui_state.handle_platform_output(window, full_output.platform_output);
                let clipped_primitives = self
                    .egui_ctx
                    .tessellate(full_output.shapes, full_output.pixels_per_point);

                let view = frame
                    .texture
                    .create_view(&wgpu::TextureViewDescriptor::default());
                let mut encoder =
                    gpu.device
                        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                            label: Some("frame_encoder"),
                        });

                let screen_descriptor = egui_wgpu::ScreenDescriptor {
                    size_in_pixels: [gpu.surface_config.width, gpu.surface_config.height],
                    pixels_per_point: full_output.pixels_per_point,
                };

                let color_state = &self.color_state;

                if config.lut_bake {
                    println!("Baking");
                    let Some(config) = &color_state.config else {
                        return;
                    };
                    let display = color_state.selected_display.clone();
                    let view = color_state.selected_view.clone();
                    let (tx, rx) = unbounded();

                    self.bake_receiver = Some(rx);

                    let processor = config
                        .processor_display(
                            &color_state.src_color_space,
                            display,
                            view,
                            ocio_rs::TransformDirection::Forward,
                        )
                        .unwrap();
                    let cpu = processor.default_cpu_processor().unwrap();
                    let lut = bake_lut(&cpu, 32, -6.5, 6.5);

                    // TODO: Need to change this to user adjusted value
                    tx.send(BakeMessage::Done {
                        size: 32,
                        data: lut,
                        min_exp: -6.5,
                        max_exp: 6.5,
                    })
                    .unwrap();
                }

                if let Some(channel_mode) = config.channel_mode {
                    exr_renderer.set_channel_mode(&gpu.queue, channel_mode);
                }
                for (id, image_delta) in &full_output.textures_delta.set {
                    egui_renderer.update_texture(&gpu.device, &gpu.queue, *id, image_delta);
                }
                {
                    egui_renderer.update_buffers(
                        &gpu.device,
                        &gpu.queue,
                        &mut encoder,
                        &clipped_primitives,
                        &screen_descriptor,
                    );
                    let mut pass = encoder
                        .begin_render_pass(&wgpu::RenderPassDescriptor {
                            label: Some("egui_pass"),
                            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                                view: &view,
                                resolve_target: None,
                                ops: wgpu::Operations {
                                    load: wgpu::LoadOp::Clear(wgpu::Color {
                                        r: 0.0,
                                        g: 0.0,
                                        b: 0.0,
                                        a: 1.0,
                                    }),
                                    store: wgpu::StoreOp::Store,
                                },
                                depth_slice: None,
                            })],
                            depth_stencil_attachment: None,
                            timestamp_writes: None,
                            occlusion_query_set: None,
                            multiview_mask: None,
                        })
                        .forget_lifetime();
                    egui_renderer.render(&mut pass, &clipped_primitives, &screen_descriptor);
                }

                for id in &full_output.textures_delta.free {
                    egui_renderer.free_texture(id);
                }

                gpu.queue.submit(Some(encoder.finish()));
                frame.present();

                window.request_redraw();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                match (event.physical_key, event.state.is_pressed()) {
                    (PhysicalKey::Code(KeyCode::Space), true) => {
                        let Some(ui_state) = &self.ui_state else {
                            return;
                        };
                        let msg_tx = &ui_state.tx;
                        msg_tx.send(Message::Pause);
                    }
                    (PhysicalKey::Code(KeyCode::ArrowLeft), true) => {
                        let Some(ui_state) = &self.ui_state else {
                            return;
                        };
                        let msg_tx = &ui_state.tx;
                        msg_tx.send(Message::GoBack(1));
                    }
                    (PhysicalKey::Code(KeyCode::ArrowRight), true) => {
                        let Some(ui_state) = &self.ui_state else {
                            return;
                        };
                        let msg_tx = &ui_state.tx;
                        msg_tx.send(Message::GoForward(1));
                    }
                    (PhysicalKey::Code(KeyCode::KeyJ), true) => {
                        let Some(ui_state) = &self.ui_state else {
                            return;
                        };
                        let msg_tx = &ui_state.tx;
                        msg_tx.send(Message::Reverse);
                    }
                    (PhysicalKey::Code(KeyCode::KeyK), true) => {
                        let Some(ui_state) = &self.ui_state else {
                            return;
                        };
                        let msg_tx = &ui_state.tx;
                        msg_tx.send(Message::Pause);
                    }
                    (PhysicalKey::Code(KeyCode::KeyL), true) => {
                        let Some(ui_state) = &self.ui_state else {
                            return;
                        };
                        let msg_tx = &ui_state.tx;
                        msg_tx.send(Message::Forward);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}
