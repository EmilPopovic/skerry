// SPDX-License-Identifier: Apache-2.0
//! The device, the target, and the one draw call per clip region.

use sk_model::{Problem, ProblemKind};
use sk_plat::RawHandles;
use sk_text::{Fonts, GlyphKey};

use crate::atlas::{Atlas, COLOR_SIDE, MASK_SIDE, Placed};
use crate::draw::{DrawList, Item, KIND_COLOR, KIND_MASK, Quad};
use crate::images::{ICON_SIDE, ImageKey, Images, SIDE as IMAGE_SIDE};

/// What one frame cost.
///
/// The decomposition is deliberate. `acquire_us` is the wait for the
/// compositor's next presentation slot and is **not** our cost — with
/// `PresentMode::Fifo` it is where the frame pacing happens, and charging it to
/// the frame budget would report a 60 Hz screen as a 16 ms regression. `cpu_us`
/// is everything after that: resolving glyphs, packing instances, encoding and
/// submitting. That is the number the 8 ms row in
/// [docs/09-performance-budgets.md](../../../docs/09-performance-budgets.md)
/// is about.
#[derive(Clone, Copy, Debug, Default)]
pub struct FrameStats {
    /// Microseconds spent waiting for a presentation slot. Frame pacing, not
    /// work.
    pub acquire_us: u64,
    /// Microseconds of our own work: glyph resolution, instance packing,
    /// encoding, submission.
    pub cpu_us: u64,
    /// Quads submitted.
    pub quads: usize,
    /// Draw calls, which is the number of distinct clip regions used.
    pub draws: usize,
    /// Glyphs rasterized this frame, which is the cost a cold cache pays.
    pub rasterized: usize,
    /// Glyphs the atlas could not place. Non-zero means text is missing.
    pub dropped: usize,
}

/// Where a frame goes.
enum Target {
    /// A window's surface, presented and paced by the compositor.
    Window { surface: wgpu::Surface<'static>, config: wgpu::SurfaceConfiguration },
    /// A texture we own, readable back as pixels.
    ///
    /// This is what makes the renderer testable and inspectable without a
    /// display server, which is the whole point of the golden-frame strategy.
    Offscreen { texture: wgpu::Texture, size: (u32, u32) },
}

/// The renderer.
///
/// When built from a window it holds that window's GPU surface, and must be
/// dropped **before** the window: the surface borrows the compositor objects the
/// window owns.
pub struct Renderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    target: Target,
    pipeline: wgpu::RenderPipeline,
    /// Kept so the bind group can be rebuilt when the thumbnail atlas grows from
    /// its placeholder — the layout is unchanged by that, only the view.
    layout: wgpu::BindGroupLayout,
    bind_group: wgpu::BindGroup,
    uniforms: wgpu::Buffer,
    instances: wgpu::Buffer,
    capacity: usize,
    mask: wgpu::Texture,
    color: wgpu::Texture,
    /// The thumbnail atlas's texture — ADR 0018's grid of equal slots.
    images: wgpu::Texture,
    /// The icon atlas's texture — ADR 0042's four bands. Its own, because an icon
    /// is keyed and sized differently from a thumbnail.
    icons: wgpu::Texture,
    /// The pages held, each its own texture, keyed by what it is of — [ADR
    /// 0020](../../../docs/adr/0020-a-preview-is-a-few-textures-bounded-by-bytes.md).
    ///
    /// **One is bound at a time**, because a frame draws one page: the bind group
    /// is repointed when the drawn key changes, which is on a turn and not per
    /// frame. `placeholder` is what a frame with no preview binds, since `wgpu`
    /// has no null binding.
    pages: std::collections::HashMap<ImageKey, wgpu::Texture>,
    placeholder: wgpu::Texture,
    /// What is held there and what is waiting for the queue.
    looking: crate::images::Preview,
    atlas: Atlas,
    /// What is in that grid, and what is queued for it.
    pictures: Images,
    /// What is in the four bands, and what is queued for them.
    marks: crate::images::Icons,
    /// Resolved instances for the frame being submitted, kept between frames so
    /// a frame is not an allocation.
    quads: Vec<Quad>,
    adapter: String,
    backend: String,
}

/// The offscreen format.
///
/// `Rgba8Unorm` rather than whatever the surface prefers, so a captured frame
/// needs no channel swizzle on the way to a PNG — a swizzle is a place for a
/// golden frame to differ from what a screen shows for a reason that is not the
/// renderer's.
const OFFSCREEN_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

impl Renderer {
    /// Creates a device and a surface for `handles` at `physical` device pixels.
    ///
    /// Tries Vulkan, then GL. Each attempt is a whole instance: a surface belongs
    /// to the instance that made it, so a fallback cannot reuse the first one's.
    ///
    /// # Safety
    ///
    /// `handles` must name a live compositor display and surface that outlive
    /// the returned renderer.
    pub unsafe fn new(
        handles: RawHandles,
        physical: (u32, u32),
        render_device: Option<(u32, u32)>,
    ) -> Result<Self, Problem> {
        let target = raw_pair(handles)?;

        let mut refused = None;
        for backends in BACKENDS {
            if backends == wgpu::Backends::VULKAN && sk_util::disabled("vulkan") {
                continue;
            }
            // SAFETY: the caller's contract, restated: the handles name live
            // objects owned by the `sk_plat::Window` that produced them, and the
            // window outlives this renderer.
            match unsafe { Self::windowed(backends, target, physical, render_device) } {
                Ok(renderer) => return Ok(renderer),
                Err(problem) => refused = Some(problem),
            }
        }
        Err(refused.unwrap_or_else(|| Problem::plain(ProblemKind::NoGpu, "no Vulkan and no GL")))
    }

    /// # Safety
    ///
    /// As [`Renderer::new`].
    unsafe fn windowed(
        backends: wgpu::Backends,
        target: (wgpu::rwh::RawDisplayHandle, wgpu::rwh::RawWindowHandle),
        physical: (u32, u32),
        render_device: Option<(u32, u32)>,
    ) -> Result<Self, Problem> {
        let instance = instance(backends);
        let (raw_display, raw_window) = target;

        // SAFETY: the caller's contract.
        let surface = unsafe {
            instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                raw_display_handle: raw_display,
                raw_window_handle: raw_window,
            })
        }
        .map_err(|error| Problem::plain(ProblemKind::NoGpu, format!("create_surface: {error}")))?;

        let (adapter, device, queue) = open_device(&instance, Some(&surface), render_device)?;
        let capabilities = surface.get_capabilities(&adapter);
        let format = pick_format(&capabilities).ok_or_else(|| {
            Problem::plain(ProblemKind::NoGpu, "the surface offers no 8-bit format")
        })?;

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: physical.0.max(1),
            height: physical.1.max(1),
            // Fifo, always. It is the only mode every driver supports, and it is
            // the one that pairs with "no redraw without input": a frame is
            // presented when the compositor is ready and the loop then goes back
            // to sleep on the display socket.
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: wgpu::CompositeAlphaMode::Opaque,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        Ok(Self::assemble(&adapter, device, queue, Target::Window { surface, config }, format))
    }

    /// Creates a device with no window, rendering into a texture.
    pub fn offscreen(physical: (u32, u32)) -> Result<Self, Problem> {
        let mut refused = None;
        for backends in BACKENDS {
            if backends == wgpu::Backends::VULKAN && sk_util::disabled("vulkan") {
                continue;
            }
            let instance = instance(backends);
            match open_device(&instance, None, None) {
                Ok((adapter, device, queue)) => {
                    let size = (physical.0.max(1), physical.1.max(1));
                    let texture = offscreen_texture(&device, size);
                    return Ok(Self::assemble(
                        &adapter,
                        device,
                        queue,
                        Target::Offscreen { texture, size },
                        OFFSCREEN_FORMAT,
                    ));
                }
                Err(problem) => refused = Some(problem),
            }
        }
        Err(refused.unwrap_or_else(|| Problem::plain(ProblemKind::NoGpu, "no Vulkan and no GL")))
    }

    fn assemble(
        adapter: &wgpu::Adapter,
        device: wgpu::Device,
        queue: wgpu::Queue,
        target: Target,
        format: wgpu::TextureFormat,
    ) -> Self {
        let Built { pipeline, layout, bind_group, uniforms, mask, color, images, icons, preview } =
            build(&device, format);
        let capacity = 1 << 14;
        let instances = instance_buffer(&device, capacity);
        let info = adapter.get_info();
        let looking = crate::images::Preview::new(device.limits().max_texture_dimension_2d);
        Self {
            device,
            queue,
            target,
            pipeline,
            bind_group,
            uniforms,
            instances,
            capacity,
            mask,
            color,
            layout,
            images,
            icons,
            pages: std::collections::HashMap::new(),
            placeholder: preview,
            looking,
            atlas: Atlas::new(),
            pictures: Images::default(),
            marks: crate::images::Icons::default(),
            quads: Vec::with_capacity(capacity),
            adapter: info.name,
            backend: format!("{:?}", info.backend),
        }
    }

    /// The adapter's name, for the log and the diagnostics overlay.
    #[must_use]
    pub fn adapter(&self) -> &str {
        &self.adapter
    }

    /// Which backend was chosen.
    #[must_use]
    pub fn backend(&self) -> &str {
        &self.backend
    }

    /// Glyphs rasterized but not yet uploaded.
    ///
    /// Non-zero means the next frame owes texels. A capture waits on this so a
    /// golden frame is never one with holes in the text.
    #[must_use]
    pub fn pending_uploads(&self) -> usize {
        self.atlas.pending()
    }

    /// Glyphs held, and how full each atlas is.
    #[must_use]
    pub fn atlas_stats(&self) -> (usize, f32, f32) {
        let (mask, color) = self.atlas.utilization();
        (self.atlas.len(), mask, color)
    }

    /// Replaces the placeholder with the real atlas, the first time one is needed.
    ///
    /// **Once, and never back.** A window that has shown a thumbnail is a window
    /// that will show another; shrinking again would be a texture allocation on a
    /// path that has already paid for one.
    fn grow_the_image_atlas(&mut self) {
        if !self.pictures.in_use() || self.images.width() == IMAGE_SIDE {
            return;
        }
        self.images =
            atlas_texture(&self.device, "thumbnails", IMAGE_SIDE, wgpu::TextureFormat::Rgba8Unorm);
        self.rebuild_bindings();
    }

    /// The same, for the icon atlas's four bands — ADR 0042.
    fn grow_the_icon_atlas(&mut self) {
        if !self.marks.in_use() || self.icons.width() == ICON_SIDE {
            return;
        }
        self.icons =
            atlas_texture(&self.device, "icons", ICON_SIDE, wgpu::TextureFormat::Rgba8Unorm);
        self.rebuild_bindings();
    }

    /// Points the bind group at whatever the textures are now.
    ///
    /// **The layout never changes** — same bindings, same types — so this is a
    /// group rebuilt and not a pipeline. Called by the two things that replace a
    /// texture: the atlas outgrowing its placeholder, and each new preview.
    fn rebuild_bindings(&mut self) {
        let view =
            |texture: &wgpu::Texture| texture.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = |filter| {
            self.device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("atlas"),
                mag_filter: filter,
                min_filter: filter,
                ..Default::default()
            })
        };
        self.bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("atlas"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: self.uniforms.as_entire_binding() },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&view(&self.mask)),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&view(&self.color)),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::Sampler(&sampler(wgpu::FilterMode::Nearest)),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: wgpu::BindingResource::TextureView(&view(&self.images)),
                },
                wgpu::BindGroupEntry {
                    binding: 5,
                    resource: wgpu::BindingResource::Sampler(&sampler(wgpu::FilterMode::Linear)),
                },
                wgpu::BindGroupEntry {
                    binding: 6,
                    resource: wgpu::BindingResource::TextureView(&view(
                        self.looking
                            .bound()
                            .and_then(|key| self.pages.get(&key))
                            .unwrap_or(&self.placeholder),
                    )),
                },
                wgpu::BindGroupEntry {
                    binding: 7,
                    resource: wgpu::BindingResource::TextureView(&view(&self.icons)),
                },
            ],
        });
    }

    /// Puts the one preview picture in its own texture.
    ///
    /// Replaces whatever was there: there is one at a time, because there is one
    /// panel. The texture is made the next time a frame is drawn, so a preview that
    /// arrives and is dismissed before the frame costs no allocation at all.
    pub fn hold_preview(&mut self, key: ImageKey, size: (u32, u32), pixels: &[u8]) {
        if !self.looking.fits(size) {
            eprintln!(
                "skerry: a {}×{} preview is past this device's {}-texel limit and is not drawn",
                size.0,
                size.1,
                self.device.limits().max_texture_dimension_2d
            );
        }
        self.looking.hold(key, size, pixels);
    }

    /// The size the preview held under `key` came back at, if that is what is held.
    #[must_use]
    pub fn preview_size(&self, key: ImageKey) -> Option<(u32, u32)> {
        self.looking.size_of(key)
    }

    /// Makes a texture for each waiting page and uploads it.
    ///
    /// **Its own texture each, rather than one that is resized.** A preview is
    /// asked for at the panel's size and a page is asked for at the same size
    /// every time, so these are interchangeable in shape and separate in identity
    /// — which is what lets the one on screen and the two ahead of it coexist.
    fn upload_the_previews(&mut self) {
        while self.looking.owes_upload() {
            let Some((key, size, pixels)) = self.looking.take() else { break };
            let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("preview"),
                size: wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &pixels,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(size.0 * 4),
                    rows_per_image: Some(size.1),
                },
                wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
            );
            self.pages.insert(key, texture);
        }
        for gone in self.looking.over_budget() {
            self.pages.remove(&gone);
        }
    }

    /// Points the bind group at the page this frame draws, if it is not already.
    ///
    /// **Once per frame at most, and usually never.** A frame draws one page, so
    /// this is a bind-group rebuild on a turn rather than on every frame — the
    /// same trade the atlas's placeholder swap makes.
    fn bind_the_drawn_page(&mut self, list: &DrawList) {
        let wanted = list.first_preview();
        if wanted.is_none_or(|key| self.looking.is_bound(key)) {
            return;
        }
        self.looking.bound_to(wanted);
        self.rebuild_bindings();
    }

    /// Pages the preview budget may not drop. See [`crate::images::Preview::keep`].
    pub fn keep_previews(&mut self, keys: &[ImageKey]) {
        self.looking.keep(keys);
    }

    /// Forgets every held page. For a panel that has closed.
    pub fn forget_previews(&mut self) {
        for gone in self.looking.forget_all() {
            self.pages.remove(&gone);
        }
    }

    /// Puts a thumbnail in the atlas, or says it did not fit.
    ///
    /// `pixels` is straight `RGBA8`, `width` by `height`, neither above
    /// [`crate::images::SLOT`]. A key already held is a no-op that returns where it
    /// already is, so a caller may offer the same image every frame without
    /// re-uploading it. See [ADR
    /// 0018](../../../docs/adr/0018-thumbnails-are-slots-in-one-atlas-previews-are-not.md).
    pub fn hold_image(&mut self, key: ImageKey, width: u32, height: u32, pixels: &[u8]) -> bool {
        self.pictures.insert(key, width, height, pixels).is_some()
    }

    /// Puts an icon in the band that fits it, or says it did not fit.
    ///
    /// `pixels` is straight `RGBA8`, neither side above the largest band's 128 —
    /// [ADR 0042](../../../docs/adr/0042-where-a-mark-comes-from-is-a-setting-and-an-icon-is-a-band-in-its-own-atlas.md).
    pub fn hold_icon(&mut self, key: ImageKey, width: u32, height: u32, pixels: &[u8]) -> bool {
        self.marks.insert(key, width, height, pixels).is_some()
    }

    /// Whether the icon atlas already holds `key`, without marking it as drawn.
    ///
    /// Like [`Self::holds_image`] and not like `get`, for the same reason.
    #[must_use]
    pub fn holds_icon(&self, key: ImageKey) -> bool {
        self.marks.holds(key)
    }

    /// Icons held, how full the four bands are, and how many uploads are waiting.
    #[must_use]
    pub fn icon_stats(&self) -> (usize, f32, usize) {
        let (held, full) = self.marks.stats();
        (held, full, self.marks.pending())
    }

    /// Whether the atlas already holds `key`, without marking it as drawn.
    ///
    /// For a caller deciding whether to ask a child for something. **Not
    /// `Images::get`**, which is the drawing path and touches the frame stamp
    /// eviction sorts on — asking "do I need to fetch this" must not make an
    /// image look more recently used than it is.
    #[must_use]
    pub fn holds_image(&self, key: ImageKey) -> bool {
        self.pictures.holds(key)
    }

    /// How many pages are held and how many bytes they are.
    ///
    /// **For the memory ADR 0020 spends.** A page of A4 at a 1200-pixel panel is
    /// 4 MB and the budget is 24 MiB, so this is the line somebody reads when an
    /// RSS figure needs explaining.
    #[must_use]
    pub fn preview_stats(&self) -> (usize, usize) {
        self.looking.stats()
    }

    /// The size of the thumbnail held under `key`, without marking it as drawn.
    ///
    /// **For a stand-in's aspect.** A thumbnail preserves its picture's aspect, so
    /// its own dimensions are what a panel needs to place the picture before it
    /// has one. Like [`Self::holds_image`] and not like `get`: asking a question
    /// must not stamp the frame the atlas evicts by.
    #[must_use]
    pub fn image_size(&self, key: ImageKey) -> Option<(u32, u32)> {
        self.pictures.size_of(key)
    }

    /// Thumbnails held, how full the atlas is, and how many uploads are waiting.
    #[must_use]
    pub fn image_stats(&self) -> (usize, f32, usize) {
        let (held, full) = self.pictures.stats();
        (held, full, self.pictures.pending())
    }

    /// The size the target is currently at, in device pixels.
    #[must_use]
    pub fn size(&self) -> (u32, u32) {
        match &self.target {
            Target::Window { config, .. } => (config.width, config.height),
            Target::Offscreen { size, .. } => *size,
        }
    }

    /// Reconfigures for a new physical size.
    ///
    /// Called on every `Resized`, including a scale change with no size change:
    /// the surface must be the window's physical size exactly, or the compositor
    /// resamples it and every glyph goes soft.
    pub fn resize(&mut self, physical: (u32, u32)) {
        let (width, height) = (physical.0.max(1), physical.1.max(1));
        if self.size() == (width, height) {
            return;
        }
        match &mut self.target {
            Target::Window { surface, config } => {
                config.width = width;
                config.height = height;
                surface.configure(&self.device, config);
            }
            Target::Offscreen { texture, size } => {
                *size = (width, height);
                *texture = offscreen_texture(&self.device, (width, height));
            }
        }
    }

    /// Submits one frame, and presents it if there is a window.
    pub fn draw(
        &mut self,
        list: &mut DrawList,
        fonts: &mut Fonts,
        background: crate::Color,
    ) -> FrameStats {
        let mut stats = FrameStats::default();
        list.finish();
        // **The image atlas's frame starts here, not at a caller's discretion.**
        // Eviction sorts on the frame an image was last drawn in, so the count has
        // to advance exactly once per frame — a caller who forgot would make every
        // entry look equally old and eviction arbitrary.
        self.pictures.begin_frame();
        self.marks.begin_frame();
        self.looking.begin_frame();
        let (width, height) = self.size();

        let acquire_started = std::time::Instant::now();
        // A surface that has to be reconfigured is normal: it happens on every
        // resize, and on some drivers after a suspend. Losing one frame is
        // correct; giving up is not.
        let frame = match &self.target {
            Target::Window { surface, config } => {
                match surface.get_current_texture().or_else(|_| {
                    surface.configure(&self.device, config);
                    surface.get_current_texture()
                }) {
                    Ok(frame) => Some(frame),
                    Err(_) => return stats,
                }
            }
            Target::Offscreen { .. } => None,
        };
        stats.acquire_us = acquire_started.elapsed().as_micros() as u64;
        let work_started = std::time::Instant::now();

        let view = match (&frame, &self.target) {
            (Some(frame), _) => frame.texture.create_view(&wgpu::TextureViewDescriptor::default()),
            (None, Target::Offscreen { texture, .. }) => {
                texture.create_view(&wgpu::TextureViewDescriptor::default())
            }
            (None, Target::Window { .. }) => return stats,
        };

        let boundaries = self.resolve(list, fonts, &mut stats);
        self.atlas.flush(&self.queue, &self.mask, &self.color);
        self.grow_the_image_atlas();
        self.grow_the_icon_atlas();
        self.upload_the_previews();
        self.bind_the_drawn_page(list);
        self.pictures.flush(&self.queue, &self.images);
        self.marks.flush(&self.queue, &self.icons);

        if self.quads.len() > self.capacity {
            self.capacity = self.quads.len().next_power_of_two();
            self.instances = instance_buffer(&self.device, self.capacity);
        }

        let uniforms = [
            width as f32,
            height as f32,
            MASK_SIDE as f32,
            MASK_SIDE as f32,
            COLOR_SIDE as f32,
            COLOR_SIDE as f32,
            IMAGE_SIDE as f32,
            IMAGE_SIDE as f32,
            ICON_SIDE as f32,
            ICON_SIDE as f32,
        ];
        // SAFETY: `[f32; 10]` and `Quad`, neither with padding — see `as_bytes`.
        let (uniforms, quads) = unsafe { (as_bytes(&uniforms), as_bytes(&self.quads)) };
        self.queue.write_buffer(&self.uniforms, 0, uniforms);
        self.queue.write_buffer(&self.instances, 0, quads);

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("quads"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear_color(background)),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.set_vertex_buffer(0, self.instances.slice(..));

            let mut start = 0u32;
            for (clip, end) in boundaries {
                let end = end as u32;
                if end > start && !clip.is_empty() {
                    let scissor = clamp_scissor(clip, width, height);
                    if scissor.2 > 0 && scissor.3 > 0 {
                        pass.set_scissor_rect(scissor.0, scissor.1, scissor.2, scissor.3);
                        pass.draw(0..6, start..end);
                        stats.draws += 1;
                    }
                }
                start = end;
            }
        }
        self.queue.submit([encoder.finish()]);
        stats.cpu_us = work_started.elapsed().as_micros() as u64;
        if let Some(frame) = frame {
            frame.present();
        }

        stats.quads = self.quads.len();
        stats
    }

    /// Reads the offscreen target back as 8-bit RGBA rows.
    ///
    /// `None` for a window target: a compositor's surface is not ours to read,
    /// and pretending otherwise would make a golden frame a screenshot.
    pub fn capture(&self) -> Option<Vec<u8>> {
        let Target::Offscreen { texture, size } = &self.target else { return None };
        let (width, height) = *size;
        // `copy_texture_to_buffer` requires each row to start on a 256-byte
        // boundary, so the buffer is padded and the padding is dropped on the
        // way out.
        let unpadded = width * 4;
        let padded = unpadded.next_multiple_of(256);
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: u64::from(padded) * u64::from(height),
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("readback") });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        );
        self.queue.submit([encoder.finish()]);

        let (sender, receiver) = std::sync::mpsc::channel();
        buffer.slice(..).map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result.is_ok());
        });
        // Blocking here is correct: a capture is a deliberate stop, not a frame.
        let _ = self.device.poll(wgpu::PollType::Wait { submission_index: None, timeout: None });
        if receiver.recv().ok() != Some(true) {
            return None;
        }

        let mapped = buffer.slice(..).get_mapped_range();
        let mut out = Vec::with_capacity((unpadded * height) as usize);
        for row in 0..height as usize {
            let start = row * padded as usize;
            out.extend_from_slice(&mapped[start..start + unpadded as usize]);
        }
        drop(mapped);
        buffer.unmap();
        Some(out)
    }

    /// Turns the frame's items into instances, and reports where each clip
    /// region's run ends.
    ///
    /// This is where a glyph becomes a rectangle: the atlas is consulted, and a
    /// glyph seen for the first time is rasterized at exactly the size it will
    /// be drawn at.
    fn resolve(
        &mut self,
        list: &DrawList,
        fonts: &mut Fonts,
        stats: &mut FrameStats,
    ) -> Vec<(crate::Rect, usize)> {
        self.quads.clear();
        let mut boundaries = Vec::with_capacity(list.groups.len());
        let mut next = 0usize;
        for group in &list.groups {
            for item in &list.items[next..group.end] {
                match item {
                    Item::Quad(quad) => self.quads.push(*quad),
                    Item::Glyph { key, x, y, color } => match self.place(*key, fonts, stats) {
                        Some(placed) if placed.width > 0 => {
                            self.quads.push(glyph_quad(placed, *x, *y, color.packed()));
                        }
                        // A space, or any glyph with no coverage, is a real and
                        // common case and draws nothing.
                        Some(_) => {}
                        None => stats.dropped += 1,
                    },
                    Item::Preview { key, rect, alpha } => {
                        // Marked drawn so eviction leaves the page on screen
                        // alone, which is the atlas's rule in a second store.
                        self.looking.drawn(*key);
                        if let Some(size) = self.looking.size_of(*key) {
                            self.quads.push(Quad {
                                rect: [rect.x, rect.y, rect.width, rect.height],
                                uv: [0.0, 0.0, size.0 as f32, size.1 as f32],
                                color: alpha.packed(),
                                kind: KIND_PREVIEW,
                                radius: 0.0,
                                border: 0.0,
                            });
                        }
                    }
                    Item::Standby { key, rect, alpha } => {
                        if let Some(slot) = self.pictures.get(*key) {
                            self.quads.push(filled_quad(slot, *rect, alpha.packed()));
                        }
                    }
                    Item::Icon { key, rect, alpha } => {
                        if let Some(slot) = self.marks.get(*key) {
                            self.quads.push(icon_quad(slot, *rect, alpha.packed()));
                        }
                        // An icon the atlas does not hold yet draws nothing here
                        // — the geometric mark under it is what is on screen, and
                        // the caller is what decides that. See `view::icon_of`.
                    }
                    Item::Image { key, rect, alpha } => {
                        if let Some(slot) = self.pictures.get(*key) {
                            self.quads.push(image_quad(slot, *rect, alpha.packed()));
                        }
                        // An image the atlas does not hold yet draws nothing,
                        // which is what a cell looks like while its thumbnail is
                        // still being decoded. Not a dropped frame: nobody has
                        // failed, the answer has not arrived.
                    }
                }
            }
            next = group.end;
            boundaries.push((group.clip, self.quads.len()));
        }
        boundaries
    }

    /// The atlas placement for `key`, rasterizing it if this is its first frame.
    fn place(
        &mut self,
        key: GlyphKey,
        fonts: &mut Fonts,
        stats: &mut FrameStats,
    ) -> Option<Placed> {
        if let Some(placed) = self.atlas.get(key) {
            return Some(placed);
        }
        stats.rasterized += 1;
        let image = fonts.rasterize(key)?;
        self.atlas.insert(key, &image)
    }
}

/// One instance, for one backend.
///
/// Deliberately **not** `VULKAN | GL`. Asking for both makes `wgpu` initialize
/// both, and initializing GL creates an EGL display and context on the way to an
/// adapter we will not use — measured at 131–181 ms of a 195 ms cold start
/// against an 80 ms budget. ADR 0003 says Vulkan where available and GL as the
/// fallback, which is a sequence and not a union.
fn instance(backends: wgpu::Backends) -> wgpu::Instance {
    wgpu::Instance::new(&wgpu::InstanceDescriptor { backends, ..Default::default() })
}

/// The backends to try, in order.
const BACKENDS: [wgpu::Backends; 2] = [wgpu::Backends::VULKAN, wgpu::Backends::GL];

fn open_device(
    instance: &wgpu::Instance,
    surface: Option<&wgpu::Surface<'static>>,
    wanted: Option<(u32, u32)>,
) -> Result<(wgpu::Adapter, wgpu::Device, wgpu::Queue), Problem> {
    let adapter = match wanted.and_then(|ids| matching_adapter(instance, surface, ids)) {
        Some(adapter) => adapter,
        None => request_any(instance, surface)?,
    };
    let (device, queue) = timed_out(
        pollster(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("skerry"),
            required_features: wgpu::Features::empty(),
            // Downlevel defaults, raised to what this adapter offers: the limits we
            // actually need are one vertex buffer and two 2048-pixel textures, and
            // asking for less is what makes the GL fallback work at all.
            required_limits: wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
            ..Default::default()
        })),
        "request_device",
    )?
    .map_err(|error| Problem::plain(ProblemKind::NoGpu, format!("request_device: {error}")))?;
    Ok((adapter, device, queue))
}

/// The adapter on a particular PCI device, if it can present to this surface.
///
/// **Not a preference — a correctness requirement on a hybrid machine.** A buffer
/// allocated on one GPU cannot be imported by a compositor rendering on another:
/// measured, mutter chose the discrete NVIDIA card, this asked for `LowPower` and
/// got the integrated AMD one, and the compositor answered `failed to import
/// supplied dmabufs` and killed the connection at start-up.
///
/// `None` when nothing matches — a machine with one GPU, or a compositor whose
/// device has no PCI ids to compare — and the caller then chooses as it always
/// did.
fn matching_adapter(
    instance: &wgpu::Instance,
    surface: Option<&wgpu::Surface<'static>>,
    (vendor, device): (u32, u32),
) -> Option<wgpu::Adapter> {
    instance.enumerate_adapters(wgpu::Backends::all()).into_iter().find(|adapter| {
        let info = adapter.get_info();
        info.vendor == vendor
            && info.device == device
            && surface.is_none_or(|surface| adapter.is_surface_supported(surface))
    })
}

/// Whatever the driver thinks is best, which is what a single-GPU machine wants.
fn request_any(
    instance: &wgpu::Instance,
    surface: Option<&wgpu::Surface<'static>>,
) -> Result<wgpu::Adapter, Problem> {
    timed_out(
        pollster(instance.request_adapter(&wgpu::RequestAdapterOptions {
            // A file manager is not a game. On a laptop with two GPUs the integrated
            // one draws quads at the same speed and does not spin up a fan or cost
            // an hour of battery — unless the compositor is on the other one, which
            // `matching_adapter` above has already decided.
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: surface,
            force_fallback_adapter: false,
        })),
        "request_adapter",
    )?
    .map_err(|error| Problem::plain(ProblemKind::NoGpu, format!("request_adapter: {error}")))
}

/// Turns a missed [`SETUP_DEADLINE`] into a `Problem` that names what stalled.
fn timed_out<T>(value: Option<T>, what: &str) -> Result<T, Problem> {
    value.ok_or_else(|| {
        Problem::plain(
            ProblemKind::NoGpu,
            format!("{what} did not answer in {}s", SETUP_DEADLINE.as_secs()),
        )
    })
}

/// The quad for a thumbnail: fitted inside its box, centred, aspect kept.
///
/// **Centred rather than stretched.** A photograph pulled to a square is worse
/// than a photograph with space beside it, and a grid where every cell's picture
/// has a different distortion is a grid nobody can scan.
fn image_quad(slot: crate::images::Slot, rect: crate::Rect, color: u32) -> Quad {
    let scale = (rect.width / slot.width.max(1) as f32)
        .min(rect.height / slot.height.max(1) as f32)
        .min(1.0);
    let (width, height) = (slot.width as f32 * scale, slot.height as f32 * scale);
    Quad {
        rect: [
            (rect.x + (rect.width - width) / 2.0).round(),
            (rect.y + (rect.height - height) / 2.0).round(),
            width.round().max(1.0),
            height.round().max(1.0),
        ],
        uv: [slot.x as f32, slot.y as f32, slot.width as f32, slot.height as f32],
        color,
        kind: KIND_IMAGE,
        radius: 0.0,
        border: 0.0,
    }
}

/// The same as [`image_quad`] with the never-enlarge rule taken out.
///
/// A stand-in is drawn where the picture it precedes will be, so it fills the
/// rectangle it is given: the caller has fitted that rectangle to the *aspect*,
/// which is the same for the thumbnail and for the picture, so filling it is
/// landing in the right place rather than stretching.
fn filled_quad(slot: crate::images::Slot, rect: crate::Rect, color: u32) -> Quad {
    Quad {
        rect: [
            rect.x.round(),
            rect.y.round(),
            rect.width.round().max(1.0),
            rect.height.round().max(1.0),
        ],
        uv: [slot.x as f32, slot.y as f32, slot.width as f32, slot.height as f32],
        color,
        kind: KIND_IMAGE,
        radius: 0.0,
        border: 0.0,
    }
}

/// An icon, centred in its box and never enlarged past its own band.
///
/// **The same fit as [`image_quad`], with its own kind.** An icon comes from the
/// band at or above the size it is drawn at, so the scale is at most one and the
/// picture is minified rather than blown up — which is the whole reason the
/// freedesktop per-size directories exist.
fn icon_quad(slot: crate::images::Slot, rect: crate::Rect, color: u32) -> Quad {
    Quad { kind: KIND_ICON, ..image_quad(slot, rect, color) }
}

/// The shader's `KIND_IMAGE`, after the four triangle directions.
const KIND_IMAGE: u32 = 7;

/// A themed icon, from the banded atlas — ADR 0042.
const KIND_ICON: u32 = 10;

/// And the one preview, which samples a texture of its own.
const KIND_PREVIEW: u32 = 8;

fn glyph_quad(placed: Placed, pen_x: f32, baseline_y: f32, color: u32) -> Quad {
    // `left` and `top` come from the rasterizer and are already in device pixels
    // at this glyph's size, so the destination is an integer translation of the
    // source. Rounding the pen instead would undo subpixel positioning.
    let x = pen_x + placed.left as f32;
    let y = baseline_y - placed.top as f32;
    Quad {
        rect: [x, y, placed.width as f32, placed.height as f32],
        uv: [placed.x as f32, placed.y as f32, placed.width as f32, placed.height as f32],
        color,
        kind: if placed.color { KIND_COLOR } else { KIND_MASK },
        radius: 0.0,
        border: 0.0,
    }
}

fn clamp_scissor(clip: crate::Rect, width: u32, height: u32) -> (u32, u32, u32, u32) {
    let x = clip.x.floor().max(0.0) as u32;
    let y = clip.y.floor().max(0.0) as u32;
    let right = (clip.right().ceil().max(0.0) as u32).min(width);
    let bottom = (clip.bottom().ceil().max(0.0) as u32).min(height);
    (x.min(width), y.min(height), right.saturating_sub(x), bottom.saturating_sub(y))
}

fn clear_color(color: crate::Color) -> wgpu::Color {
    let (red, green, blue, alpha) = color.components();
    let channel = |value: u8| f64::from(value) / 255.0;
    wgpu::Color { r: channel(red), g: channel(green), b: channel(blue), a: channel(alpha) }
}

/// The `raw-window-handle` pair for whichever display server produced these.
///
/// The one place in the GPU stack that knows there is more than one. `sk-plat`
/// hands over raw pointers and ids on purpose — it may not name a
/// `raw-window-handle` type, and `tools/check-invariants.py` enforces that — so
/// the translation happens here, at the seam it belongs to.
///
/// **`Xcb` and not `Xlib`.** `x11rb` holds an `xcb_connection_t *` and
/// `VK_KHR_xcb_surface` takes one; going through `Xlib` would mean loading a
/// second library to hand back a pointer we already have.
fn raw_pair(
    handles: RawHandles,
) -> Result<(wgpu::rwh::RawDisplayHandle, wgpu::rwh::RawWindowHandle), Problem> {
    match handles {
        RawHandles::Wayland { display, surface } => {
            let (Some(display), Some(surface)) =
                (std::ptr::NonNull::new(display), std::ptr::NonNull::new(surface))
            else {
                return Err(Problem::plain(ProblemKind::NoGpu, "null compositor handle"));
            };
            Ok((
                wgpu::rwh::RawDisplayHandle::Wayland(wgpu::rwh::WaylandDisplayHandle::new(display)),
                wgpu::rwh::RawWindowHandle::Wayland(wgpu::rwh::WaylandWindowHandle::new(surface)),
            ))
        }
        RawHandles::X11 { connection, screen, window, visual } => {
            let Some(window) = std::num::NonZeroU32::new(window) else {
                return Err(Problem::plain(ProblemKind::NoGpu, "null X11 window"));
            };
            let mut handle = wgpu::rwh::XcbWindowHandle::new(window);
            // The visual the window was created with. Vulkan picks one itself if
            // this is absent, and on a server offering more than one visual of
            // the same depth it can pick a different one from the window's —
            // which is a `BadMatch` at surface creation and no window at all.
            handle.visual_id = std::num::NonZeroU32::new(visual);
            Ok((
                wgpu::rwh::RawDisplayHandle::Xcb(wgpu::rwh::XcbDisplayHandle::new(
                    std::ptr::NonNull::new(connection),
                    screen,
                )),
                wgpu::rwh::RawWindowHandle::Xcb(handle),
            ))
        }
    }
}

/// Prefers a non-sRGB 8-bit format.
///
/// With an sRGB surface the hardware would convert everything we write, so a
/// colour picked in a theme would arrive on screen as a different colour and
/// every alpha blend would happen in a different space than the one the theme
/// was designed in. Writing sRGB values into a linear-unorm surface is what
/// every other toolkit does, and matching them is not a detail a user can unsee.
fn pick_format(capabilities: &wgpu::SurfaceCapabilities) -> Option<wgpu::TextureFormat> {
    let wanted = [wgpu::TextureFormat::Bgra8Unorm, wgpu::TextureFormat::Rgba8Unorm];
    wanted
        .into_iter()
        .find(|format| capabilities.formats.contains(format))
        .or_else(|| capabilities.formats.first().copied())
}

/// Everything that depends only on the device and the target format.
struct Built {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    bind_group: wgpu::BindGroup,
    uniforms: wgpu::Buffer,
    mask: wgpu::Texture,
    color: wgpu::Texture,
    images: wgpu::Texture,
    icons: wgpu::Texture,
    preview: wgpu::Texture,
}

fn build(device: &wgpu::Device, format: wgpu::TextureFormat) -> Built {
    let mask = atlas_texture(device, "glyph coverage", MASK_SIDE, wgpu::TextureFormat::R8Unorm);
    let color = atlas_texture(device, "colour glyphs", COLOR_SIDE, wgpu::TextureFormat::Rgba8Unorm);
    let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("uniforms"),
        // Five `vec2<f32>`, rounded up to the sixteen bytes a uniform binding is
        // sized in.
        size: 48,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    // **One texel until a thumbnail exists** — ADR 0018's "allocated lazily". A
    // window that opens no image pays no sixteen megabytes, which is what makes
    // that size affordable against an RSS budget that is already missed. `wgpu`
    // has no null binding, so the placeholder is a real 1×1 texture and the swap
    // is a bind group rebuilt; the *layout* never changes, so the pipeline does
    // not either.
    let images = atlas_texture(device, "thumbnails", 1, wgpu::TextureFormat::Rgba8Unorm);
    // One texel until an icon exists, which is the same rule and the reason a
    // window under `icons = marks` allocates none of the four megabytes.
    let icons = atlas_texture(device, "icons", 1, wgpu::TextureFormat::Rgba8Unorm);
    // One texel until a preview exists, for the reason the atlas is.
    let preview = atlas_texture(device, "preview", 1, wgpu::TextureFormat::Rgba8Unorm);
    let lazy = Atlases { images: &images, icons: &icons, preview: &preview };
    let (layout, bind_group) = bindings(device, &uniforms, &mask, &color, lazy);
    Built {
        pipeline: pipeline(device, &layout, format),
        layout,
        bind_group,
        uniforms,
        mask,
        color,
        images,
        icons,
        preview,
    }
}

/// The three textures that are one texel until something needs them.
#[derive(Clone, Copy)]
struct Atlases<'t> {
    images: &'t wgpu::Texture,
    icons: &'t wgpu::Texture,
    preview: &'t wgpu::Texture,
}

fn bindings(
    device: &wgpu::Device,
    uniforms: &wgpu::Buffer,
    mask: &wgpu::Texture,
    color: &wgpu::Texture,
    lazy: Atlases<'_>,
) -> (wgpu::BindGroupLayout, wgpu::BindGroup) {
    let Atlases { images, icons, preview } = lazy;
    // Nearest, deliberately. Every glyph is drawn 1:1 from a raster made at its
    // device size, so a filtering sampler could only hide a bug — which is
    // exactly the bug Spike A had.
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("atlas"),
        mag_filter: wgpu::FilterMode::Nearest,
        min_filter: wgpu::FilterMode::Nearest,
        ..Default::default()
    });
    // **And a filtering one, for thumbnails only.** The sentence above is about
    // glyphs and stays true of them. A thumbnail is the other case: the XDG cache
    // is 128 pixels square and a grid mark is forty to sixty, so it is *always*
    // minified — and nearest-sampling a photograph down by half is the aliasing a
    // box filter was used in the child to avoid, undone on the GPU.
    let smooth = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("images"),
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
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
            texture_entry(1, false),
            texture_entry(2, false),
            wgpu::BindGroupLayoutEntry {
                binding: 3,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::NonFiltering),
                count: None,
            },
            texture_entry(4, true),
            texture_entry(6, true),
            texture_entry(7, true),
            wgpu::BindGroupLayoutEntry {
                binding: 5,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
        ],
    });

    let view =
        |texture: &wgpu::Texture| texture.create_view(&wgpu::TextureViewDescriptor::default());
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("atlas"),
        layout: &layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: uniforms.as_entire_binding() },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(&view(mask)),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::TextureView(&view(color)),
            },
            wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::Sampler(&sampler) },
            wgpu::BindGroupEntry {
                binding: 4,
                resource: wgpu::BindingResource::TextureView(&view(images)),
            },
            wgpu::BindGroupEntry { binding: 5, resource: wgpu::BindingResource::Sampler(&smooth) },
            wgpu::BindGroupEntry {
                binding: 6,
                resource: wgpu::BindingResource::TextureView(&view(preview)),
            },
            wgpu::BindGroupEntry {
                binding: 7,
                resource: wgpu::BindingResource::TextureView(&view(icons)),
            },
        ],
    });
    (layout, bind_group)
}

fn pipeline(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    format: wgpu::TextureFormat,
) -> wgpu::RenderPipeline {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("quads"),
        source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("quads"),
        bind_group_layouts: &[layout],
        push_constant_ranges: &[],
    });
    let float4 = wgpu::VertexFormat::Float32x4;
    let attributes = [
        wgpu::VertexAttribute { format: float4, offset: 0, shader_location: 0 },
        wgpu::VertexAttribute { format: float4, offset: 16, shader_location: 1 },
        wgpu::VertexAttribute {
            format: wgpu::VertexFormat::Uint32,
            offset: 32,
            shader_location: 2,
        },
        wgpu::VertexAttribute {
            format: wgpu::VertexFormat::Uint32,
            offset: 36,
            shader_location: 3,
        },
        wgpu::VertexAttribute {
            format: wgpu::VertexFormat::Float32,
            offset: 40,
            shader_location: 4,
        },
        wgpu::VertexAttribute {
            format: wgpu::VertexFormat::Float32,
            offset: 44,
            shader_location: 5,
        },
    ];
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("quads"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            buffers: &[wgpu::VertexBufferLayout {
                array_stride: std::mem::size_of::<Quad>() as u64,
                step_mode: wgpu::VertexStepMode::Instance,
                attributes: &attributes,
            }],
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
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
    })
}

fn offscreen_texture(device: &wgpu::Device, size: (u32, u32)) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("offscreen"),
        size: wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: OFFSCREEN_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    })
}

fn atlas_texture(
    device: &wgpu::Device,
    label: &str,
    side: u32,
    format: wgpu::TextureFormat,
) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d { width: side, height: side, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

fn instance_buffer(device: &wgpu::Device, capacity: usize) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("quads"),
        size: (capacity * std::mem::size_of::<Quad>()) as u64,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

fn texture_entry(binding: u32, filterable: bool) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

/// Reinterprets a slice of plain-old-data as bytes.
///
/// Four lines instead of `bytemuck`, which is a derive macro and four crates
/// against a budget of twenty-five for one cast in one file.
///
/// # Safety
///
/// Every byte of every `T` must be initialised: no padding, and no field that
/// may hold uninitialised memory (`MaybeUninit`, a union). Padding would hand
/// the GPU bytes nobody wrote, and reading those is undefined behaviour. `f32`,
/// `u32`, arrays of them, and a `#[repr(C)]` struct of them whose size is the
/// sum of its fields' all qualify — `Quad` asserts that last part where it is
/// declared.
unsafe fn as_bytes<T: Copy>(slice: &[T]) -> &[u8] {
    // SAFETY: the pointer and length describe exactly the memory `slice`
    // borrows, `u8` has no alignment to violate, and the caller's contract makes
    // every one of those bytes initialised.
    unsafe { std::slice::from_raw_parts(slice.as_ptr().cast::<u8>(), std::mem::size_of_val(slice)) }
}

/// How long a setup future may stay pending before it is a hang, not a wait.
///
/// The whole device open measures 19–25 ms on Vulkan
/// ([docs/09](../../../docs/09-performance-budgets.md)), so two seconds is two
/// orders of magnitude of headroom. The deadline is not there to be reached; it
/// is there so that a `wgpu` version or a backend which returns `Pending` from
/// one of these is a refusal with a message rather than a core at 100% with no
/// way out — the same shape as the idle-CPU bug docs/09 already records.
const SETUP_DEADLINE: std::time::Duration = std::time::Duration::from_secs(2);

/// Blocks on a `wgpu` future without an async runtime.
///
/// [ADR 0005](../../../docs/adr/0005-no-async-runtime.md) forbids the runtime,
/// and this is why it costs nothing: `wgpu`'s three setup futures are the only
/// ones in the program, they resolve immediately on a native backend, and a
/// spinning thread during start-up is a better answer than an executor.
///
/// `None` when [`SETUP_DEADLINE`] passes. The waker is a no-op, so polling is
/// the only way this can ever make progress; after the first few hundred passes
/// it yields rather than spins, because by then the answer is not coming from
/// this core.
fn pollster<F: std::future::Future>(future: F) -> Option<F::Output> {
    use std::task::{Context, Poll, Waker};
    let deadline = std::time::Instant::now() + SETUP_DEADLINE;
    let mut future = std::pin::pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    let mut passes = 0u32;
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
            return Some(value);
        }
        passes += 1;
        if passes < 256 {
            std::hint::spin_loop();
            continue;
        }
        if std::time::Instant::now() > deadline {
            return None;
        }
        std::thread::yield_now();
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::{SETUP_DEADLINE, as_bytes, clamp_scissor, glyph_quad, pollster};
    use crate::Rect;
    use crate::atlas::Placed;

    #[test]
    fn a_scissor_never_leaves_the_surface() {
        let clipped = clamp_scissor(Rect::new(-10.0, -10.0, 5000.0, 5000.0), 800, 600);
        assert_eq!(clipped, (0, 0, 800, 600));
        let outside = clamp_scissor(Rect::new(900.0, 700.0, 10.0, 10.0), 800, 600);
        assert_eq!(outside.2, 0, "a clip past the edge draws nothing");
    }

    #[test]
    fn a_glyph_lands_where_the_rasterizer_said_it_would() {
        let placed = Placed { x: 7, y: 9, width: 6, height: 11, left: 1, top: 9, color: false };
        let quad = glyph_quad(placed, 100.25, 40.0, 0xffff_ffff);
        // The pen plus the bearing, unrounded: rounding here is what makes a run
        // of text stutter.
        assert!((quad.rect[0] - 101.25).abs() < f32::EPSILON);
        // The baseline minus the ascent above it.
        assert!((quad.rect[1] - 31.0).abs() < f32::EPSILON);
        assert!(
            quad.uv.iter().zip([7.0, 9.0, 6.0, 11.0]).all(|(a, b)| (a - b).abs() < f32::EPSILON)
        );
    }

    #[test]
    fn as_bytes_covers_the_whole_slice_and_nothing_more() {
        let values = [1.0f32, 2.0, 3.0];
        // SAFETY: `f32` has no padding.
        let bytes = unsafe { as_bytes(&values) };
        assert_eq!(bytes.len(), 12);
        assert_eq!(bytes[0..4], 1.0f32.to_ne_bytes());
    }

    #[test]
    fn a_ready_future_resolves_without_an_executor() {
        assert_eq!(pollster(std::future::ready(7)), Some(7));
    }

    /// The waker is a no-op, so a future that never resolves can only be polled
    /// forever. Without the deadline, start-up became a hot spin on one core
    /// with no way out — and the claim that `wgpu`'s setup futures resolve
    /// immediately is a claim about a dependency's internals with nothing
    /// enforcing it.
    #[test]
    fn a_future_that_never_resolves_gives_up_rather_than_spinning_forever() {
        struct Never;
        impl std::future::Future for Never {
            type Output = ();
            fn poll(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
            ) -> std::task::Poll<()> {
                std::task::Poll::Pending
            }
        }
        let started = std::time::Instant::now();
        assert_eq!(pollster(Never), None);
        let waited = started.elapsed();
        assert!(waited >= SETUP_DEADLINE, "gave up after {waited:?}");
        assert!(waited < SETUP_DEADLINE * 2, "took {waited:?} to give up");
    }
}
