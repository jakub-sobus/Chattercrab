//! Draws the remote screen share. The color conversion happens in the shader,
//! so the UI never expands the picture to RGBA.
//!
//! Pictures do not go through `update` and `view`. A subscription moves the
//! newest decoded picture into a [`LatestFrame`] slot without producing
//! messages, and the widget draws whatever the slot holds, asking for the next
//! redraw while it is on screen. A redraw does not rebuild the widget tree.

use std::sync::{Arc, Mutex, MutexGuard};

use chattercrab_media::Nv12Frame;
use iced::widget::shader::{self, Primitive};
use iced::{Rectangle, mouse, wgpu, window};

/// Newest picture of the current remote share. The room keeps it, so it
/// survives view rebuilds and layout changes (fullscreen).
#[derive(Debug, Clone, Default)]
pub struct LatestFrame(Arc<Mutex<Option<Nv12Frame>>>);

impl LatestFrame {
    /// Whether a picture arrived since the last [`Self::clear`].
    pub fn has_frame(&self) -> bool {
        self.lock().is_some()
    }

    /// Forgets the picture when the share ends or changes hands.
    pub fn clear(&self) {
        *self.lock() = None;
    }

    /// Replaces the picture. True for the first one since [`Self::clear`].
    pub fn put(&self, frame: Nv12Frame) -> bool {
        self.lock().replace(frame).is_none()
    }

    /// Tells slots apart (subscription identity).
    pub fn id(&self) -> usize {
        Arc::as_ptr(&self.0) as usize
    }

    fn get(&self) -> Option<Nv12Frame> {
        self.lock().clone()
    }

    fn lock(&self) -> MutexGuard<'_, Option<Nv12Frame>> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Newest picture of the share, letterboxed inside the widget.
pub struct ShareView {
    /// Picture to draw, kept by the room.
    pub latest: LatestFrame,
}

#[derive(Debug)]
pub struct SharePrimitive {
    frame: Option<Nv12Frame>,
}

impl<Message> shader::Program<Message> for ShareView {
    type State = ();
    type Primitive = SharePrimitive;

    /// Redraws on every display frame while the share is on screen, so a new
    /// picture in the slot is drawn without a message. `prepare` uploads only
    /// pictures it has not uploaded yet.
    fn update(
        &self,
        _state: &mut Self::State,
        event: &iced::Event,
        _bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Option<shader::Action<Message>> {
        match event {
            iced::Event::Window(window::Event::RedrawRequested(_)) => {
                Some(shader::Action::request_redraw())
            }
            _ => None,
        }
    }

    fn draw(
        &self,
        _state: &Self::State,
        _cursor: mouse::Cursor,
        _bounds: Rectangle,
    ) -> Self::Primitive {
        SharePrimitive {
            frame: self.latest.get(),
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
        if let Some(frame) = &self.frame {
            pipeline.prepare(device, queue, frame, bounds);
        }
    }

    fn draw(&self, pipeline: &Self::Pipeline, pass: &mut wgpu::RenderPass<'_>) -> bool {
        // The pipeline still holds the last share's textures until a picture arrives.
        if self.frame.is_some() {
            pipeline.draw(pass);
        }
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
    /// [`Nv12Frame::seq`] of the picture in the textures; 0 for none.
    uploaded: u64,
    /// A frame was skipped as unusable; logged once.
    rejected: bool,
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
            uploaded: 0,
            rejected: false,
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
        // The size comes from the remote encoder; wgpu panics on textures it cannot
        // create. Keep showing the previous picture instead.
        if !frame_is_drawable(frame, device.limits().max_texture_dimension_2d) {
            if !self.rejected {
                self.rejected = true;
                tracing::warn!(
                    width = frame.width,
                    height = frame.height,
                    y_len = frame.y.len(),
                    uv_len = frame.uv.len(),
                    "skipping screen-share frames that cannot be drawn"
                );
            }
            return;
        }
        let y_width = frame.width.max(2);
        let y_height = frame.height.max(2);
        let uv_width = y_width.div_ceil(2);
        let uv_height = y_height.div_ceil(2);
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
        if frame.seq != self.uploaded {
            if let Some(texture) = self.y.as_ref().map(|plane| plane.texture.clone()) {
                write_plane(
                    queue,
                    &texture,
                    &frame.y,
                    frame.width,
                    frame.height,
                    frame.y_stride,
                );
            }
            if let Some(texture) = self.uv.as_ref().map(|plane| plane.texture.clone()) {
                write_plane(
                    queue,
                    &texture,
                    &frame.uv,
                    uv_width,
                    uv_height,
                    frame.uv_stride,
                );
            }
            self.uploaded = frame.seq;
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

/// Whether the frame fits the GPU's texture limit and its planes hold every row that
/// [`write_plane`] reads.
fn frame_is_drawable(frame: &Nv12Frame, max_dimension: u32) -> bool {
    let (width, height) = (frame.width, frame.height);
    if width == 0 || height == 0 || width > max_dimension || height > max_dimension {
        return false;
    }
    // Same chroma size as `prepare` uploads.
    let uv_width = width.max(2).div_ceil(2);
    let uv_height = height.max(2).div_ceil(2);
    plane_holds(frame.y.len(), width, height, frame.y_stride)
        && plane_holds(frame.uv.len(), uv_width * 2, uv_height, frame.uv_stride)
}

/// `rows` rows of `row_bytes` bytes, `stride` bytes apart, fit in `len` bytes.
fn plane_holds(len: usize, row_bytes: u32, rows: u32, stride: u32) -> bool {
    let needed = u64::from(stride) * u64::from(rows - 1) + u64::from(row_bytes);
    stride >= row_bytes && len as u64 >= needed
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

/// Uploads `rows` rows of `row_texels` texels, `stride` bytes apart. Unlike
/// buffer-to-texture copies, `Queue::write_texture` takes any `bytes_per_row`
/// (no 256-byte alignment), so the planes go up as they are.
fn write_plane(
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    data: &[u8],
    row_texels: u32,
    rows: u32,
    stride: u32,
) {
    queue.write_texture(
        texture.as_image_copy(),
        data,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(stride),
            rows_per_image: Some(rows),
        },
        wgpu::Extent3d {
            width: row_texels,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(width: u32, height: u32, y_stride: u32, y_len: usize, uv_len: usize) -> Nv12Frame {
        Nv12Frame {
            width,
            height,
            y_stride,
            uv_stride: y_stride,
            y: vec![0; y_len].into(),
            uv: vec![0; uv_len].into(),
            seq: 1,
        }
    }

    #[test]
    fn put_reports_the_first_picture_after_a_clear() {
        let latest = LatestFrame::default();
        assert!(!latest.has_frame());
        assert!(
            latest.put(frame(4, 4, 4, 16, 8)),
            "first picture of the share"
        );
        let mut next = frame(4, 4, 4, 16, 8);
        next.seq = 2;
        assert!(!latest.put(next), "not the first picture any more");
        assert_eq!(latest.get().map(|f| f.seq), Some(2));
        assert_eq!(latest.clone().id(), latest.id());
        assert_ne!(LatestFrame::default().id(), latest.id());

        latest.clear();
        assert!(!latest.has_frame());
        assert!(latest.put(frame(4, 4, 4, 16, 8)));
    }

    #[test]
    fn drawable_frames_fit_the_limit_and_their_planes() {
        assert!(frame_is_drawable(&frame(4, 4, 4, 16, 8), 8192));
        // Padded rows; the last row needs no padding.
        assert!(frame_is_drawable(&frame(4, 4, 8, 28, 12), 8192));
        assert!(frame_is_drawable(&frame(8192, 2, 8192, 16384, 8192), 8192));
        assert!(!frame_is_drawable(&frame(8193, 2, 8193, 16386, 8194), 8192));
        assert!(!frame_is_drawable(&frame(0, 4, 4, 16, 8), 8192));
        // Short planes and strides narrower than a row.
        assert!(!frame_is_drawable(&frame(4, 4, 4, 15, 8), 8192));
        assert!(!frame_is_drawable(&frame(4, 4, 4, 16, 7), 8192));
        assert!(!frame_is_drawable(&frame(4, 4, 3, 64, 64), 8192));
    }
}
