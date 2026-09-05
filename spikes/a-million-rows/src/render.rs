// SPDX-License-Identifier: Apache-2.0
//! The `sk-gpu` half of ADR 0003 at spike fidelity: one pipeline, one draw call,
//! instanced quads and two atlas textures.
//!
//! GPU time is measured with timestamp queries because
//! docs/09-performance-budgets.md says so, and the readback is deliberately two
//! frames behind: a `map_async` waited on inside the frame it was issued would
//! measure the harness stalling on itself.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use crate::atlas::{COLOR_SIDE, MASK_SIDE};

pub const KIND_SOLID: u32 = 0;
pub const KIND_MASK: u32 = 1;
pub const KIND_COLOR: u32 = 2;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Quad {
    /// Destination in device pixels: x, y, width, height.
    pub rect: [f32; 4],
    /// Source in atlas pixels: x, y, width, height.
    pub uv: [f32; 4],
    pub color: u32,
    pub kind: u32,
}

/// How many frames of timestamp queries are in flight. Measured at three, a
/// headless loop outran the readback on 23% of frames; eight costs 8 KB and
/// leaves the distribution intact.
const RING: usize = 8;

struct Slot {
    resolve: wgpu::Buffer,
    readback: wgpu::Buffer,
    state: Arc<AtomicU8>,
}

const IDLE: u8 = 0;
const PENDING: u8 = 1;
const READY: u8 = 2;

struct Timing {
    queries: wgpu::QuerySet,
    slots: Vec<Slot>,
    period_ns: f32,
    pub dropped: u64,
}

pub struct Gpu {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub info: wgpu::AdapterInfo,
    pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    uniforms: wgpu::Buffer,
    instances: wgpu::Buffer,
    capacity: usize,
    mask: wgpu::Texture,
    color: wgpu::Texture,
    timing: Option<Timing>,
    frame: u64,
    /// Frames where a timestamp slot was still in flight, so that frame has no
    /// GPU sample. Reported rather than silently thinning the distribution.
    pub timing_gaps: u64,
}

/// Selects an adapter by a substring of its name — `radv`, `nvidia`, `llvmpipe`.
/// Enumerating rather than using `request_adapter` because the whole point is to
/// measure a *named* device, not whichever one wgpu prefers.
pub fn pick_adapter(instance: &wgpu::Instance, wanted: Option<&str>) -> Result<wgpu::Adapter, String> {
    let adapters = instance.enumerate_adapters(wgpu::Backends::VULKAN);
    if adapters.is_empty() {
        return Err("no Vulkan adapter — is the loader installed?".into());
    }
    let describe = |a: &wgpu::Adapter| {
        let info = a.get_info();
        format!("{} ({})", info.name, info.driver)
    };
    match wanted {
        None => Ok(adapters
            .into_iter()
            .next()
            .expect("checked non-empty above")),
        Some(needle) => {
            let needle = needle.to_ascii_lowercase();
            adapters
                .iter()
                .find(|a| {
                    let info = a.get_info();
                    info.name.to_ascii_lowercase().contains(&needle)
                        || info.driver.to_ascii_lowercase().contains(&needle)
                })
                .cloned()
                .ok_or_else(|| {
                    let names: Vec<_> = adapters.iter().map(describe).collect();
                    format!("no adapter matching `{needle}` — have: {}", names.join(", "))
                })
        }
    }
}

impl Gpu {
    pub fn new(adapter: &wgpu::Adapter, format: wgpu::TextureFormat) -> Result<Self, String> {
        let wants_timing = adapter.features().contains(wgpu::Features::TIMESTAMP_QUERY);
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("spike-a"),
            required_features: if wants_timing {
                wgpu::Features::TIMESTAMP_QUERY
            } else {
                wgpu::Features::empty()
            },
            required_limits: adapter.limits(),
            ..Default::default()
        }))
        .map_err(|e| format!("request_device: {e}"))?;

        let mask = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("glyph mask atlas"),
            size: wgpu::Extent3d { width: MASK_SIDE, height: MASK_SIDE, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let color = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("colour glyph atlas"),
            size: wgpu::Extent3d { width: COLOR_SIDE, height: COLOR_SIDE, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });

        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("uniforms"),
            size: 32,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        // Nearest, deliberately. Every glyph is drawn 1:1 from a raster made at
        // its device size, so a filtered sampler could only hide a bug.
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("atlas"),
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("atlas"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                texture_entry(1),
                texture_entry(2),
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::NonFiltering),
                    count: None,
                },
            ],
        });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("atlas"),
            layout: &layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: uniforms.as_entire_binding() },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(
                        &mask.create_view(&wgpu::TextureViewDescriptor::default()),
                    ),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(
                        &color.create_view(&wgpu::TextureViewDescriptor::default()),
                    ),
                },
                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::Sampler(&sampler) },
            ],
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("quads"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("quads"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });

        let attributes = [
            wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 0, shader_location: 0 },
            wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 16, shader_location: 1 },
            wgpu::VertexAttribute { format: wgpu::VertexFormat::Uint32, offset: 32, shader_location: 2 },
            wgpu::VertexAttribute { format: wgpu::VertexFormat::Uint32, offset: 36, shader_location: 3 },
        ];

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("quads"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<Quad>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &attributes,
                }],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let capacity = 1 << 16;
        let instances = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("quads"),
            size: (capacity * std::mem::size_of::<Quad>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let timing = wants_timing.then(|| Timing::new(&device, &queue));

        Ok(Self {
            device,
            queue,
            info: adapter.get_info(),
            pipeline,
            bind_group,
            uniforms,
            instances,
            capacity,
            mask,
            color,
            timing,
            frame: 0,
            timing_gaps: 0,
        })
    }

    pub fn has_gpu_timing(&self) -> bool {
        self.timing.is_some()
    }

    pub fn mask_texture(&self) -> &wgpu::Texture {
        &self.mask
    }

    pub fn color_texture(&self) -> &wgpu::Texture {
        &self.color
    }

    /// Draws one frame. Returns any GPU durations that finished resolving, in
    /// milliseconds — usually one, from two frames ago.
    pub fn draw(
        &mut self,
        view: &wgpu::TextureView,
        viewport: (u32, u32),
        quads: &[Quad],
        background: wgpu::Color,
    ) -> Vec<f32> {
        if quads.len() > self.capacity {
            self.capacity = quads.len().next_power_of_two();
            self.instances = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("quads"),
                size: (self.capacity * std::mem::size_of::<Quad>()) as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }

        self.queue.write_buffer(
            &self.uniforms,
            0,
            bytemuck::cast_slice(&[
                viewport.0 as f32,
                viewport.1 as f32,
                MASK_SIDE as f32,
                MASK_SIDE as f32,
                COLOR_SIDE as f32,
                COLOR_SIDE as f32,
                0.0,
                0.0,
            ]),
        );
        self.queue.write_buffer(&self.instances, 0, bytemuck::cast_slice(quads));

        let slot = (self.frame as usize) % RING;
        let timing_this_frame = match &self.timing {
            Some(timing) => timing.slots[slot].state.load(Ordering::Acquire) == IDLE,
            None => false,
        };
        if self.timing.is_some() && !timing_this_frame {
            self.timing_gaps += 1;
        }

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
        {
            let timestamp_writes = if timing_this_frame {
                self.timing.as_ref().map(|t| wgpu::RenderPassTimestampWrites {
                    query_set: &t.queries,
                    beginning_of_pass_write_index: Some((slot * 2) as u32),
                    end_of_pass_write_index: Some((slot * 2 + 1) as u32),
                })
            } else {
                None
            };

            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("list"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(background),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.set_vertex_buffer(0, self.instances.slice(..));
            pass.draw(0..6, 0..quads.len() as u32);
        }

        if timing_this_frame {
            if let Some(timing) = &self.timing {
                let s = &timing.slots[slot];
                encoder.resolve_query_set(
                    &timing.queries,
                    (slot * 2) as u32..(slot * 2 + 2) as u32,
                    &s.resolve,
                    0,
                );
                encoder.copy_buffer_to_buffer(&s.resolve, 0, &s.readback, 0, 16);
            }
        }

        self.queue.submit([encoder.finish()]);

        if timing_this_frame {
            if let Some(timing) = &self.timing {
                let s = &timing.slots[slot];
                s.state.store(PENDING, Ordering::Release);
                let flag = s.state.clone();
                s.readback.slice(..).map_async(wgpu::MapMode::Read, move |result| {
                    flag.store(if result.is_ok() { READY } else { IDLE }, Ordering::Release);
                });
            }
        }

        self.frame += 1;
        // Drives map callbacks without blocking. A frame that finds nothing
        // ready simply reports no GPU sample.
        let _ = self.device.poll(wgpu::PollType::Poll);
        self.collect_gpu_samples()
    }

    fn collect_gpu_samples(&mut self) -> Vec<f32> {
        let Some(timing) = &mut self.timing else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for slot in &timing.slots {
            if slot.state.load(Ordering::Acquire) != READY {
                continue;
            }
            {
                let view = slot.readback.slice(..).get_mapped_range();
                let stamps: &[u64] = bytemuck::cast_slice(&view);
                // A GPU whose counter did not advance across the pass reports a
                // zero delta rather than a negative one; either way it is a
                // sample we cannot trust, so it is dropped and counted.
                if stamps[1] > stamps[0] {
                    let ns = (stamps[1] - stamps[0]) as f32 * timing.period_ns;
                    out.push(ns / 1.0e6);
                } else {
                    timing.dropped += 1;
                }
            }
            slot.readback.unmap();
            slot.state.store(IDLE, Ordering::Release);
        }
        out
    }

    pub fn dropped_gpu_samples(&self) -> u64 {
        self.timing.as_ref().map_or(0, |t| t.dropped)
    }
}

impl Timing {
    fn new(device: &wgpu::Device, queue: &wgpu::Queue) -> Self {
        let queries = device.create_query_set(&wgpu::QuerySetDescriptor {
            label: Some("frame timestamps"),
            ty: wgpu::QueryType::Timestamp,
            count: (RING * 2) as u32,
        });
        let slots = (0..RING)
            .map(|_| Slot {
                // QUERY_RESOLVE_BUFFER_ALIGNMENT is 256, so each slot gets its
                // own buffer rather than an offset into a shared one.
                resolve: device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("query resolve"),
                    size: 256,
                    usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
                    mapped_at_creation: false,
                }),
                readback: device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("query readback"),
                    size: 16,
                    usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                }),
                state: Arc::new(AtomicU8::new(IDLE)),
            })
            .collect();
        Self { queries, slots, period_ns: queue.get_timestamp_period(), dropped: 0 }
    }
}

fn texture_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: false },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}
