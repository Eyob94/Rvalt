use exr::prelude::{Sample, read_first_rgba_layer_from_file};
use image::{ImageBuffer, Rgb};
use ocio_rs::{Config, TransformDirection};

fn linear_to_shaper(linear: f32, min_exp: f32, max_exp: f32) -> f32 {
    ((linear.max(1e-10).log2() - min_exp) / (max_exp - min_exp)).clamp(0.0, 1.0)
}

fn shaper_to_linear(shaper: f32, min_exp: f32, max_exp: f32) -> f32 {
    2f32.powf((shaper * (max_exp - min_exp)) + min_exp)
}

fn bake_lut(cpu: &ocio_rs::CPUProcessor, size: usize, min_exp: f32, max_exp: f32) -> Vec<f32> {
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

fn lerp(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

fn flat_index(size: usize, r: usize, g: usize, b: usize) -> usize {
    (b * size * size + g * size + r) * 3
}

fn fetch(data: &[f32], size: usize, r: usize, g: usize, b: usize) -> [f32; 3] {
    let i = flat_index(size, r, g, b);
    [data[i], data[i + 1], data[i + 2]]
}

fn sample_lut_trilinear(data: &[f32], size: usize, r: f32, g: f32, b: f32) -> [f32; 3] {
    let fr = r.clamp(0.0, 1.0) * (size - 1) as f32;
    let fg = g.clamp(0.0, 1.0) * (size - 1) as f32;
    let fb = b.clamp(0.0, 1.0) * (size - 1) as f32;

    let r0 = fr.floor() as usize;
    let g0 = fg.floor() as usize;
    let b0 = fb.floor() as usize;

    let r1 = (r0 + 1).min(size - 1);
    let g1 = (g0 + 1).min(size - 1);
    let b1 = (b0 + 1).min(size - 1);

    let tr = fr - r0 as f32;
    let tg = fg - g0 as f32;
    let tb = fb - b0 as f32;

    let c00 = lerp(
        fetch(data, size, r0, g0, b0),
        fetch(data, size, r1, g0, b0),
        tr,
    );
    let c10 = lerp(
        fetch(data, size, r0, g1, b0),
        fetch(data, size, r1, g1, b0),
        tr,
    );
    let c01 = lerp(
        fetch(data, size, r0, g0, b1),
        fetch(data, size, r1, g0, b1),
        tr,
    );
    let c11 = lerp(
        fetch(data, size, r0, g1, b1),
        fetch(data, size, r1, g1, b1),
        tr,
    );

    // 4 -> 2 (lerp along g)
    let c0 = lerp(c00, c10, tg);
    let c1 = lerp(c01, c11, tg);

    // 2 -> 1 (lerp along b)
    lerp(c0, c1, tb)
}

fn main() -> anyhow::Result<()> {
    let config = Config::from_file("./studio-config-v4.0.0_aces-v2.0_ocio-v2.5.ocio")?;
    let processor = config.processor_display(
        "ACEScg",
        "sRGB - Display",
        "ACES 2.0 - SDR 100 nits (Rec.709)",
        TransformDirection::Forward,
    )?;

    let cpu = processor.default_cpu_processor()?;

    let size = 32;
    let min_exp = -6.5;
    let max_exp = 6.5;
    let lut = bake_lut(&cpu, size, min_exp, max_exp);

    let data = read_first_rgba_layer_from_file(
        "./SPARKS_P3_PQ_4000nit_00511.exr",
        |resolution, _| {
            let default_pixel = [0.0, 0.0, 0.0, 0.0];
            let empty_line = vec![default_pixel; resolution.width()];
            let empty_image = vec![empty_line; resolution.height()];
            empty_image
        },
        |pixel_vector, position, (r, g, b, a): (f32, f32, f32, f32)| {
            pixel_vector[position.y()][position.x()] = [r, g, b, a];
        },
    )?;

    let width = data.attributes.display_window.size.width() as u32;
    let height = data.attributes.display_window.size.height() as u32;

    let mut out: ImageBuffer<Rgb<u8>, Vec<u8>> = ImageBuffer::new(width, height);
    for (y, row) in data.layer_data.channel_data.pixels.into_iter().enumerate() {
        for (x, [r, g, b, _]) in row.into_iter().enumerate() {
            let (r, g, b) = (
                linear_to_shaper(r, min_exp, max_exp),
                linear_to_shaper(g, min_exp, max_exp),
                linear_to_shaper(b, min_exp, max_exp),
            );
            let display = sample_lut_trilinear(&lut, size, r, g, b);


            let (dr, dg, db) = (display[0], display[1], display[2]);

            let r8 = (dr.clamp(0.0, 1.0) * 255.0) as u8;
            let g8 = (dg.clamp(0.0, 1.0) * 255.0) as u8;
            let b8 = (db.clamp(0.0, 1.0) * 255.0) as u8;

            out.put_pixel(x as u32, y as u32, Rgb([r8, g8, b8]));
        }
    }

    out.save("stage2_output.png")?;
    println!("Wrote stage2_output.png");

    Ok(())
}
