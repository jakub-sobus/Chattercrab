//! Draws one NV12 screen-share frame. The color conversion happens in the shader,
//! so the UI never expands the picture to RGBA.

use std::sync::Arc;

use chattercrab_media::Nv12Frame;
use iced::mouse;
use iced::widget::shader::{self, Primitive};
use iced::{Rectangle, wgpu};

/// Latest picture to draw, letterboxed inside the widget.
#[derive(Clone)]
pub struct ShareView {
    /// Decoded NV12 frame.
    pub frame: Nv12Frame,
}

#[derive(Debug)]
pub struct SharePrimitive {
    frame: Nv12Frame,
}

impl<Message> shader::Program<Message> for ShareView {
    type State = ();
    type Primitive = SharePrimitive;

    fn draw(
        &self,
        _state: &Self::State,
        _cursor: mouse::Cursor,
        _bounds: Rectangle,
    ) -> Self::Primitive {
        SharePrimitive {
            frame: self.frame.clone(),
        }
    }
}

impl Primitive for SharePrimitive {
    type Pipeline = SharePipeline;

    fn prepare(
        &self,
        pipeline: &mut Self::Pipeline,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        bounds: &Rectangle,
        _viewport: &iced::widget::shader::Viewport,
    ) {
        pipeline.prepare(device, queue, &self.frame, bounds);
    }

    fn draw(&self, pipeline: &Self::Pipeline, pass: &mut wgpu::RenderPass<'_>) -> bool {
        pipeline.draw(pass);
        true
    }
}

/// GPU state shared by every share frame.
pub struct SharePipeline {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    uniform: wgpu::Buffer,
    bind: Option<wgpu::BindGroup>,
    y: Option<Plane>,
    uv: Option<Plane>,
    y_pad: Vec<u8>,
    uv_pad: Vec<u8>,
    uploaded: usize,
}

struct Plane {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    width: u32,
    height: u32,
}

impl shader::Pipeline for SharePipeline {
    fn new(device: &wgpu::Device, _queue: &wgpu::Queue, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("share-nv12"),
            source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(include_str!(
                "share_view.wgsl"
            ))),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("share-nv12"),
            entries: &[
                texture_entry(0),
                texture_entry(1),
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
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
            label: Some("share-nv12"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("share-nv12"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                ..wgpu::PrimitiveState::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("share-nv12"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..wgpu::SamplerDescriptor::default()
        });
        let uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("share-fit"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self {
            pipeline,
            layout,
            sampler,
            uniform,
            bind: None,
            y: None,
            uv: None,
            y_pad: Vec::new(),
            uv_pad: Vec::new(),
            uploaded: 0,
        }
    }
}

impl SharePipeline {
    fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        frame: &Nv12Frame,
        bounds: &Rectangle,
    ) {
        let y_width = frame.width.max(2);
        let y_height = frame.height.max(2);
        let uv_width = y_width / 2;
        let uv_height = y_height / 2;
        let same_size = self
            .y
            .as_ref()
            .is_some_and(|plane| plane.width == y_width && plane.height == y_height);
        if !same_size {
            self.y = Some(plane(
                device,
                y_width,
                y_height,
                wgpu::TextureFormat::R8Unorm,
                "share-y",
            ));
            self.uv = Some(plane(
                device,
                uv_width.max(1),
                uv_height.max(1),
                wgpu::TextureFormat::Rg8Unorm,
                "share-uv",
            ));
            self.uploaded = 0;
            self.bind = None;
        }
        let generation = Arc::as_ptr(&frame.y) as *const () as usize;
        if generation != self.uploaded {
            if let Some(texture) = self.y.as_ref().map(|plane| plane.texture.clone()) {
                write_plane(
                    queue,
                    &texture,
                    &frame.y,
                    frame.width,
                    frame.height,
                    frame.y_stride,
                    1,
                    &mut self.y_pad,
                );
            }
            if let Some(texture) = self.uv.as_ref().map(|plane| plane.texture.clone()) {
                write_plane(
                    queue,
                    &texture,
                    &frame.uv,
                    uv_width.max(1),
                    uv_height.max(1),
                    frame.uv_stride,
                    2,
                    &mut self.uv_pad,
                );
            }
            self.uploaded = generation;
        }
        queue.write_buffer(
            &self.uniform,
            0,
            &fit_bytes(bounds, frame.width, frame.height),
        );
        if self.bind.is_none()
            && let (Some(y), Some(uv)) = (&self.y, &self.uv)
        {
            self.bind = Some(device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("share-nv12"),
                layout: &self.layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&y.view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&uv.view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: self.uniform.as_entire_binding(),
                    },
                ],
            }));
        }
    }

    fn draw(&self, pass: &mut wgpu::RenderPass<'_>) {
        let Some(bind) = &self.bind else {
            return;
        };
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, bind, &[]);
        pass.draw(0..4, 0..1);
    }
}

fn texture_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

fn plane(
    device: &wgpu::Device,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
    label: &str,
) -> Plane {
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
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    Plane {
        texture,
        view,
        width,
        height,
    }
}

/// `bytes_per_texel` is 1 for luma and 2 for interleaved chroma.
#[allow(clippy::too_many_arguments)]
fn write_plane(
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    data: &[u8],
    row_texels: u32,
    rows: u32,
    src_stride: u32,
    bytes_per_texel: u32,
    scratch: &mut Vec<u8>,
) {
    let row_bytes = row_texels.saturating_mul(bytes_per_texel).max(1);
    let rows = rows.max(1);
    let aligned = row_bytes.next_multiple_of(256);
    let packed = src_stride == aligned && data.len() >= aligned as usize * rows as usize;
    let bytes: &[u8] = if packed {
        data
    } else {
        let bytes_len = aligned as usize * rows as usize;
        scratch.resize(bytes_len, 0);
        let copy = row_bytes.min(src_stride) as usize;
        for row in 0..rows as usize {
            let start = row * src_stride as usize;
            let end = start + copy;
            if end > data.len() {
                break;
            }
            let dest = row * aligned as usize;
            scratch[dest..dest + copy].copy_from_slice(&data[start..end]);
        }
        scratch
    };
    queue.write_texture(
        texture.as_image_copy(),
        bytes,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(aligned),
            rows_per_image: Some(rows),
        },
        wgpu::Extent3d {
            width: row_texels.max(1),
            height: rows,
            depth_or_array_layers: 1,
        },
    );
}

fn fit_bytes(bounds: &Rectangle, frame_w: u32, frame_h: u32) -> [u8; 16] {
    let widget_w = bounds.width.max(1.0);
    let widget_h = bounds.height.max(1.0);
    let frame_aspect = frame_w.max(1) as f32 / frame_h.max(1) as f32;
    let widget_aspect = widget_w / widget_h;
    let (scale_x, scale_y) = if frame_aspect > widget_aspect {
        (1.0, widget_aspect / frame_aspect)
    } else {
        (frame_aspect / widget_aspect, 1.0)
    };
    let offset_x = (1.0 - scale_x) * 0.5;
    let offset_y = (1.0 - scale_y) * 0.5;
    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&offset_x.to_le_bytes());
    out[4..8].copy_from_slice(&offset_y.to_le_bytes());
    out[8..12].copy_from_slice(&scale_x.to_le_bytes());
    out[12..16].copy_from_slice(&scale_y.to_le_bytes());
    out
}
