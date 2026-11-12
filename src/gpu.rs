//! wgpu renderer: one pipeline, one vertex format, one draw call per clip rect.
//! Rounded rects, circles and lines are shaded analytically (SDF) in the fragment shader;
//! text samples a single-channel glyph atlas.

use crate::ui::{Color, Painter, Vertex};
use std::sync::Arc;
use winit::window::Window;

const SHADER: &str = r#"
struct Globals { screen: vec2<f32>, scale: f32, linear_out: f32 };
@group(0) @binding(0) var<uniform> g: Globals;
@group(0) @binding(1) var atlas_tex: texture_2d<f32>;
@group(0) @binding(2) var atlas_smp: sampler;

struct VIn {
    @location(0) pos: vec2<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) color: vec4<f32>,
    @location(3) shape: vec4<f32>,
    @location(4) params: vec4<f32>,
};

struct VOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) shape: vec4<f32>,
    @location(3) params: vec4<f32>,
    @location(4) px: vec2<f32>,
};

@vertex
fn vs_main(v: VIn) -> VOut {
    var o: VOut;
    o.clip = vec4<f32>(v.pos.x / g.screen.x * 2.0 - 1.0, 1.0 - v.pos.y / g.screen.y * 2.0, 0.0, 1.0);
    o.uv = v.uv;
    o.color = v.color;
    o.shape = v.shape;
    o.params = v.params;
    o.px = v.pos;
    return o;
}

fn to_linear(c: vec3<f32>) -> vec3<f32> {
    let lo = c / 12.92;
    let hi = pow((c + vec3<f32>(0.055)) / 1.055, vec3<f32>(2.4));
    return select(hi, lo, c <= vec3<f32>(0.04045));
}

@fragment
fn fs_main(i: VOut) -> @location(0) vec4<f32> {
    let mode = i.params.x;
    var cov = 1.0;
    if (mode > 0.5 && mode < 1.5) {
        cov = textureSampleLevel(atlas_tex, atlas_smp, i.uv, 0.0).r;
    } else if (mode > 1.5 && mode < 2.5) {
        // Rounded rectangle, optionally stroked.
        let r = i.params.y;
        let q = abs(i.px - i.shape.xy) - i.shape.zw + vec2<f32>(r);
        var d = length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0) - r;
        if (i.params.z > 0.0) {
            d = abs(d + i.params.z * 0.5) - i.params.z * 0.5;
        }
        cov = clamp(0.5 - d * g.scale, 0.0, 1.0);
    } else if (mode > 2.5) {
        // Capsule (line segment with round caps).
        let pa = i.px - i.shape.xy;
        let ba = i.shape.zw - i.shape.xy;
        let h = clamp(dot(pa, ba) / max(dot(ba, ba), 1e-6), 0.0, 1.0);
        let d = length(pa - ba * h) - i.params.y;
        cov = clamp(0.5 - d * g.scale, 0.0, 1.0);
    }
    var rgb = i.color.rgb;
    if (g.linear_out > 0.5) {
        rgb = to_linear(rgb);
    }
    let a = i.color.a * cov;
    return vec4<f32>(rgb * a, a);
}
"#;

fn bytes<T: Copy>(s: &[T]) -> &[u8] {
    // SAFETY: T is a plain #[repr(C)] POD type (f32/u32 aggregates) without padding.
    unsafe { std::slice::from_raw_parts(s.as_ptr() as *const u8, std::mem::size_of_val(s)) }
}

pub struct Renderer {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    ubuf: wgpu::Buffer,
    atlas: wgpu::Texture,
    atlas_size: u32,
    vbuf: wgpu::Buffer,
    ibuf: wgpu::Buffer,
    linear_out: bool,
}

impl Renderer {
    pub fn new(device: wgpu::Device, queue: wgpu::Queue, format: wgpu::TextureFormat, atlas_size: u32) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ui"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let ubuf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("globals"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let atlas = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("glyph atlas"),
            size: wgpu::Extent3d { width: atlas_size, height: atlas_size, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("atlas"),
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ui"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
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
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let view = atlas.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("ui"),
            layout: &bgl,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: ubuf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&view) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&sampler) },
            ],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("ui"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });
        let attrs = wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2, 2 => Float32x4, 3 => Float32x4, 4 => Float32x4];
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("ui"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<Vertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &attrs,
                })],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let vbuf = Self::buffer(&device, 1 << 20, wgpu::BufferUsages::VERTEX);
        let ibuf = Self::buffer(&device, 1 << 18, wgpu::BufferUsages::INDEX);
        Renderer {
            device,
            queue,
            pipeline,
            bind_group,
            ubuf,
            atlas,
            atlas_size,
            vbuf,
            ibuf,
            linear_out: format.is_srgb(),
        }
    }

    fn buffer(device: &wgpu::Device, size: u64, usage: wgpu::BufferUsages) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size,
            usage: usage | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    /// Records and submits one frame into `target`.
    pub fn render(&mut self, target: &wgpu::TextureView, painter: &mut Painter, phys: (u32, u32), clear: Color) {
        let atlas = &mut painter.fonts.atlas;
        if atlas.dirty {
            debug_assert_eq!(atlas.size, self.atlas_size);
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &self.atlas,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &atlas.data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(atlas.size),
                    rows_per_image: Some(atlas.size),
                },
                wgpu::Extent3d { width: atlas.size, height: atlas.size, depth_or_array_layers: 1 },
            );
            atlas.dirty = false;
        }
        let s = painter.scale;
        let globals = [phys.0 as f32 / s, phys.1 as f32 / s, s, if self.linear_out { 1.0 } else { 0.0 }];
        self.queue.write_buffer(&self.ubuf, 0, bytes(&globals));

        let vb = bytes(&painter.verts);
        let ib = bytes(&painter.indices);
        if vb.len() as u64 > self.vbuf.size() {
            self.vbuf = Self::buffer(&self.device, (vb.len() as u64).next_power_of_two(), wgpu::BufferUsages::VERTEX);
        }
        if ib.len() as u64 > self.ibuf.size() {
            self.ibuf = Self::buffer(&self.device, (ib.len() as u64).next_power_of_two(), wgpu::BufferUsages::INDEX);
        }
        if !vb.is_empty() {
            self.queue.write_buffer(&self.vbuf, 0, vb);
            self.queue.write_buffer(&self.ibuf, 0, ib);
        }

        let c = if self.linear_out { clear.0.map(srgb_to_linear) } else { clear.0 };
        let mut enc = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("ui") });
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("ui"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: c[0] as f64,
                            g: c[1] as f64,
                            b: c[2] as f64,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if !vb.is_empty() {
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &self.bind_group, &[]);
                pass.set_vertex_buffer(0, self.vbuf.slice(..));
                pass.set_index_buffer(self.ibuf.slice(..), wgpu::IndexFormat::Uint32);
                for cmd in painter.cmds.iter().filter(|c| c.count > 0) {
                    let x0 = (cmd.clip.x * s).floor().clamp(0.0, phys.0 as f32) as u32;
                    let y0 = (cmd.clip.y * s).floor().clamp(0.0, phys.1 as f32) as u32;
                    let x1 = (cmd.clip.right() * s).ceil().clamp(0.0, phys.0 as f32) as u32;
                    let y1 = (cmd.clip.bottom() * s).ceil().clamp(0.0, phys.1 as f32) as u32;
                    if x1 <= x0 || y1 <= y0 {
                        continue;
                    }
                    pass.set_scissor_rect(x0, y0, x1 - x0, y1 - y0);
                    pass.draw_indexed(cmd.start..cmd.start + cmd.count, 0, 0..1);
                }
            }
        }
        self.queue.submit([enc.finish()]);
    }
}

fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}

const ATLAS_SIZE: u32 = 2048;

async fn adapter_device(
    instance: &wgpu::Instance,
    surface: Option<&wgpu::Surface<'_>>,
) -> Result<(wgpu::Adapter, wgpu::Device, wgpu::Queue), String> {
    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: surface,
            force_fallback_adapter: false,
            apply_limit_buckets: false,
        })
        .await
        .map_err(|e| format!("no suitable GPU adapter: {e}"))?;
    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor {
            label: Some("wgitpui"),
            required_limits: wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
            ..Default::default()
        })
        .await
        .map_err(|e| format!("failed to create device: {e}"))?;
    Ok((adapter, device, queue))
}

/// A window surface plus renderer.
pub struct Gpu {
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    pub r: Renderer,
}

impl Gpu {
    pub fn new(window: Arc<Window>) -> Result<Gpu, String> {
        pollster::block_on(async {
            let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
            let size = window.inner_size();
            let surface = instance.create_surface(window).map_err(|e| e.to_string())?;
            let (adapter, device, queue) = adapter_device(&instance, Some(&surface)).await?;
            let caps = surface.get_capabilities(&adapter);
            // Prefer a non-sRGB target: UI colors are authored in sRGB and blending there matches
            // what designers expect. If only sRGB is offered the shader linearizes instead.
            let format = caps.formats.iter().copied().find(|f| !f.is_srgb()).unwrap_or(caps.formats[0]);
            let mut config = surface
                .get_default_config(&adapter, size.width.max(1), size.height.max(1))
                .ok_or("surface not supported by adapter")?;
            config.format = format;
            config.present_mode = wgpu::PresentMode::AutoVsync;
            config.desired_maximum_frame_latency = 1;
            surface.configure(&device, &config);
            Ok(Gpu { surface, config, r: Renderer::new(device, queue, format, ATLAS_SIZE) })
        })
    }

    pub fn resize(&mut self, w: u32, h: u32) {
        self.config.width = w.max(1);
        self.config.height = h.max(1);
        self.surface.configure(&self.r.device, &self.config);
    }

    pub fn frame(&mut self, painter: &mut Painter, clear: Color) {
        let tex = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.r.device, &self.config);
                return;
            }
            _ => return,
        };
        let view = tex.texture.create_view(&wgpu::TextureViewDescriptor::default());
        self.r.render(&view, painter, (self.config.width, self.config.height), clear);
        self.r.queue.present(tex);
    }
}

/// Renders one frame offscreen and writes it as a PNG. Used by `--screenshot`.
pub fn render_png(
    painter: &mut Painter,
    phys: (u32, u32),
    clear: Color,
    path: &std::path::Path,
) -> Result<(), String> {
    pollster::block_on(async {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let (_, device, queue) = adapter_device(&instance, None).await?;
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let mut r = Renderer::new(device, queue, format, ATLAS_SIZE);
        let tex = r.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("screenshot"),
            size: wgpu::Extent3d { width: phys.0, height: phys.1, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
        painter.fonts.atlas.dirty = true;
        r.render(&view, painter, phys, clear);

        let row = (phys.0 * 4).div_ceil(256) * 256;
        let buf = r.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: (row * phys.1) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut enc = r.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        enc.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buf,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: Some(phys.1) },
            },
            wgpu::Extent3d { width: phys.0, height: phys.1, depth_or_array_layers: 1 },
        );
        r.queue.submit([enc.finish()]);
        let slice = buf.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        r.device
            .poll(wgpu::PollType::Wait { submission_index: None, timeout: None })
            .map_err(|e| e.to_string())?;
        let data = slice.get_mapped_range().map_err(|e| e.to_string())?;
        let mut rgba = Vec::with_capacity((phys.0 * phys.1 * 4) as usize);
        for y in 0..phys.1 {
            let start = (y * row) as usize;
            rgba.extend_from_slice(&data[start..start + (phys.0 * 4) as usize]);
        }
        std::fs::write(path, crate::png::encode_rgba(phys.0, phys.1, &rgba)).map_err(|e| e.to_string())
    })
}
