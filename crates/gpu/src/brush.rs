//! Rasterizes each brush stroke with one instanced draw, then resolves its offscreen
//! pixels into the existing stage compositor's paint buffer.

use anicraft_doc::bitmap_brush::StampBlend;
use anicraft_render::brush::{BrushLayer, StampRenderer, TipAtlas};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, Weak};
use wgpu::util::DeviceExt;

const COLOR: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;
const DEPTH: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

pub(crate) struct Pipelines {
    accumulate: wgpu::RenderPipeline,
    strongest: wgpu::RenderPipeline,
    resolve: wgpu::ComputePipeline,
    sampler: wgpu::Sampler,
    atlases: Mutex<VecDeque<(Weak<TipAtlas>, wgpu::TextureView)>>,
}

impl Pipelines {
    pub(crate) fn new(device: &wgpu::Device) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("bitmap brush"),
            source: wgpu::ShaderSource::Wgsl(include_str!("brush.wgsl").into()),
        });
        let draw = |max: bool| {
            const ATTRS: [wgpu::VertexAttribute; 3] = wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4, 2 => Float32x4];
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("bitmap brush instances"),
                layout: None,
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("stamp_vertex"),
                    compilation_options: Default::default(),
                    buffers: &[Some(wgpu::VertexBufferLayout { attributes: &ATTRS, array_stride: 48, step_mode: wgpu::VertexStepMode::Instance })],
                },
                primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleStrip, ..Default::default() },
                depth_stencil: max.then_some(wgpu::DepthStencilState {
                    format: DEPTH,
                    depth_write_enabled: Some(true),
                    depth_compare: Some(wgpu::CompareFunction::GreaterEqual),
                    stencil: Default::default(),
                    bias: Default::default(),
                }),
                multisample: Default::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some(if max { "strongest" } else { "accumulate" }),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: COLOR,
                        blend: (!max).then_some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let resolve = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("resolve bitmap brush"),
            layout: None,
            module: &module,
            entry_point: Some("resolve"),
            compilation_options: Default::default(),
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("bitmap brush sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        });
        Self { accumulate: draw(false), strongest: draw(true), resolve, sampler, atlases: Mutex::new(VecDeque::new()) }
    }

    fn texture(&self, device: &wgpu::Device, queue: &wgpu::Queue, atlas: &Arc<TipAtlas>) -> wgpu::TextureView {
        let key = Arc::downgrade(atlas);
        let mut cache = self.atlases.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(i) = cache.iter().position(|(a, _)| a.ptr_eq(&key))
            && let Some(entry) = cache.remove(i)
        {
            let view = entry.1.clone();
            cache.push_front(entry);
            return view;
        }
        let tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("bitmap brush tip array"),
            size: wgpu::Extent3d { width: atlas.width, height: atlas.height, depth_or_array_layers: atlas.tips.len() as u32 },
            mip_level_count: atlas.level_count(),
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        for (tip, levels) in atlas.tips.iter().enumerate() {
            for (n, mip) in levels.iter().enumerate() {
                let (width, height) = atlas.level_size(n as u32);
                queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: &tex,
                        mip_level: n as u32,
                        origin: wgpu::Origin3d { x: 0, y: 0, z: tip as u32 },
                        aspect: wgpu::TextureAspect::All,
                    },
                    mip,
                    wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(width), rows_per_image: Some(height) },
                    wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
                );
            }
        }
        let view = tex.create_view(&wgpu::TextureViewDescriptor { dimension: Some(wgpu::TextureViewDimension::D2Array), ..Default::default() });
        cache.truncate(7);
        cache.push_front((key, view.clone()));
        view
    }
}

pub(crate) struct Recorder<'a> {
    pub device: &'a wgpu::Device,
    pub queue: &'a wgpu::Queue,
    pub pipelines: &'a Pipelines,
    pub encoder: &'a mut wgpu::CommandEncoder,
}

pub(crate) struct Target<'a> {
    pub buffer: &'a wgpu::Buffer,
    pub at: u32,
}

impl StampRenderer<Target<'_>> for Recorder<'_> {
    type Error = std::convert::Infallible;

    fn draw(&mut self, stroke: &BrushLayer, target: &mut Target<'_>) -> Result<(), Self::Error> {
        let (w, h) = (stroke.bounds.width() as u32, stroke.bounds.height() as u32);
        let (sx, sy) = (2.0 / w as f32, -2.0 / h as f32);
        let instances: Vec<[[f32; 4]; 3]> = stroke
            .instances
            .iter()
            .map(|s| {
                let [a, b, c, d, x, y] = s.transform.m;
                [
                    [a * sx, b * sy, c * sx, d * sy],
                    [(x - stroke.bounds.x0 as f32) * sx - 1.0, (y - stroke.bounds.y0 as f32) * sy + 1.0, f32::from(s.tip), s.lod],
                    s.color,
                ]
            })
            .collect();
        let vertices = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("bitmap brush instances"),
            contents: bytemuck::cast_slice(&instances),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let maximum = stroke.blend == StampBlend::MaxPerStroke;
        let pipe = if maximum { &self.pipelines.strongest } else { &self.pipelines.accumulate };
        let atlas = self.pipelines.texture(self.device, self.queue, &stroke.atlas);
        let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("bitmap brush tip bindings"),
            layout: &pipe.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&atlas) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&self.pipelines.sampler) },
            ],
        });
        let texture = |format, usage| {
            self.device
                .create_texture(&wgpu::TextureDescriptor {
                    label: Some("bitmap brush scratch surface"),
                    size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT | usage,
                    view_formats: &[],
                })
                .create_view(&Default::default())
        };
        let color = texture(COLOR, wgpu::TextureUsages::TEXTURE_BINDING);
        let depth = maximum.then(|| texture(DEPTH, wgpu::TextureUsages::empty()));
        {
            let mut pass = self.encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("draw bitmap brush"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &color,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: depth.as_ref().map(|view| wgpu::RenderPassDepthStencilAttachment {
                    view,
                    depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(0.0), store: wgpu::StoreOp::Discard }),
                    stencil_ops: None,
                }),
                ..Default::default()
            });
            pass.set_pipeline(pipe);
            pass.set_bind_group(0, &bind, &[]);
            pass.set_vertex_buffer(0, vertices.slice(..));
            pass.draw(0..4, 0..instances.len() as u32);
        }
        let at = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("bitmap brush destination"),
            contents: bytemuck::cast_slice(&[target.at, 0, 0, 0]),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let pipe = &self.pipelines.resolve;
        let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("resolve bitmap brush bindings"),
            layout: &pipe.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&color) },
                wgpu::BindGroupEntry { binding: 1, resource: target.buffer.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 2, resource: at.as_entire_binding() },
            ],
        });
        let mut pass = self.encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("resolve bitmap brush"), timestamp_writes: None });
        pass.set_pipeline(pipe);
        pass.set_bind_group(0, &bind, &[]);
        pass.dispatch_workgroups(w.div_ceil(8), h.div_ceil(8), 1);
        Ok(())
    }
}
