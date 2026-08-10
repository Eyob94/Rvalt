use ocio_rs::Config;
use wgpu::util::DeviceExt;

pub enum BakeMessage {
    Failed(String),
    Done {
        size: usize,
        data: Vec<f32>,
        min_exp: f32,
        max_exp: f32,
    },
}

#[derive(Default)]
pub struct ColorState {
    pub config: Option<ocio_rs::Config>,
    pub config_path: Option<String>,
    pub config_error: Option<String>,
    pub src_color_space: String,
    pub available_displays: Vec<String>,
    pub available_views: Vec<String>,
    pub selected_view: String,
    pub selected_display: String,
    pub min_exp: f32,
    pub max_exp: f32,
}

impl ColorState {
    pub fn load_config() -> Self {
        let path = match Self::resolve_ocio_config_path() {
            Some(p) => p,
            None => {
                return Self {
                    config_error: Some("OCIO path missing".into()),
                    ..Default::default()
                };
            }
        };
        match Config::from_file(&path) {
            Ok(config) => {
                let available_displays: Vec<String> = (0..config.num_displays())
                    .filter_map(|i| config.display(i))
                    .collect();
                let selected_display = available_displays.first().cloned().unwrap_or_default();
                let available_views: Vec<String> = (0..config.num_views(&selected_display))
                    .filter_map(|i| config.view(&selected_display, i))
                    .collect();
                Self {
                    config: Some(config),
                    config_error: None,
                    config_path: Some(path.to_string()),
                    src_color_space: "ACEScg".into(),
                    selected_display,
                    selected_view: available_views.first().cloned().unwrap_or_default(),
                    available_displays,
                    available_views,
                    min_exp: -6.5,
                    max_exp: 6.5,
                }
            }
            Err(e) => Self {
                config_error: Some(e.to_string()),
                ..Default::default()
            },
        }
    }

    pub(crate) fn resolve_ocio_config_path() -> Option<String> {
        if let Ok(path) = std::env::var("OCIO") {
            return Some(path);
        }
        Some("/Users/eyob/rvalt/assets/studio-config-v4.0.0_aces-v2.0_ocio-v2.5.ocio".to_string())
    }

    pub fn change_display(&mut self, new_display: impl Into<String>) {
        let Some(config) = &self.config else { return };
        let new_display = new_display.into();

        self.available_views = (0..config.num_views(&new_display))
            .filter_map(|i| config.view(&new_display, i))
            .collect();
        self.selected_view = self.available_views.first().cloned().unwrap_or_default();
        self.selected_display = new_display;
    }

    pub fn change_view(&mut self, new_view: impl Into<String>) {
        self.selected_view = new_view.into();
    }
}

pub fn upload_lut(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    lut: &[f32],
    size: usize,
) -> (wgpu::Texture, wgpu::TextureView, wgpu::Sampler) {
    let mut rgba = Vec::with_capacity(lut.len() / 3 * 4);
    for chunk in lut.chunks(3) {
        rgba.push(half::f16::from_f32(chunk[0]));
        rgba.push(half::f16::from_f32(chunk[1]));
        rgba.push(half::f16::from_f32(chunk[2]));
        rgba.push(half::f16::from_f32(1.0));
    }

    let texture = device.create_texture_with_data(
        queue,
        &wgpu::TextureDescriptor {
            label: Some("ocio_lut"),
            size: wgpu::Extent3d {
                width: size as u32,
                height: size as u32,
                depth_or_array_layers: size as u32,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D3,
            format: wgpu::TextureFormat::Rgba16Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        },
        wgpu::util::TextureDataOrder::LayerMajor,
        bytemuck::cast_slice(&rgba),
    );

    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("ocio_lut_sampler"),
        address_mode_u: wgpu::AddressMode::ClampToEdge,
        address_mode_w: wgpu::AddressMode::ClampToEdge,
        address_mode_v: wgpu::AddressMode::ClampToEdge,
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });

    (texture, view, sampler)
}

fn linear_to_shaper(linear: f32, min_exp: f32, max_exp: f32) -> f32 {
    ((linear.max(1e-10).log2() - min_exp) / (max_exp - min_exp)).clamp(0.0, 1.0)
}

fn shaper_to_linear(shaper: f32, min_exp: f32, max_exp: f32) -> f32 {
    2f32.powf((shaper * (max_exp - min_exp)) + min_exp)
}

pub fn bake_lut(cpu: &ocio_rs::CPUProcessor, size: usize, min_exp: f32, max_exp: f32) -> Vec<f32> {
    let mut data = Vec::with_capacity((size.pow(3)) * 3);
    if size <= 1 {
        // Size cannot be less than or equal to 1
        return data;
    }
    for b in 0..size {
        for g in 0..size {
            for r in 0..size {
                let shaper_r = r as f32 / (size - 1) as f32;
                let shaper_g = g as f32 / (size - 1) as f32;
                let shaper_b = b as f32 / (size - 1) as f32;

                let lin_r = shaper_to_linear(shaper_r, min_exp, max_exp);
                let lin_g = shaper_to_linear(shaper_g, min_exp, max_exp);
                let lin_b = shaper_to_linear(shaper_b, min_exp, max_exp);

                let mut rgb = [lin_r, lin_g, lin_b];
                cpu.apply_rgb(&mut rgb);

                data.extend(rgb);
            }
        }
    }

    data
}
