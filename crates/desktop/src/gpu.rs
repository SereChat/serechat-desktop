//! wgpu renderer: one pipeline, one instance buffer, one draw call per frame.

use std::fmt;
use std::sync::Arc;

use winit::window::Window;

use crate::text::{ATLAS_SIZE, Upload};

/// One UI primitive as consumed by `shader.wgsl`. See the shader header for
/// the meaning of each field.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Instance {
    /// Position and size in logical pixels.
    pub rect: [f32; 4],
    /// Clip rectangle `[x0, y0, x1, y1]` in logical pixels.
    pub clip: [f32; 4],
    /// Fill (or glyph) colour, straight-alpha sRGB.
    pub color: [f32; 4],
    /// Border colour.
    pub color2: [f32; 4],
    /// Glyph atlas rectangle in texels.
    pub uv: [f32; 4],
    /// Corner radius, border width, blur radius, primitive kind.
    pub params: [f32; 4],
}

// SAFETY: `#[repr(C)]`, only `f32` fields, no padding, and all-zero is valid.
unsafe impl bytemuck::Zeroable for Instance {}
// SAFETY: see above; every bit pattern is a valid `f32`.
unsafe impl bytemuck::Pod for Instance {}

/// Shader uniforms, laid out to match `Globals` in `shader.wgsl`.
#[repr(C)]
#[derive(Clone, Copy)]
struct Globals {
    viewport: [f32; 2],
    scale: f32,
    linear_output: f32,
    atlas_size: [f32; 2],
    pad: [f32; 2],
}

// SAFETY: `#[repr(C)]`, only `f32` fields, no padding, and all-zero is valid.
unsafe impl bytemuck::Zeroable for Globals {}
// SAFETY: see above.
unsafe impl bytemuck::Pod for Globals {}

/// Failure to bring up the GPU.
#[derive(Debug)]
pub enum GpuError {
    /// The window could not be turned into a surface.
    Surface(wgpu::CreateSurfaceError),
    /// No adapter can present to the window.
    Adapter(wgpu::RequestAdapterError),
    /// The adapter refused to create a device.
    Device(wgpu::RequestDeviceError),
    /// The surface reports no usable texture format.
    NoSurfaceFormat,
}

impl fmt::Display for GpuError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Surface(e) => write!(f, "cannot create a rendering surface: {e}"),
            Self::Adapter(e) => write!(f, "no compatible GPU adapter: {e}"),
            Self::Device(e) => write!(f, "cannot open the GPU device: {e}"),
            Self::NoSurfaceFormat => f.write_str("the window surface supports no texture formats"),
        }
    }
}

impl std::error::Error for GpuError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Surface(e) => Some(e),
            Self::Adapter(e) => Some(e),
            Self::Device(e) => Some(e),
            Self::NoSurfaceFormat => None,
        }
    }
}

/// Owns every GPU object needed to draw the UI into the window.
pub struct Renderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    globals: wgpu::Buffer,
    atlas: wgpu::Texture,
    instances: wgpu::Buffer,
    linear_output: bool,
}

impl Renderer {
    /// Initialises wgpu for `window`, preferring the low-power GPU: a UI does
    /// not need the discrete card and waking it costs battery and startup time.
    ///
    /// # Errors
    /// See [`GpuError`].
    pub async fn new(window: Arc<Window>, display: winit::event_loop::OwnedDisplayHandle) -> Result<Self, GpuError> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_with_display_handle_from_env(Box::new(display)));
        let size = window.inner_size();
        let surface = instance.create_surface(window).map_err(GpuError::Surface)?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::LowPower,
                compatible_surface: Some(&surface),
                ..Default::default()
            })
            .await
            .map_err(GpuError::Adapter)?;
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("serechat"),
                required_limits: wgpu::Limits::downlevel_webgl2_defaults().using_resolution(adapter.limits()),
                memory_hints: wgpu::MemoryHints::MemoryUsage,
                ..Default::default()
            })
            .await
            .map_err(GpuError::Device)?;

        // Blend in sRGB space (like browsers do) by picking a non-sRGB
        // format; fall back to converting in the shader if there is none.
        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .or_else(|| caps.formats.first().copied())
            .ok_or(GpuError::NoSurfaceFormat)?;
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            color_space: wgpu::SurfaceColorSpace::Auto,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::AutoVsync,
            desired_maximum_frame_latency: 2,
            alpha_mode: caps.alpha_modes.first().copied().unwrap_or(wgpu::CompositeAlphaMode::Auto),
            view_formats: Vec::new(),
        };
        surface.configure(&device, &config);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ui"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });

        let globals = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("globals"),
            size: size_of::<Globals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let atlas = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("glyph atlas"),
            size: wgpu::Extent3d { width: ATLAS_SIZE, height: ATLAS_SIZE, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let atlas_view = atlas.create_view(&wgpu::TextureViewDescriptor::default());
        // Glyph quads land exactly on texels, so filtering never blends
        // neighbours; linear only smooths float rounding error.
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("atlas"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
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
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("ui"),
            layout: &layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: globals.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&atlas_view) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&sampler) },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("ui"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let attributes = wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4, 2 => Float32x4, 3 => Float32x4, 4 => Float32x4, 5 => Float32x4];
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("ui"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: size_of::<Instance>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &attributes,
                })],
            },
            primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleStrip, ..Default::default() },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });

        let instances = Self::instance_buffer(&device, 1024);
        Ok(Self {
            surface,
            device,
            queue,
            config,
            pipeline,
            bind_group,
            globals,
            atlas,
            instances,
            linear_output: format.is_srgb(),
        })
    }

    fn instance_buffer(device: &wgpu::Device, capacity: usize) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("instances"),
            size: (capacity * size_of::<Instance>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    /// Resizes the swapchain. Zero-sized (minimised) windows are ignored.
    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
    }

    /// Uploads pending glyphs and draws `instances` over `clear`.
    ///
    /// Returns `false` when no frame could be acquired (window occluded,
    /// timeout); the caller should simply try again on the next redraw.
    pub fn render(&mut self, instances: &[Instance], uploads: &mut Vec<Upload>, scale: f32, clear: [f32; 4]) -> bool {
        for upload in uploads.drain(..) {
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &self.atlas,
                    mip_level: 0,
                    origin: wgpu::Origin3d { x: upload.x, y: upload.y, z: 0 },
                    aspect: wgpu::TextureAspect::All,
                },
                &upload.data,
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(upload.w), rows_per_image: None },
                wgpu::Extent3d { width: upload.w, height: upload.h, depth_or_array_layers: 1 },
            );
        }

        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame) => frame,
            wgpu::CurrentSurfaceTexture::Suboptimal(frame) => {
                // Draw this one, reconfigure for the next.
                self.surface.configure(&self.device, &self.config);
                frame
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                return false;
            }
            wgpu::CurrentSurfaceTexture::Timeout
            | wgpu::CurrentSurfaceTexture::Occluded
            | wgpu::CurrentSurfaceTexture::Validation => return false,
        };

        let globals = Globals {
            viewport: [self.config.width as f32, self.config.height as f32],
            scale,
            linear_output: if self.linear_output { 1.0 } else { 0.0 },
            atlas_size: [ATLAS_SIZE as f32; 2],
            pad: [0.0; 2],
        };
        self.queue.write_buffer(&self.globals, 0, bytemuck::bytes_of(&globals));

        let bytes = bytemuck::cast_slice::<Instance, u8>(instances);
        if bytes.len() as u64 > self.instances.size() {
            self.instances = Self::instance_buffer(&self.device, instances.len().next_power_of_two());
        }
        if !bytes.is_empty() {
            self.queue.write_buffer(&self.instances, 0, bytes);
        }

        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("ui") });
        {
            let [r, g, b, a] = clear.map(f64::from);
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("ui"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(if self.linear_output {
                            wgpu::Color { r: srgb_to_linear(r), g: srgb_to_linear(g), b: srgb_to_linear(b), a }
                        } else {
                            wgpu::Color { r, g, b, a }
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            if !instances.is_empty() {
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &self.bind_group, &[]);
                pass.set_vertex_buffer(0, self.instances.slice(..bytes.len() as u64));
                pass.draw(0..4, 0..instances.len() as u32);
            }
        }
        self.queue.submit([encoder.finish()]);
        self.queue.present(frame);
        true
    }
}

fn srgb_to_linear(c: f64) -> f64 {
    if c <= 0.040_45 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}
