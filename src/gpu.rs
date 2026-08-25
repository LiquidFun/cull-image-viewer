//! wgpu renderer: one textured quad, plus a bounded texture cache.
//!
//! This layer holds no policy. Zoom, pan, orientation and fit decisions all come from
//! [`crate::view`], which is unit-tested; here we only upload pixels and issue a draw.
//!
//! Textures are `Rgba8UnormSrgb` on an sRGB surface so the GPU does sRGB<->linear
//! conversion around filtering in fixed-function hardware (REQUIREMENTS.md R10).

use std::collections::HashMap;

use crate::decode::Image;
use crate::view::{Orientation, View, Viewport};

/// Uniform block matching `Transform` in shader.wgsl. 16-byte aligned throughout.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Transform {
    scale: [f32; 2],
    offset: [f32; 2],
    uv: [f32; 4],
}

/// A texture plus the metadata needed to draw it.
pub struct Entry {
    texture: wgpu::Texture,
    /// Built once at upload time rather than per frame: it only references the texture
    /// view, the uniform buffer and the sampler, none of which change afterwards.
    bind: wgpu::BindGroup,
    pub stored: (u32, u32),
    pub orientation: Orientation,
    /// Colour verdict for the decoded pixels, so R10's non-sRGB warning can reach the UI.
    pub colour: crate::icc::Verdict,
    /// False until every row has been written. A partially filled texture must not be
    /// drawn, because the unwritten rows hold undefined data.
    complete: bool,
}

/// Bounded map from group index to payload, with window-based eviction.
///
/// Generic over the payload purely so the eviction bookkeeping can be unit-tested
/// without a GPU device; the real instantiation is inside [`TextureCache`].
///
/// Every mutator returns the values it evicted, so the owner can recycle them rather
/// than let them drop.
///
/// The cap mirrors the prefetch window: there is no point holding textures for images
/// the ring has already evicted. At 25.6 MP and 4 bytes per pixel each entry is ~102 MB,
/// so the cap is what keeps VRAM in hand.
pub struct Cache<T> {
    entries: HashMap<usize, T>,
    capacity: usize,
}

impl<T> Cache<T> {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: HashMap::new(),
            capacity: capacity.max(1),
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn contains(&self, index: usize) -> bool {
        self.entries.contains_key(&index)
    }

    pub fn get(&self, index: usize) -> Option<&T> {
        self.entries.get(&index)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Drop everything, e.g. after the group list changed and indices shifted.
    pub fn clear(&mut self) -> Vec<T> {
        self.entries.drain().map(|(_, v)| v).collect()
    }

    /// Evict entries outside `centre +/- radius`, optionally keeping one extra index.
    ///
    /// The extra index is the hovered row, whose texture would otherwise be evicted
    /// immediately after upload because it sits far outside the navigation window.
    pub fn retain_window_with(
        &mut self,
        centre: usize,
        radius: usize,
        keep: Option<usize>,
    ) -> Vec<T> {
        let lo = centre.saturating_sub(radius);
        let hi = centre.saturating_add(radius);
        let doomed: Vec<usize> = self
            .entries
            .keys()
            .copied()
            .filter(|&i| !((i >= lo && i <= hi) || keep == Some(i)))
            .collect();
        doomed
            .into_iter()
            .filter_map(|i| self.entries.remove(&i))
            .collect()
    }

    /// Insert, then evict the entries furthest from `index` until within capacity.
    pub fn insert(&mut self, index: usize, value: T) -> Vec<T> {
        let mut evicted = Vec::new();
        if let Some(old) = self.entries.insert(index, value) {
            evicted.push(old);
        }
        while self.entries.len() > self.capacity {
            let Some(&victim) = self
                .entries
                .keys()
                .filter(|&&i| i != index)
                .max_by_key(|&&i| i.abs_diff(index))
            else {
                break;
            };
            if let Some(v) = self.entries.remove(&victim) {
                evicted.push(v);
            }
        }
        evicted
    }
}

/// The real cache: group index to uploaded texture, plus a pool of textures to reuse.
///
/// Recycling matters: every frame in a shoot is the same size, so without a pool the
/// program creates and destroys 102 MB textures continuously as the window moves. That
/// churn makes the driver allocate and free large blocks all session, which is what made
/// the UI degrade the longer it ran.
/// An upload in progress, filled a band of rows at a time.
struct Pending {
    index: usize,
    image: Box<Image>,
    /// Next row to write.
    row: u32,
}

pub struct TextureCache {
    cache: Cache<Entry>,
    pool: Vec<wgpu::Texture>,
    /// Cap on recycled textures held in reserve, so the pool cannot itself grow without
    /// bound if the user opens folders of differing image sizes.
    pool_limit: usize,
    /// Uploads not yet finished, nearest-to-cursor first.
    pending: Vec<Pending>,
    /// The non-texture half of every bind group. Cloned handles, so this is cheap.
    layout: wgpu::BindGroupLayout,
    uniform: wgpu::Buffer,
    sampler: wgpu::Sampler,
    /// Where finished pixel buffers go, to be decoded into again rather than freed.
    pixels: std::sync::Arc<crate::decode::BufferPool>,
}

impl TextureCache {
    pub fn new(
        capacity: usize,
        layout: wgpu::BindGroupLayout,
        uniform: wgpu::Buffer,
        sampler: wgpu::Sampler,
        pixels: std::sync::Arc<crate::decode::BufferPool>,
    ) -> Self {
        Self {
            cache: Cache::new(capacity),
            pool: Vec::new(),
            // Four spares is plenty to absorb the churn of a moving window.
            pool_limit: 4,
            pending: Vec::new(),
            layout,
            uniform,
            sampler,
            pixels,
        }
    }

    /// Finish with an upload, returning its pixels to the pool for the decoders.
    fn retire(&mut self, index: usize) {
        if let Some(i) = self.pending.iter().position(|p| p.index == index) {
            // `remove`, not `swap_remove`: the list is kept in nearest-first order and
            // reshuffling it mid-pump would upload a further image before a nearer one.
            self.pixels.give(self.pending.remove(i).image.rgba);
        }
    }

    pub fn contains(&self, index: usize) -> bool {
        self.cache.contains(index)
    }

    pub fn get(&self, index: usize) -> Option<&Entry> {
        self.cache.get(index)
    }

    pub fn len(&self) -> usize {
        self.cache.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }

    pub fn pooled(&self) -> usize {
        self.pool.len()
    }

    pub fn clear(&mut self) {
        let evicted = self.cache.clear();
        self.recycle(evicted);
    }

    pub fn retain_window_with(&mut self, centre: usize, radius: usize, keep: Option<usize>) {
        let evicted = self.cache.retain_window_with(centre, radius, keep);
        self.recycle(evicted);
    }

    /// Keep evicted textures for reuse, up to the pool limit.
    fn recycle(&mut self, evicted: Vec<Entry>) {
        for e in evicted {
            if self.pool.len() < self.pool_limit {
                self.pool.push(e.texture);
            }
            // Beyond the limit the texture drops here and the driver reclaims it.
        }
    }

    /// Take a pooled texture of exactly this size, if one is available.
    fn take_pooled(&mut self, width: u32, height: u32) -> Option<wgpu::Texture> {
        let i = self
            .pool
            .iter()
            .position(|t| t.width() == width && t.height() == height)?;
        Some(self.pool.swap_remove(i))
    }

    /// Start uploading an image. Rows are written later by [`TextureCache::pump`].
    ///
    /// A 25.6 MP texture is ~102 MB, and `write_texture` costs a CPU memcpy into a
    /// staging buffer -- roughly 14 ms, most of a 60 Hz frame. Doing that in one call
    /// makes the window stutter every time an image lands, so the transfer is split into
    /// bands and spread across frames under a time budget. The texture is not drawn until
    /// every band has been written.
    pub fn queue_upload(&mut self, device: &wgpu::Device, index: usize, image: Box<Image>) {
        // A zero-sized texture is invalid in wgpu, so refuse rather than panic.
        if image.width == 0 || image.height == 0 {
            log::warn!("refusing to upload zero-sized image at index {index}");
            return;
        }

        let size = wgpu::Extent3d {
            width: image.width,
            height: image.height,
            depth_or_array_layers: 1,
        };
        // Recycle an identically-sized texture when one is free, which is the common
        // case since every frame in a shoot has the same dimensions.
        let texture = match self.take_pooled(image.width, image.height) {
            Some(t) => t,
            None => device.create_texture(&wgpu::TextureDescriptor {
                label: Some("image"),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                // sRGB-aware: filtering then happens in linear light. See R10.
                format: wgpu::TextureFormat::Rgba8UnormSrgb,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            }),
        };

        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("image bind group"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.uniform.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });

        let evicted = self.cache.insert(
            index,
            Entry {
                texture,
                bind,
                stored: (image.width, image.height),
                orientation: Orientation::new(image.orientation),
                // Classified here, where the profile is still in hand: the ring hands
                // the pixels over exactly once, so this is the only chance to see it.
                colour: crate::icc::classify(image.icc.as_deref(), image.color_space),
                complete: false,
            },
        );
        self.recycle(evicted);

        // Replace any earlier pending upload for this index, recovering its buffer.
        self.retire(index);
        self.pending.push(Pending {
            index,
            image,
            row: 0,
        });
    }

    /// Write as many bands as fit in `budget`, nearest to `priority` first.
    ///
    /// Returns true when uploads remain, so the caller can schedule another frame.
    ///
    /// `priority` itself is exempt from the budget and is finished in one go. The budget
    /// exists to stop a background upload stealing a frame from a *visible* image, but
    /// while `priority` is incomplete nothing is drawn at all, so spreading its 14 ms
    /// over four vsync frames just means four frames of blank screen instead of one
    /// long one. Neighbours still pay the budget.
    pub fn pump(
        &mut self,
        queue: &wgpu::Queue,
        priority: usize,
        budget: std::time::Duration,
    ) -> bool {
        // Drop uploads whose entry has been evicted; the work is no longer wanted, but
        // the buffer still is.
        let evicted: Vec<usize> = self
            .pending
            .iter()
            .map(|p| p.index)
            .filter(|&i| !self.cache.contains(i))
            .collect();
        for i in evicted {
            self.retire(i);
        }
        if self.pending.is_empty() {
            return false;
        }
        // The image on screen finishes first; neighbours fill in behind it.
        self.pending
            .sort_by_key(|p| p.index.abs_diff(priority));

        let start = std::time::Instant::now();
        let mut wrote_any = false;
        while let Some(p) = self.pending.first() {
            // Keep going while the user is waiting on this one; otherwise respect the
            // frame budget, but always make at least one band of progress per frame so
            // a tiny budget cannot stall the queue entirely.
            if p.index != priority && wrote_any && start.elapsed() >= budget {
                break;
            }
            let p = &mut self.pending[0];
            let height = p.image.height;
            let width = p.image.width;
            let row_bytes = (width * 4) as usize;

            // Band sized so one write moves a few MB: small enough to stay responsive,
            // large enough that per-call overhead does not dominate.
            const BAND_BYTES: usize = 4 << 20;
            let band_rows = ((BAND_BYTES / row_bytes.max(1)) as u32)
                .clamp(1, height.saturating_sub(p.row).max(1));
            let rows = band_rows.min(height - p.row);

            let offset = p.row as usize * row_bytes;
            let end = offset + rows as usize * row_bytes;
            let Some(slice) = p.image.rgba.get(offset..end) else {
                // Buffer shorter than the declared dimensions: give up on this one
                // rather than risk a panic on malformed input.
                log::warn!("pixel buffer too short for index {}", p.index);
                let idx = p.index;
                self.retire(idx);
                continue;
            };

            let Some(entry) = self.cache.get(p.index) else {
                let idx = p.index;
                self.retire(idx);
                continue;
            };

            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &entry.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d {
                        x: 0,
                        y: p.row,
                        z: 0,
                    },
                    aspect: wgpu::TextureAspect::All,
                },
                slice,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(width * 4),
                    rows_per_image: Some(rows),
                },
                wgpu::Extent3d {
                    width,
                    height: rows,
                    depth_or_array_layers: 1,
                },
            );

            wrote_any = true;
            p.row += rows;
            if p.row >= height {
                let idx = p.index;
                self.retire(idx);
                if let Some(e) = self.cache.entries.get_mut(&idx) {
                    e.complete = true;
                }
            }
        }

        !self.pending.is_empty()
    }

    /// True when the texture for `index` is fully written and safe to draw.
    pub fn is_complete(&self, index: usize) -> bool {
        self.cache.get(index).is_some_and(|e| e.complete)
    }

    /// Number of uploads still in flight.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }
}

/// Everything needed to draw the image layer.
pub struct Renderer {
    pipeline: wgpu::RenderPipeline,
    uniform: wgpu::Buffer,
    pub cache: TextureCache,
}

impl Renderer {
    pub fn new(
        device: &wgpu::Device,
        target_format: wgpu::TextureFormat,
        cache_capacity: usize,
        pixels: std::sync::Arc<crate::decode::BufferPool>,
    ) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("image shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });

        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("image bind group layout"),
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

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("image pipeline layout"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("image pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                targets: &[Some(target_format.into())],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("image transform"),
            size: std::mem::size_of::<Transform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        // Linear filtering both ways: minification is the common case when fitting a
        // 25 MP image to the window, magnification matters when zoomed past 1:1.
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("image sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            // No mip chain is generated, so this only matters if one is added later.
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });

        Self {
            pipeline,
            cache: TextureCache::new(
                cache_capacity,
                layout,
                uniform.clone(),
                sampler,
                pixels,
            ),
            uniform,
        }
    }

    /// Draw the image at `index`, if its texture is resident.
    ///
    /// Returns false when there is nothing to draw, so the caller can show a
    /// placeholder instead.
    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &self,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        index: usize,
        view: &View,
        viewport: Viewport,
        // Physical-pixel rect of the image area: (x, y, width, height). Clip space is
        // mapped onto exactly this rect, so the aspect ratio matches `viewport`.
        rect: (f32, f32, f32, f32),
    ) -> bool {
        let Some(entry) = self.cache.get(index) else {
            return false;
        };
        // Unwritten rows hold undefined data, so wait for the bands to finish.
        if !entry.complete {
            return false;
        }

        let displayed = entry
            .orientation
            .displayed_size(entry.stored.0, entry.stored.1);
        let (sx, sy, tx, ty) = view.clip_transform(displayed, viewport);
        let xf = Transform {
            scale: [sx, sy],
            offset: [tx, ty],
            uv: entry.orientation.uv_matrix(),
        };
        queue.write_buffer(&self.uniform, 0, bytemuck::bytes_of(&xf));

        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("image pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    // Dark neutral surround, so the eye is not pulled off the photo.
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: 0.05,
                        g: 0.05,
                        b: 0.05,
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
        // Without this, clip space would cover the whole surface while the transform was
        // computed against the narrower image area, stretching the image horizontally.
        pass.set_viewport(rect.0, rect.1, rect.2, rect.3, 0.0, 1.0);
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &entry.bind, &[]);
        pass.draw(0..6, 0..1);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Eviction is tested on `Cache<usize>`; `upload` is the only part needing a device.
    fn cache_with(indices: &[usize], capacity: usize) -> Cache<usize> {
        let mut c = Cache::new(capacity);
        for &i in indices {
            c.insert(i, i);
        }
        c
    }

    /// Total values ever evicted, so tests can assert nothing is silently dropped.
    fn insert_all(c: &mut Cache<usize>, indices: &[usize]) -> Vec<usize> {
        let mut out = Vec::new();
        for &i in indices {
            out.extend(c.insert(i, i));
        }
        out
    }

    #[test]
    fn transform_is_pod_and_correctly_sized() {
        // Must match the WGSL struct: two vec2 then a vec4.
        assert_eq!(std::mem::size_of::<Transform>(), 32);
        assert_eq!(std::mem::align_of::<Transform>(), 4);
        let xf = Transform {
            scale: [1.0, 0.5],
            offset: [0.0, 0.0],
            uv: [1.0, 0.0, 0.0, 1.0],
        };
        // bytemuck must accept it, which is what the uniform write relies on.
        assert_eq!(bytemuck::bytes_of(&xf).len(), 32);
    }

    #[test]
    fn new_cache_is_empty() {
        let c: Cache<usize> = Cache::new(21);
        assert!(c.is_empty());
        assert_eq!(c.len(), 0);
        assert!(!c.contains(0));
        assert!(c.get(0).is_none());
    }

    #[test]
    fn capacity_is_at_least_one() {
        // A zero-capacity cache could never hold the image being displayed.
        assert_eq!(Cache::<usize>::new(0).capacity(), 1);
    }

    #[test]
    fn retain_window_drops_everything_outside() {
        let mut c = cache_with(&[0, 5, 9, 10, 11, 15, 100], 100);
        let evicted = c.retain_window_with(10, 2, None);
        assert_eq!(evicted.len(), 4, "evicted values must be returned for recycling");
        assert_eq!(c.len(), 3);
        for i in [9, 10, 11] {
            assert!(c.contains(i), "index {i} is inside the window");
        }
        for i in [0, 5, 15, 100] {
            assert!(!c.contains(i), "index {i} should have been evicted");
        }
    }

    #[test]
    fn retain_window_clamps_at_zero() {
        // A window around index 1 must not underflow.
        let mut c = cache_with(&[0, 1, 2, 8], 100);
        c.retain_window_with(1, 5, None);
        assert!(c.contains(0) && c.contains(1) && c.contains(2));
        assert!(!c.contains(8));
    }

    #[test]
    fn capacity_evicts_the_furthest_entry() {
        let mut c: Cache<usize> = Cache::new(3);
        for i in [10, 11, 12] {
            c.insert(i, i);
        }
        assert_eq!(c.len(), 3);
        // Inserting near 10 should evict 12, the furthest from the new index.
        c.insert(9, 9);
        assert_eq!(c.len(), 3);
        assert!(c.contains(9) && c.contains(10) && c.contains(11));
        assert!(!c.contains(12), "furthest entry should be evicted");
    }

    #[test]
    fn newly_inserted_entry_is_never_the_eviction_victim() {
        // Even inserting a distant index must keep that index, or the image the user
        // just navigated to would vanish immediately.
        let mut c: Cache<usize> = Cache::new(2);
        c.insert(0, 0);
        c.insert(1, 1);
        c.insert(500, 500);
        assert!(c.contains(500), "the just-inserted entry must survive");
        assert_eq!(c.len(), 2);
    }



    #[test]
    fn clear_empties_the_cache_and_returns_values() {
        let mut c = cache_with(&[1, 2, 3], 10);
        let out = c.clear();
        assert!(c.is_empty());
        assert_eq!(out.len(), 3, "cleared values must be returned for recycling");
    }

    #[test]
    fn retain_window_keeps_the_extra_index() {
        // The hovered row lives far outside the navigation window; without the
        // exemption its texture was evicted the instant after upload.
        let mut c = cache_with(&[9, 10, 11, 500], 100);
        c.retain_window_with(10, 1, Some(500));
        assert!(c.contains(500), "hovered index must survive eviction");
        assert_eq!(c.len(), 4);

        // And without the exemption it goes.
        c.retain_window_with(10, 1, None);
        assert!(!c.contains(500));
    }

    #[test]
    fn eviction_returns_every_displaced_value() {
        // Nothing may be dropped on the floor, or textures could not be recycled.
        let mut c: Cache<usize> = Cache::new(3);
        let evicted = insert_all(&mut c, &[1, 2, 3, 4, 5]);
        assert_eq!(c.len(), 3);
        assert_eq!(evicted.len(), 2, "two inserts must have displaced something");
    }

    #[test]
    fn replacing_an_index_returns_the_old_value() {
        let mut c: Cache<usize> = Cache::new(4);
        c.insert(7, 7);
        let evicted = c.insert(7, 70);
        assert_eq!(evicted, vec![7], "the replaced value must come back");
        assert_eq!(*c.get(7).unwrap(), 70);
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn capacity_of_one_still_works() {
        let mut c: Cache<usize> = Cache::new(1);
        c.insert(4, 4);
        c.insert(5, 5);
        assert_eq!(c.len(), 1);
        assert!(c.contains(5), "the newest insert wins");
    }
}
