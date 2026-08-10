use std::{fmt::Display, path::Path};

use exr::prelude::*;
use strum_macros::{Display, EnumIter};
use tracing::{debug, info};
use wgpu::{include_wgsl, util::DeviceExt};

use crate::ocio::upload_lut;

#[derive(Debug, Clone, Default)]
pub struct ExrImage {
    pub width: usize,
    pub height: usize,
    pub r: Vec<half::f16>,
    pub g: Vec<half::f16>,
    pub b: Vec<half::f16>,
    pub a: Vec<half::f16>,
}
fn channel_to_f16(samples: &FlatSamples) -> Option<&[half::f16]> {
    match samples {
        FlatSamples::F16(v) => Some(v.as_slice()),
        _ => None,
    }
}

fn channel_to_f16_owned(samples: &FlatSamples) -> Vec<half::f16> {
    match channel_to_f16(samples) {
        Some(slice) => slice.to_vec(),
        None => match samples {
            FlatSamples::F32(v) => v.iter().map(|&f| half::f16::from_f32(f)).collect(),
            FlatSamples::U32(v) => v.iter().map(|&u| half::f16::from_f32(u as f32)).collect(),
            FlatSamples::F16(_) => unreachable!(),
        },
    }
}

pub fn load_exr(path: impl AsRef<Path>) -> anyhow::Result<ExrImage> {
    let t0 = std::time::Instant::now();

    let t_read0 = std::time::Instant::now();
    let bytes = std::fs::read(path.as_ref())?;
    let t_read1 = std::time::Instant::now();

    let image = read()
        .no_deep_data()
        .largest_resolution_level()
        .all_channels()
        .all_layers()
        .all_attributes()
        .from_file(path.as_ref())?;
    let t1 = std::time::Instant::now();

    let layer = image
        .layer_data
        .first()
        .ok_or_else(|| anyhow::anyhow!("no layers"))?;
    let size = layer.size;
    let width = size.width();
    let height = size.height();
    let find_channel = |name: &str| -> Option<&FlatSamples> {
        layer
            .channel_data
            .list
            .iter()
            .find(|c| c.name.to_string().eq_ignore_ascii_case(name))
            .map(|c| &c.sample_data)
    };
    let r = find_channel("R").ok_or_else(|| anyhow::anyhow!("missing R channel"))?;
    let g = find_channel("G").ok_or_else(|| anyhow::anyhow!("missing G channel"))?;
    let b = find_channel("B").ok_or_else(|| anyhow::anyhow!("missing B channel"))?;
    let a = find_channel("A");

    let pixel_count = width * height;
    let t2 = std::time::Instant::now();

    let r_vals = channel_to_f16_owned(r);
    let g_vals = channel_to_f16_owned(g);
    let b_vals = channel_to_f16_owned(b);
    let a_vals: Vec<half::f16> = match a {
        Some(channel) => channel_to_f16_owned(channel),
        None => vec![half::f16::from_f32(1.0); pixel_count],
    };

    let t3 = std::time::Instant::now();
    info!(
        pixel_count,
        raw_read_ms = t_read1.duration_since(t_read0).as_millis(),
        read_ms = t1.duration_since(t0).as_millis(),
        channel_lookup_ms = t2.duration_since(t1).as_millis(),
        collect_channels_ms = t3.duration_since(t2).as_millis(),
        total_ms = t3.duration_since(t0).as_millis(),
        "load_exr timing breakdown"
    );

    Ok(ExrImage {
        width,
        height,
        r: r_vals,
        g: g_vals,
        b: b_vals,
        a: a_vals,
    })
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
struct ShaperUniforms {
    min_exp: f32,
    max_exp: f32,
    channel_mode: u32,
    _padding: f32,
}

#[derive(Default, Clone, Debug, Copy, PartialEq, EnumIter, Display)]
#[repr(u32)]
pub enum ChannelMode {
    #[default]
    Rgba = 0,
    Red = 1,
    Green = 2,
    Blue = 3,
    Alpha = 4,
    Luminance = 5,
}

pub struct GpuState {
    pub(crate) device: wgpu::Device,
    pub(crate) queue: wgpu::Queue,
    pub(crate) surface: wgpu::Surface<'static>,
    pub(crate) surface_config: wgpu::SurfaceConfiguration,
}

pub struct ExrGpuImage {
    pub width: u32,
    pub height: u32,
    pub r_texture: wgpu::Texture,
    pub g_texture: wgpu::Texture,
    pub b_texture: wgpu::Texture,
    pub a_texture: wgpu::Texture,
    pub egui_tex_id: egui::TextureId,
    pub display_view: wgpu::TextureView,
    bind_group: wgpu::BindGroup,
}

pub struct ExrRenderer {
    pub(crate) pipeline: wgpu::RenderPipeline,
    pub(crate) bind_group_layout: wgpu::BindGroupLayout,
    pub(crate) sampler: wgpu::Sampler,
    lut_bind_group_layout: wgpu::BindGroupLayout,
    lut_sampler: wgpu::Sampler,
    lut_bind_group: wgpu::BindGroup,
    shaper_buffer: wgpu::Buffer,
    channel_mode: u32,
    min_exp: f32,
    max_exp: f32,
}

impl ExrRenderer {
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue) -> Self {
        let shader = device.create_shader_module(include_wgsl!("./shader.wgsl"));

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("exr_bind_group_layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let lut_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("lut_bind_group_layout"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D3,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 2,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                ],
            });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("exr_pipeline_layout"),
            bind_group_layouts: &[Some(&bind_group_layout), Some(&lut_bind_group_layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("exr_display_pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba8UnormSrgb,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("exr_sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });

        let shaper_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("shaper_uniforms"),
            contents: bytemuck::cast_slice(&[ShaperUniforms {
                min_exp: -6.5,
                max_exp: 6.5,
                channel_mode: 0,
                _padding: 0.0,
            }]),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        let identity_size = 2;
        let identity_data = identity_lut(identity_size);
        let (lut_texture, lut_view, lut_sampler) =
            upload_lut(device, queue, &identity_data, identity_size);

        let lut_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("lut_bind_group"),
            layout: &lut_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&lut_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&lut_sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: shaper_buffer.as_entire_binding(),
                },
            ],
        });

        Self {
            pipeline,
            bind_group_layout,
            sampler,
            lut_bind_group_layout,
            lut_sampler,
            lut_bind_group,
            shaper_buffer,
            channel_mode: 0,
            min_exp: -6.5,
            max_exp: 6.5,
        }
    }

    pub fn set_channel_mode(&mut self, queue: &wgpu::Queue, mode: u32) {
        self.channel_mode = mode;

        queue.write_buffer(
            &self.shaper_buffer,
            0,
            bytemuck::cast_slice(&[ShaperUniforms {
                min_exp: self.min_exp,
                max_exp: self.max_exp,
                channel_mode: mode,
                _padding: 0.0,
            }]),
        );
    }

    pub fn set_color_lut(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        lut_data: &[f32],
        size: usize,
        min_exp: f32,
        max_exp: f32,
    ) {
        let (_texture, view, sampler) = upload_lut(device, queue, lut_data, size);

        queue.write_buffer(
            &self.shaper_buffer,
            0,
            bytemuck::cast_slice(&[ShaperUniforms {
                min_exp,
                max_exp,
                channel_mode: self.channel_mode,
                _padding: 0.0,
            }]),
        );

        self.lut_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("lut_bind_group"),
            layout: &self.lut_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.lut_sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: self.shaper_buffer.as_entire_binding(),
                },
            ],
        });
    }
    pub fn upload_and_prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        egui_renderer: &mut egui_wgpu::Renderer,
        img: &ExrImage,
    ) -> ExrGpuImage {
        let width = img.width as u32;
        let height = img.height as u32;

        let make_channel_tex = |data: &[half::f16], label: &str| -> wgpu::Texture {
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::R16Float,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                bytemuck::cast_slice(data),
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(width * 2), // 1 channel, 2 bytes (f16) per pixel
                    rows_per_image: Some(height),
                },
                wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
            );
            texture
        };

        let r_texture = make_channel_tex(&img.r, "exr_r_channel");
        let g_texture = make_channel_tex(&img.g, "exr_g_channel");
        let b_texture = make_channel_tex(&img.b, "exr_b_channel");
        let a_texture = make_channel_tex(&img.a, "exr_a_channel");

        let r_view = r_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let g_view = g_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let b_view = b_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let a_view = a_texture.create_view(&wgpu::TextureViewDescriptor::default());

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("exr_bind_group"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&r_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&g_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&b_view),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(&a_view),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        let display_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("exr_display_target"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let display_view = display_texture.create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("exr_display_pass_encoder"),
        });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("exr_display_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &display_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.set_bind_group(1, &self.lut_bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        queue.submit(Some(encoder.finish()));

        let egui_tex_id =
            egui_renderer.register_native_texture(device, &display_view, wgpu::FilterMode::Linear);

        ExrGpuImage {
            width,
            height,
            r_texture,
            g_texture,
            b_texture,
            a_texture,
            egui_tex_id,
            display_view,
            bind_group,
        }
    }

    pub fn render_update(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        gpu_img: &ExrGpuImage,
        display_view: &wgpu::TextureView,
    ) {
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("exr_update_pass_encoder"),
        });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("exr_update_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: display_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &gpu_img.bind_group, &[]);
            pass.set_bind_group(1, &self.lut_bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        queue.submit(Some(encoder.finish()));
    }
}

fn identity_lut(size: usize) -> Vec<f32> {
    let mut data = Vec::with_capacity(size * size * size * 3);
    for b in 0..size {
        for g in 0..size {
            for r in 0..size {
                data.push(r as f32 / (size - 1) as f32);
                data.push(g as f32 / (size - 1) as f32);
                data.push(b as f32 / (size - 1) as f32);
            }
        }
    }
    data
}
