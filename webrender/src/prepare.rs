/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! # Prepare pass
//!
//! TODO: document this!

use std::{cmp, u32, usize};
use api::{PremultipliedColorF, PropertyBinding, GradientStop, ExtendMode};
use api::{BoxShadowClipMode, LineOrientation, BorderStyle, ClipMode};
use api::image_tiling::{self, Repetition};
use api::units::*;
use euclid::Scale;
use euclid::approxeq::ApproxEq;
use smallvec::SmallVec;
use crate::border::{get_max_scale_for_border, build_border_instances};
use crate::clip::{ClipStore};
use crate::spatial_tree::{ROOT_SPATIAL_NODE_INDEX, SpatialNodeIndex};
use crate::clip::{ClipDataStore, ClipNodeFlags, ClipChainInstance, ClipItemKind};
use crate::frame_builder::{FrameBuildingContext, FrameBuildingState, PictureContext, PictureState};
use crate::gpu_cache::{GpuCacheHandle, GpuDataRequest};
use crate::gpu_types::{BrushFlags};
use crate::internal_types::PlaneSplitAnchor;
use crate::picture::{PicturePrimitive, TileCacheLogger};
use crate::picture::{PrimitiveList, SurfaceIndex};
use crate::prim_store::gradient::{GRADIENT_FP_STOPS, GradientCacheKey, GradientStopKey};
use crate::prim_store::gradient::LinearGradientPrimitive;
use crate::prim_store::line_dec::MAX_LINE_DECORATION_RESOLUTION;
use crate::prim_store::*;
use crate::render_backend::DataStores;
use crate::render_task_cache::{RenderTaskCacheKeyKind, RenderTaskCacheEntryHandle, RenderTaskCacheKey, to_cache_size};
use crate::render_task::RenderTask;
use crate::segment::SegmentBuilder;
use crate::texture_cache::TEXTURE_REGION_DIMENSIONS;
use crate::util::{clamp_to_scale_factor, pack_as_float, raster_rect_to_device_pixels};
use crate::visibility::{PrimitiveVisibility, PrimitiveVisibilityIndex, compute_conservative_visible_rect};


const MAX_MASK_SIZE: f32 = 4096.0;

const MIN_BRUSH_SPLIT_AREA: f32 = 128.0 * 128.0;


pub fn prepare_primitives(
    store: &mut PrimitiveStore,
    prim_list: &mut PrimitiveList,
    pic_context: &PictureContext,
    pic_state: &mut PictureState,
    frame_context: &FrameBuildingContext,
    frame_state: &mut FrameBuildingState,
    data_stores: &mut DataStores,
    scratch: &mut PrimitiveScratchBuffer,
    tile_cache_log: &mut TileCacheLogger,
) {
    profile_scope!("prepare_primitives");
    for (cluster_index, cluster) in prim_list.clusters.iter_mut().enumerate() {
        profile_scope!("cluster");
        pic_state.map_local_to_pic.set_target_spatial_node(
            cluster.spatial_node_index,
            frame_context.spatial_tree,
        );

        for (idx, prim_instance) in (&mut prim_list.prim_instances[cluster.prim_range()]).iter_mut().enumerate() {
            let prim_instance_index = cluster.prim_range.start + idx;
            if prim_instance.visibility_info == PrimitiveVisibilityIndex::INVALID {
                continue;
            }

            // The original clipped world rect was calculated during the initial visibility pass.
            // However, it's possible that the dirty rect has got smaller, if tiles were not
            // dirty. Intersecting with the dirty rect here eliminates preparing any primitives
            // outside the dirty rect, and reduces the size of any off-screen surface allocations
            // for clip masks / render tasks that we make.
            {
                let visibility_info = &mut scratch.prim_info[prim_instance.visibility_info.0 as usize];
                let dirty_region = frame_state.current_dirty_region();

                for dirty_region in &dirty_region.dirty_rects {
                    if visibility_info.clipped_world_rect.intersects(&dirty_region.world_rect) {
                        visibility_info.visibility_mask.include(dirty_region.visibility_mask);
                    }
                }

                if visibility_info.visibility_mask.is_empty() {
                    prim_instance.visibility_info = PrimitiveVisibilityIndex::INVALID;
                    continue;
                }
            }

            let plane_split_anchor = PlaneSplitAnchor::new(cluster_index, prim_instance_index);

            if prepare_prim_for_render(
                store,
                prim_instance,
                cluster.spatial_node_index,
                pic_context,
                pic_state,
                frame_context,
                frame_state,
                plane_split_anchor,
                data_stores,
                scratch,
                tile_cache_log,
            ) {
                frame_state.profile_counters.visible_primitives.inc();
            }
        }
    }
}

fn prepare_prim_for_render(
    store: &mut PrimitiveStore,
    prim_instance: &mut PrimitiveInstance,
    prim_spatial_node_index: SpatialNodeIndex,
    pic_context: &PictureContext,
    pic_state: &mut PictureState,
    frame_context: &FrameBuildingContext,
    frame_state: &mut FrameBuildingState,
    plane_split_anchor: PlaneSplitAnchor,
    data_stores: &mut DataStores,
    scratch: &mut PrimitiveScratchBuffer,
    tile_cache_log: &mut TileCacheLogger,
) -> bool {
    profile_scope!("prepare_prim_for_render");
    // If we have dependencies, we need to prepare them first, in order
    // to know the actual rect of this primitive.
    // For example, scrolling may affect the location of an item in
    // local space, which may force us to render this item on a larger
    // picture target, if being composited.
    let pic_info = {
        match prim_instance.kind {
            PrimitiveInstanceKind::Picture { pic_index ,.. } => {
                let pic = &mut store.pictures[pic_index.0];

                let clipped_prim_bounding_rect = scratch
                    .prim_info[prim_instance.visibility_info.0 as usize]
                    .clipped_world_rect;

                match pic.take_context(
                    pic_index,
                    clipped_prim_bounding_rect,
                    pic_context.surface_spatial_node_index,
                    pic_context.raster_spatial_node_index,
                    pic_context.surface_index,
                    &pic_context.subpixel_mode,
                    frame_state,
                    frame_context,
                    scratch,
                    tile_cache_log,
                ) {
                    Some(info) => Some(info),
                    None => {
                        if prim_instance.is_chased() {
                            println!("\tculled for carrying an invisible composite filter");
                        }

                        prim_instance.visibility_info = PrimitiveVisibilityIndex::INVALID;

                        return false;
                    }
                }
            }
            PrimitiveInstanceKind::TextRun { .. } |
            PrimitiveInstanceKind::Rectangle { .. } |
            PrimitiveInstanceKind::LineDecoration { .. } |
            PrimitiveInstanceKind::NormalBorder { .. } |
            PrimitiveInstanceKind::ImageBorder { .. } |
            PrimitiveInstanceKind::YuvImage { .. } |
            PrimitiveInstanceKind::Image { .. } |
            PrimitiveInstanceKind::LinearGradient { .. } |
            PrimitiveInstanceKind::RadialGradient { .. } |
            PrimitiveInstanceKind::ConicGradient { .. } |
            PrimitiveInstanceKind::Clear { .. } |
            PrimitiveInstanceKind::Backdrop { .. } => {
                None
            }
        }
    };

    let is_passthrough = match pic_info {
        Some((pic_context_for_children, mut pic_state_for_children, mut prim_list)) => {
            let is_passthrough = pic_context_for_children.is_passthrough;

            prepare_primitives(
                store,
                &mut prim_list,
                &pic_context_for_children,
                &mut pic_state_for_children,
                frame_context,
                frame_state,
                data_stores,
                scratch,
                tile_cache_log,
            );

            // Restore the dependencies (borrow check dance)
            store.pictures[pic_context_for_children.pic_index.0]
                .restore_context(
                    pic_context.surface_index,
                    prim_list,
                    pic_context_for_children,
                    pic_state_for_children,
                    frame_state,
                );

            is_passthrough
        }
        None => {
            false
        }
    };

    let prim_rect = data_stores.get_local_prim_rect(
        prim_instance,
        store,
    );

    if !is_passthrough {
        update_clip_task(
            prim_instance,
            &prim_rect.origin,
            prim_spatial_node_index,
            pic_context.raster_spatial_node_index,
            pic_context,
            pic_state,
            frame_context,
            frame_state,
            store,
            data_stores,
            scratch,
        );

        if prim_instance.is_chased() {
            println!("\tconsidered visible and ready with local pos {:?}", prim_rect.origin);
        }
    }

    #[cfg(debug_assertions)]
    {
        prim_instance.prepared_frame_id = frame_state.render_tasks.frame_id();
    }

    prepare_interned_prim_for_render(
        store,
        prim_instance,
        prim_spatial_node_index,
        plane_split_anchor,
        pic_context,
        pic_state,
        frame_context,
        frame_state,
        data_stores,
        scratch,
    );

    true
}

/// Prepare an interned primitive for rendering, by requesting
/// resources, render tasks etc. This is equivalent to the
/// prepare_prim_for_render_inner call for old style primitives.
fn prepare_interned_prim_for_render(
    store: &mut PrimitiveStore,
    prim_instance: &mut PrimitiveInstance,
    prim_spatial_node_index: SpatialNodeIndex,
    plane_split_anchor: PlaneSplitAnchor,
    pic_context: &PictureContext,
    pic_state: &mut PictureState,
    frame_context: &FrameBuildingContext,
    frame_state: &mut FrameBuildingState,
    data_stores: &mut DataStores,
    scratch: &mut PrimitiveScratchBuffer,
) {
    let is_chased = prim_instance.is_chased();
    let device_pixel_scale = frame_state.surfaces[pic_context.surface_index.0].device_pixel_scale;

    match &mut prim_instance.kind {
        PrimitiveInstanceKind::LineDecoration { data_handle, ref mut cache_handle, .. } => {
            profile_scope!("LineDecoration");
            let prim_data = &mut data_stores.line_decoration[*data_handle];
            let common_data = &mut prim_data.common;
            let line_dec_data = &mut prim_data.kind;

            // Update the template this instane references, which may refresh the GPU
            // cache with any shared template data.
            line_dec_data.update(common_data, frame_state);

            // Work out the device pixel size to be used to cache this line decoration.
            if is_chased {
                println!("\tline decoration key={:?}", line_dec_data.cache_key);
            }

            // If we have a cache key, it's a wavy / dashed / dotted line. Otherwise, it's
            // a simple solid line.
            if let Some(cache_key) = line_dec_data.cache_key.as_ref() {
                // TODO(gw): Do we ever need / want to support scales for text decorations
                //           based on the current transform?
                let scale_factor = Scale::new(1.0) * device_pixel_scale;
                let mut task_size = (LayoutSize::from_au(cache_key.size) * scale_factor).ceil().to_i32();
                if task_size.width > MAX_LINE_DECORATION_RESOLUTION as i32 ||
                   task_size.height > MAX_LINE_DECORATION_RESOLUTION as i32 {
                     let max_extent = cmp::max(task_size.width, task_size.height);
                     let task_scale_factor = Scale::new(MAX_LINE_DECORATION_RESOLUTION as f32 / max_extent as f32);
                     task_size = (LayoutSize::from_au(cache_key.size) * scale_factor * task_scale_factor)
                                    .ceil().to_i32();
                }

                // Request a pre-rendered image task.
                // TODO(gw): This match is a bit untidy, but it should disappear completely
                //           once the prepare_prims and batching are unified. When that
                //           happens, we can use the cache handle immediately, and not need
                //           to temporarily store it in the primitive instance.
                *cache_handle = Some(frame_state.resource_cache.request_render_task(
                    RenderTaskCacheKey {
                        size: task_size,
                        kind: RenderTaskCacheKeyKind::LineDecoration(cache_key.clone()),
                    },
                    frame_state.gpu_cache,
                    frame_state.render_tasks,
                    None,
                    false,
                    |render_tasks| {
                        render_tasks.add().init(RenderTask::new_line_decoration(
                            task_size,
                            cache_key.style,
                            cache_key.orientation,
                            cache_key.wavy_line_thickness.to_f32_px(),
                            LayoutSize::from_au(cache_key.size),
                        ))
                    }
                ));
            }
        }
        PrimitiveInstanceKind::TextRun { run_index, data_handle, .. } => {
            profile_scope!("TextRun");
            let prim_data = &mut data_stores.text_run[*data_handle];
            let run = &mut store.text_runs[*run_index];

            prim_data.common.may_need_repetition = false;

            // The glyph transform has to match `glyph_transform` in "ps_text_run" shader.
            // It's relative to the rasterizing space of a glyph.
            let transform = frame_context.spatial_tree
                .get_relative_transform(
                    prim_spatial_node_index,
                    pic_context.raster_spatial_node_index,
                )
                .into_fast_transform();
            let prim_offset = prim_data.common.prim_rect.origin.to_vector() - run.reference_frame_relative_offset;

            let pic = &store.pictures[pic_context.pic_index.0];
            let raster_space = pic.get_raster_space(frame_context.spatial_tree);
            let surface = &frame_state.surfaces[pic_context.surface_index.0];
            let prim_info = &scratch.prim_info[prim_instance.visibility_info.0 as usize];
            let root_scaling_factor = match pic.raster_config {
                Some(ref raster_config) => raster_config.root_scaling_factor,
                None => 1.0
            };

            run.request_resources(
                prim_offset,
                prim_info.clip_chain.pic_clip_rect,
                &prim_data.font,
                &prim_data.glyphs,
                &transform.to_transform().with_destination::<_>(),
                surface,
                prim_spatial_node_index,
                raster_space,
                root_scaling_factor,
                &pic_context.subpixel_mode,
                frame_state.resource_cache,
                frame_state.gpu_cache,
                frame_state.render_tasks,
                frame_context.spatial_tree,
                scratch,
            );

            // Update the template this instane references, which may refresh the GPU
            // cache with any shared template data.
            prim_data.update(frame_state);
        }
        PrimitiveInstanceKind::Clear { data_handle, .. } => {
            profile_scope!("Clear");
            let prim_data = &mut data_stores.prim[*data_handle];

            prim_data.common.may_need_repetition = false;

            // Update the template this instane references, which may refresh the GPU
            // cache with any shared template data.
            prim_data.update(frame_state, frame_context.scene_properties);
        }
        PrimitiveInstanceKind::NormalBorder { data_handle, ref mut cache_handles, .. } => {
            profile_scope!("NormalBorder");
            let prim_data = &mut data_stores.normal_border[*data_handle];
            let common_data = &mut prim_data.common;
            let border_data = &mut prim_data.kind;

            common_data.may_need_repetition =
                matches!(border_data.border.top.style, BorderStyle::Dotted | BorderStyle::Dashed) ||
                matches!(border_data.border.right.style, BorderStyle::Dotted | BorderStyle::Dashed) ||
                matches!(border_data.border.bottom.style, BorderStyle::Dotted | BorderStyle::Dashed) ||
                matches!(border_data.border.left.style, BorderStyle::Dotted | BorderStyle::Dashed);


            // Update the template this instance references, which may refresh the GPU
            // cache with any shared template data.
            border_data.update(common_data, frame_state);

            // TODO(gw): For now, the scale factors to rasterize borders at are
            //           based on the true world transform of the primitive. When
            //           raster roots with local scale are supported in future,
            //           that will need to be accounted for here.
            let scale = frame_context
                .spatial_tree
                .get_world_transform(prim_spatial_node_index)
                .scale_factors();

            // Scale factors are normalized to a power of 2 to reduce the number of
            // resolution changes.
            // For frames with a changing scale transform round scale factors up to
            // nearest power-of-2 boundary so that we don't keep having to redraw
            // the content as it scales up and down. Rounding up to nearest
            // power-of-2 boundary ensures we never scale up, only down --- avoiding
            // jaggies. It also ensures we never scale down by more than a factor of
            // 2, avoiding bad downscaling quality.
            let scale_width = clamp_to_scale_factor(scale.0, false);
            let scale_height = clamp_to_scale_factor(scale.1, false);
            // Pick the maximum dimension as scale
            let world_scale = LayoutToWorldScale::new(scale_width.max(scale_height));
            let mut scale = world_scale * device_pixel_scale;
            let max_scale = get_max_scale_for_border(border_data);
            scale.0 = scale.0.min(max_scale.0);

            // For each edge and corner, request the render task by content key
            // from the render task cache. This ensures that the render task for
            // this segment will be available for batching later in the frame.
            let mut handles: SmallVec<[RenderTaskCacheEntryHandle; 8]> = SmallVec::new();

            for segment in &border_data.border_segments {
                // Update the cache key device size based on requested scale.
                let cache_size = to_cache_size(segment.local_task_size, &mut scale);
                let cache_key = RenderTaskCacheKey {
                    kind: RenderTaskCacheKeyKind::BorderSegment(segment.cache_key.clone()),
                    size: cache_size,
                };

                handles.push(frame_state.resource_cache.request_render_task(
                    cache_key,
                    frame_state.gpu_cache,
                    frame_state.render_tasks,
                    None,
                    false,          // TODO(gw): We don't calculate opacity for borders yet!
                    |render_tasks| {
                        render_tasks.add().init(RenderTask::new_border_segment(
                            cache_size,
                            build_border_instances(
                                &segment.cache_key,
                                cache_size,
                                &border_data.border,
                                scale,
                            ),
                        ))
                    }
                ));
            }

            *cache_handles = scratch
                .border_cache_handles
                .extend(handles);
        }
        PrimitiveInstanceKind::ImageBorder { data_handle, .. } => {
            profile_scope!("ImageBorder");
            let prim_data = &mut data_stores.image_border[*data_handle];

            // TODO: get access to the ninepatch and to check whwther we need support
            // for repetitions in the shader.

            // Update the template this instane references, which may refresh the GPU
            // cache with any shared template data.
            prim_data.kind.update(&mut prim_data.common, frame_state);
        }
        PrimitiveInstanceKind::Rectangle { data_handle, segment_instance_index, color_binding_index, .. } => {
            profile_scope!("Rectangle");
            let prim_data = &mut data_stores.prim[*data_handle];
            prim_data.common.may_need_repetition = false;

            if *color_binding_index != ColorBindingIndex::INVALID {
                match store.color_bindings[*color_binding_index] {
                    PropertyBinding::Binding(..) => {
                        // We explicitly invalidate the gpu cache
                        // if the color is animating.
                        let gpu_cache_handle =
                            if *segment_instance_index == SegmentInstanceIndex::INVALID {
                                None
                            } else if *segment_instance_index == SegmentInstanceIndex::UNUSED {
                                Some(&prim_data.common.gpu_cache_handle)
                            } else {
                                Some(&scratch.segment_instances[*segment_instance_index].gpu_cache_handle)
                            };
                        if let Some(gpu_cache_handle) = gpu_cache_handle {
                            frame_state.gpu_cache.invalidate(gpu_cache_handle);
                        }
                    }
                    PropertyBinding::Value(..) => {},
                }
            }

            // Update the template this instane references, which may refresh the GPU
            // cache with any shared template data.
            prim_data.update(
                frame_state,
                frame_context.scene_properties,
            );

            write_segment(
                *segment_instance_index,
                frame_state,
                &mut scratch.segments,
                &mut scratch.segment_instances,
                |request| {
                    prim_data.kind.write_prim_gpu_blocks(
                        request,
                        frame_context.scene_properties,
                    );
                }
            );
        }
        PrimitiveInstanceKind::YuvImage { data_handle, segment_instance_index, .. } => {
            profile_scope!("YuvImage");
            let prim_data = &mut data_stores.yuv_image[*data_handle];
            let common_data = &mut prim_data.common;
            let yuv_image_data = &mut prim_data.kind;

            common_data.may_need_repetition = false;

            // Update the template this instane references, which may refresh the GPU
            // cache with any shared template data.
            yuv_image_data.update(common_data, frame_state);

            write_segment(
                *segment_instance_index,
                frame_state,
                &mut scratch.segments,
                &mut scratch.segment_instances,
                |request| {
                    yuv_image_data.write_prim_gpu_blocks(request);
                }
            );
        }
        PrimitiveInstanceKind::Image { data_handle, image_instance_index, .. } => {
            profile_scope!("Image");
            let prim_data = &mut data_stores.image[*data_handle];
            let common_data = &mut prim_data.common;
            let image_data = &mut prim_data.kind;

            if image_data.stretch_size.width >= common_data.prim_rect.size.width &&
                image_data.stretch_size.height >= common_data.prim_rect.size.height {

                common_data.may_need_repetition = false;
            }

            // Update the template this instane references, which may refresh the GPU
            // cache with any shared template data.
            image_data.update(common_data, frame_state);

            let image_instance = &mut store.images[*image_instance_index];

            write_segment(
                image_instance.segment_instance_index,
                frame_state,
                &mut scratch.segments,
                &mut scratch.segment_instances,
                |request| {
                    image_data.write_prim_gpu_blocks(request);
                },
            );
        }
        PrimitiveInstanceKind::LinearGradient { data_handle, gradient_index, .. } => {
            profile_scope!("LinearGradient");
            let prim_data = &mut data_stores.linear_grad[*data_handle];
            let gradient = &mut store.linear_gradients[*gradient_index];

            // Update the template this instane references, which may refresh the GPU
            // cache with any shared template data.
            prim_data.update(frame_state);

            if prim_data.stretch_size.width >= prim_data.common.prim_rect.size.width &&
                prim_data.stretch_size.height >= prim_data.common.prim_rect.size.height {

                prim_data.common.may_need_repetition = false;
            }

            if prim_data.supports_caching {
                let gradient_size = (prim_data.end_point - prim_data.start_point).to_size();

                // Calculate what the range of the gradient is that covers this
                // primitive. These values are included in the cache key. The
                // size of the gradient task is the length of a texture cache
                // region, for maximum accuracy, and a minimal size on the
                // axis that doesn't matter.
                let (size, orientation, prim_start_offset, prim_end_offset) =
                    if prim_data.start_point.x.approx_eq(&prim_data.end_point.x) {
                        let prim_start_offset = -prim_data.start_point.y / gradient_size.height;
                        let prim_end_offset = (prim_data.common.prim_rect.size.height - prim_data.start_point.y)
                                                / gradient_size.height;
                        let size = DeviceIntSize::new(16, TEXTURE_REGION_DIMENSIONS);
                        (size, LineOrientation::Vertical, prim_start_offset, prim_end_offset)
                    } else {
                        let prim_start_offset = -prim_data.start_point.x / gradient_size.width;
                        let prim_end_offset = (prim_data.common.prim_rect.size.width - prim_data.start_point.x)
                                                / gradient_size.width;
                        let size = DeviceIntSize::new(TEXTURE_REGION_DIMENSIONS, 16);
                        (size, LineOrientation::Horizontal, prim_start_offset, prim_end_offset)
                    };

                // Build the cache key, including information about the stops.
                let mut stops = vec![GradientStopKey::empty(); prim_data.stops.len()];

                // Reverse the stops as required, same as the gradient builder does
                // for the slow path.
                if prim_data.reverse_stops {
                    for (src, dest) in prim_data.stops.iter().rev().zip(stops.iter_mut()) {
                        let stop = GradientStop {
                            offset: 1.0 - src.offset,
                            color: src.color,
                        };
                        *dest = stop.into();
                    }
                } else {
                    for (src, dest) in prim_data.stops.iter().zip(stops.iter_mut()) {
                        *dest = (*src).into();
                    }
                }

                gradient.cache_segments.clear();

                // emit render task caches and image rectangles to draw a gradient
                // with offsets from start_offset to end_offset.
                //
                // the primitive is covered by a gradient that ranges from
                // prim_start_offset to prim_end_offset.
                //
                // when clamping, these two pairs of offsets will always be the same.
                // when repeating, however, we march across the primitive, blitting
                // copies of the gradient along the way.  each copy has a range from
                // 0.0 to 1.0 (assuming it's fully visible), but where it appears on
                // the primitive changes as we go.  this position is also expressed
                // as an offset: gradient_offset_base.  that is, in terms of stops,
                // we draw a gradient from start_offset to end_offset.  its actual
                // location on the primitive is at start_offset + gradient_offset_base.
                //
                // either way, we need a while-loop to draw the gradient as well
                // because it might have more than 4 stops (the maximum of a cached
                // segment) and/or hard stops. so we have a walk-within-the-walk from
                // start_offset to end_offset caching up to GRADIENT_FP_STOPS stops at a
                // time.
                fn emit_segments(start_offset: f32, // start and end offset together are
                                 end_offset: f32,   // always a subrange of 0..1
                                 gradient_offset_base: f32,
                                 prim_start_offset: f32, // the offsets of the entire gradient as it
                                 prim_end_offset: f32,   // covers the entire primitive.
                                 prim_origin_in: LayoutPoint,
                                 prim_size_in: LayoutSize,
                                 task_size: DeviceIntSize,
                                 is_opaque: bool,
                                 stops: &[GradientStopKey],
                                 orientation: LineOrientation,
                                 frame_state: &mut FrameBuildingState,
                                 gradient: &mut LinearGradientPrimitive)
                {
                    // these prints are used to generate documentation examples, so
                    // leaving them in but commented out:
                    //println!("emit_segments call:");
                    //println!("\tstart_offset: {}, end_offset: {}", start_offset, end_offset);
                    //println!("\tprim_start_offset: {}, prim_end_offset: {}", prim_start_offset, prim_end_offset);
                    //println!("\tgradient_offset_base: {}", gradient_offset_base);
                    let mut first_stop = 0;
                    // look for an inclusive range of stops [first_stop, last_stop].
                    // once first_stop points at (or past) the last stop, we're done.
                    while first_stop < stops.len()-1 {

                        // if the entire sub-gradient starts at an offset that's past the
                        // segment's end offset, we're done.
                        if stops[first_stop].offset > end_offset {
                            return;
                        }

                        // accumulate stops until we have GRADIENT_FP_STOPS of them, or we hit
                        // a hard stop:
                        let mut last_stop = first_stop;
                        let mut hard_stop = false;   // did we stop on a hard stop?
                        while last_stop < stops.len()-1 &&
                              last_stop - first_stop + 1 < GRADIENT_FP_STOPS
                        {
                            if stops[last_stop+1].offset == stops[last_stop].offset {
                                hard_stop = true;
                                break;
                            }

                            last_stop = last_stop + 1;
                        }

                        let num_stops = last_stop - first_stop + 1;

                        // repeated hard stops at the same offset, skip
                        if num_stops == 0 {
                            first_stop = last_stop + 1;
                            continue;
                        }

                        // if the last_stop offset is before start_offset, the segment's not visible:
                        if stops[last_stop].offset < start_offset {
                            first_stop = if hard_stop { last_stop+1 } else { last_stop };
                            continue;
                        }

                        let segment_start_point = start_offset.max(stops[first_stop].offset);
                        let segment_end_point   = end_offset  .min(stops[last_stop ].offset);

                        let mut segment_stops = [GradientStopKey::empty(); GRADIENT_FP_STOPS];
                        for i in 0..num_stops {
                            segment_stops[i] = stops[first_stop + i];
                        }

                        let cache_key = GradientCacheKey {
                            orientation,
                            start_stop_point: VectorKey {
                                x: segment_start_point,
                                y: segment_end_point,
                            },
                            stops: segment_stops,
                        };

                        let mut prim_origin = prim_origin_in;
                        let mut prim_size   = prim_size_in;

                        // the primitive is covered by a segment from overall_start to
                        // overall_end; scale and shift based on the length of the actual
                        // segment that we're drawing:
                        let inv_length = 1.0 / ( prim_end_offset - prim_start_offset );
                        if orientation == LineOrientation::Horizontal {
                            prim_origin.x    += ( segment_start_point + gradient_offset_base - prim_start_offset )
                                                * inv_length * prim_size.width;
                            prim_size.width  *= ( segment_end_point - segment_start_point )
                                                * inv_length; // 2 gradient_offset_bases cancel out
                        } else {
                            prim_origin.y    += ( segment_start_point + gradient_offset_base - prim_start_offset )
                                                * inv_length * prim_size.height;
                            prim_size.height *= ( segment_end_point - segment_start_point )
                                                * inv_length; // 2 gradient_offset_bases cancel out
                        }

                        // <= 0 can happen if a hardstop lands exactly on an edge
                        if prim_size.area() > 0.0 {
                            let local_rect = LayoutRect::new( prim_origin, prim_size );

                            // documentation example traces:
                            //println!("\t\tcaching from offset {} to {}", segment_start_point, segment_end_point);
                            //println!("\t\tand blitting to {:?}", local_rect);

                            // Request the render task each frame.
                            gradient.cache_segments.push(
                                CachedGradientSegment {
                                    handle: frame_state.resource_cache.request_render_task(
                                        RenderTaskCacheKey {
                                            size: task_size,
                                            kind: RenderTaskCacheKeyKind::Gradient(cache_key),
                                        },
                                        frame_state.gpu_cache,
                                        frame_state.render_tasks,
                                        None,
                                        is_opaque,
                                        |render_tasks| {
                                            render_tasks.add().init(RenderTask::new_gradient(
                                                task_size,
                                                segment_stops,
                                                orientation,
                                                segment_start_point,
                                                segment_end_point,
                                            ))
                                        }),
                                    local_rect: local_rect,
                                }
                            );
                        }

                        // if ending on a hardstop, skip past it for the start of the next run:
                        first_stop = if hard_stop { last_stop + 1 } else { last_stop };
                    }
                }

                if prim_data.extend_mode == ExtendMode::Clamp ||
                   ( prim_start_offset >= 0.0 && prim_end_offset <= 1.0 )  // repeat doesn't matter
                {
                    // To support clamping, we need to make sure that quads are emitted for the
                    // segments before and after the 0.0...1.0 range of offsets.  emit_segments
                    // can handle that by duplicating the first and last point if necessary:
                    if prim_start_offset < 0.0 {
                        stops.insert(0, GradientStopKey {
                            offset: prim_start_offset,
                            color : stops[0].color
                        });
                    }

                    if prim_end_offset > 1.0 {
                        stops.push( GradientStopKey {
                            offset: prim_end_offset,
                            color : stops[stops.len()-1].color
                        });
                    }

                    emit_segments(prim_start_offset, prim_end_offset,
                                  0.0,
                                  prim_start_offset, prim_end_offset,
                                  prim_data.common.prim_rect.origin,
                                  prim_data.common.prim_rect.size,
                                  size,
                                  prim_data.stops_opacity.is_opaque,
                                  &stops,
                                  orientation,
                                  frame_state,
                                  gradient);
                }
                else
                {
                    let mut segment_start_point = prim_start_offset;
                    while segment_start_point < prim_end_offset {

                        // gradient stops are expressed in the range 0.0 ... 1.0, so to blit
                        // a copy of the gradient, snap to the integer just before the offset
                        // we want ...
                        let gradient_offset_base = segment_start_point.floor();
                        // .. and then draw from a start offset in range 0 to 1 ...
                        let repeat_start = segment_start_point - gradient_offset_base;
                        // .. up to the next integer, but clamped to the primitive's real
                        // end offset:
                        let repeat_end = (gradient_offset_base + 1.0).min(prim_end_offset) - gradient_offset_base;

                        emit_segments(repeat_start, repeat_end,
                                      gradient_offset_base,
                                      prim_start_offset, prim_end_offset,
                                      prim_data.common.prim_rect.origin,
                                      prim_data.common.prim_rect.size,
                                      size,
                                      prim_data.stops_opacity.is_opaque,
                                      &stops,
                                      orientation,
                                      frame_state,
                                      gradient);

                        segment_start_point = repeat_end + gradient_offset_base;
                    }
                }
            }

            if prim_data.tile_spacing != LayoutSize::zero() {
                // We are performing the decomposition on the CPU here, no need to
                // have it in the shader.
                prim_data.common.may_need_repetition = false;

                let prim_info = &scratch.prim_info[prim_instance.visibility_info.0 as usize];

                let map_local_to_world = SpaceMapper::new_with_target(
                    ROOT_SPATIAL_NODE_INDEX,
                    prim_spatial_node_index,
                    frame_context.global_screen_world_rect,
                    frame_context.spatial_tree,
                );

                gradient.visible_tiles_range = decompose_repeated_primitive(
                    &prim_info.combined_local_clip_rect,
                    &prim_data.common.prim_rect,
                    prim_info.clipped_world_rect,
                    &prim_data.stretch_size,
                    &prim_data.tile_spacing,
                    frame_state,
                    &mut scratch.gradient_tiles,
                    &map_local_to_world,
                    &mut |_, mut request| {
                        request.push([
                            prim_data.start_point.x,
                            prim_data.start_point.y,
                            prim_data.end_point.x,
                            prim_data.end_point.y,
                        ]);
                        request.push([
                            pack_as_float(prim_data.extend_mode as u32),
                            prim_data.stretch_size.width,
                            prim_data.stretch_size.height,
                            0.0,
                        ]);
                    }
                );

                if gradient.visible_tiles_range.is_empty() {
                    prim_instance.visibility_info = PrimitiveVisibilityIndex::INVALID;
                }
            }

            // TODO(gw): Consider whether it's worth doing segment building
            //           for gradient primitives.
        }
        PrimitiveInstanceKind::RadialGradient { data_handle, ref mut visible_tiles_range, .. } => {
            profile_scope!("RadialGradient");
            let prim_data = &mut data_stores.radial_grad[*data_handle];

            if prim_data.stretch_size.width >= prim_data.common.prim_rect.size.width &&
                prim_data.stretch_size.height >= prim_data.common.prim_rect.size.height {

                // We are performing the decomposition on the CPU here, no need to
                // have it in the shader.
                prim_data.common.may_need_repetition = false;
            }

            // Update the template this instane references, which may refresh the GPU
            // cache with any shared template data.
            prim_data.update(frame_state);

            if prim_data.tile_spacing != LayoutSize::zero() {
                let prim_info = &scratch.prim_info[prim_instance.visibility_info.0 as usize];

                let map_local_to_world = SpaceMapper::new_with_target(
                    ROOT_SPATIAL_NODE_INDEX,
                    prim_spatial_node_index,
                    frame_context.global_screen_world_rect,
                    frame_context.spatial_tree,
                );

                prim_data.common.may_need_repetition = false;

                *visible_tiles_range = decompose_repeated_primitive(
                    &prim_info.combined_local_clip_rect,
                    &prim_data.common.prim_rect,
                    prim_info.clipped_world_rect,
                    &prim_data.stretch_size,
                    &prim_data.tile_spacing,
                    frame_state,
                    &mut scratch.gradient_tiles,
                    &map_local_to_world,
                    &mut |_, mut request| {
                        request.push([
                            prim_data.center.x,
                            prim_data.center.y,
                            prim_data.params.start_radius,
                            prim_data.params.end_radius,
                        ]);
                        request.push([
                            prim_data.params.ratio_xy,
                            pack_as_float(prim_data.extend_mode as u32),
                            prim_data.stretch_size.width,
                            prim_data.stretch_size.height,
                        ]);
                    },
                );

                if visible_tiles_range.is_empty() {
                    prim_instance.visibility_info = PrimitiveVisibilityIndex::INVALID;
                }
            }

            // TODO(gw): Consider whether it's worth doing segment building
            //           for gradient primitives.
        }
        PrimitiveInstanceKind::ConicGradient { data_handle, ref mut visible_tiles_range, .. } => {
            profile_scope!("ConicGradient");
            let prim_data = &mut data_stores.conic_grad[*data_handle];

            if prim_data.stretch_size.width >= prim_data.common.prim_rect.size.width &&
                prim_data.stretch_size.height >= prim_data.common.prim_rect.size.height {

                // We are performing the decomposition on the CPU here, no need to
                // have it in the shader.
                prim_data.common.may_need_repetition = false;
            }

            // Update the template this instane references, which may refresh the GPU
            // cache with any shared template data.
            prim_data.update(frame_state);

            if prim_data.tile_spacing != LayoutSize::zero() {
                let prim_info = &scratch.prim_info[prim_instance.visibility_info.0 as usize];

                let map_local_to_world = SpaceMapper::new_with_target(
                    ROOT_SPATIAL_NODE_INDEX,
                    prim_spatial_node_index,
                    frame_context.global_screen_world_rect,
                    frame_context.spatial_tree,
                );

                prim_data.common.may_need_repetition = false;

                *visible_tiles_range = decompose_repeated_primitive(
                    &prim_info.combined_local_clip_rect,
                    &prim_data.common.prim_rect,
                    prim_info.clipped_world_rect,
                    &prim_data.stretch_size,
                    &prim_data.tile_spacing,
                    frame_state,
                    &mut scratch.gradient_tiles,
                    &map_local_to_world,
                    &mut |_, mut request| {
                        request.push([
                            prim_data.center.x,
                            prim_data.center.y,
                            prim_data.params.start_offset,
                            prim_data.params.end_offset,
                        ]);
                        request.push([
                            prim_data.params.angle,
                            pack_as_float(prim_data.extend_mode as u32),
                            prim_data.stretch_size.width,
                            prim_data.stretch_size.height,
                        ]);
                    },
                );

                if visible_tiles_range.is_empty() {
                    prim_instance.visibility_info = PrimitiveVisibilityIndex::INVALID;
                }
            }

            // TODO(gw): Consider whether it's worth doing segment building
            //           for gradient primitives.
        }
        PrimitiveInstanceKind::Picture { pic_index, segment_instance_index, .. } => {
            profile_scope!("Picture");
            let pic = &mut store.pictures[pic_index.0];
            let prim_info = &scratch.prim_info[prim_instance.visibility_info.0 as usize];

            if pic.prepare_for_render(
                frame_context,
                frame_state,
                data_stores,
            ) {
                if let Some(ref mut splitter) = pic_state.plane_splitter {
                    PicturePrimitive::add_split_plane(
                        splitter,
                        frame_context.spatial_tree,
                        prim_spatial_node_index,
                        pic.precise_local_rect,
                        &prim_info.combined_local_clip_rect,
                        frame_state.current_dirty_region().combined,
                        plane_split_anchor,
                    );
                }

                // If this picture uses segments, ensure the GPU cache is
                // up to date with segment local rects.
                // TODO(gw): This entire match statement above can now be
                //           refactored into prepare_interned_prim_for_render.
                if pic.can_use_segments() {
                    write_segment(
                        *segment_instance_index,
                        frame_state,
                        &mut scratch.segments,
                        &mut scratch.segment_instances,
                        |request| {
                            request.push(PremultipliedColorF::WHITE);
                            request.push(PremultipliedColorF::WHITE);
                            request.push([
                                -1.0,       // -ve means use prim rect for stretch size
                                0.0,
                                0.0,
                                0.0,
                            ]);
                        }
                    );
                }
            } else {
                prim_instance.visibility_info = PrimitiveVisibilityIndex::INVALID;
            }
        }
        PrimitiveInstanceKind::Backdrop { data_handle } => {
            profile_scope!("Backdrop");
            let backdrop_pic_index = data_stores.backdrop[*data_handle].kind.pic_index;

            // Setup a dependency on the backdrop picture to ensure it is rendered prior to rendering this primitive.
            let backdrop_surface_index = store.pictures[backdrop_pic_index.0].raster_config.as_ref().unwrap().surface_index;
            if let Some(backdrop_tasks) = frame_state.surfaces[backdrop_surface_index.0].render_tasks {
                let picture_task_id = frame_state.surfaces[pic_context.surface_index.0].render_tasks.as_ref().unwrap().port;
                frame_state.render_tasks.add_dependency(picture_task_id, backdrop_tasks.root);
            } else {
                if prim_instance.is_chased() {
                    println!("\tBackdrop primitive culled because backdrop task was not assigned render tasks");
                }
                prim_instance.visibility_info = PrimitiveVisibilityIndex::INVALID;
            }
        }
    };
}


fn write_segment<F>(
    segment_instance_index: SegmentInstanceIndex,
    frame_state: &mut FrameBuildingState,
    segments: &mut SegmentStorage,
    segment_instances: &mut SegmentInstanceStorage,
    f: F,
) where F: Fn(&mut GpuDataRequest) {
    debug_assert_ne!(segment_instance_index, SegmentInstanceIndex::INVALID);
    if segment_instance_index != SegmentInstanceIndex::UNUSED {
        let segment_instance = &mut segment_instances[segment_instance_index];

        if let Some(mut request) = frame_state.gpu_cache.request(&mut segment_instance.gpu_cache_handle) {
            let segments = &segments[segment_instance.segments_range];

            f(&mut request);

            for segment in segments {
                request.write_segment(
                    segment.local_rect,
                    [0.0; 4],
                );
            }
        }
    }
}

fn decompose_repeated_primitive(
    combined_local_clip_rect: &LayoutRect,
    prim_local_rect: &LayoutRect,
    prim_world_rect: WorldRect,
    stretch_size: &LayoutSize,
    tile_spacing: &LayoutSize,
    frame_state: &mut FrameBuildingState,
    gradient_tiles: &mut GradientTileStorage,
    map_local_to_world: &SpaceMapper<LayoutPixel, WorldPixel>,
    callback: &mut dyn FnMut(&LayoutRect, GpuDataRequest),
) -> GradientTileRange {
    let mut visible_tiles = Vec::new();

    // Tighten the clip rect because decomposing the repeated image can
    // produce primitives that are partially covering the original image
    // rect and we want to clip these extra parts out.
    let tight_clip_rect = combined_local_clip_rect
        .intersection(prim_local_rect).unwrap();

    let visible_rect = compute_conservative_visible_rect(
        &tight_clip_rect,
        prim_world_rect,
        map_local_to_world,
    );
    let stride = *stretch_size + *tile_spacing;

    let repetitions = image_tiling::repetitions(prim_local_rect, &visible_rect, stride);
    for Repetition { origin, .. } in repetitions {
        let mut handle = GpuCacheHandle::new();
        let rect = LayoutRect {
            origin,
            size: *stretch_size,
        };

        if let Some(request) = frame_state.gpu_cache.request(&mut handle) {
            callback(&rect, request);
        }

        visible_tiles.push(VisibleGradientTile {
            local_rect: rect,
            local_clip_rect: tight_clip_rect,
            handle
        });
    }

    // At this point if we don't have tiles to show it means we could probably
    // have done a better a job at culling during an earlier stage.
    // Clearing the screen rect has the effect of "culling out" the primitive
    // from the point of view of the batch builder, and ensures we don't hit
    // assertions later on because we didn't request any image.
    if visible_tiles.is_empty() {
        GradientTileRange::empty()
    } else {
        gradient_tiles.extend(visible_tiles)
    }
}


fn update_clip_task_for_brush(
    instance: &PrimitiveInstance,
    prim_origin: &LayoutPoint,
    prim_info: &mut PrimitiveVisibility,
    prim_spatial_node_index: SpatialNodeIndex,
    root_spatial_node_index: SpatialNodeIndex,
    pic_context: &PictureContext,
    pic_state: &mut PictureState,
    frame_context: &FrameBuildingContext,
    frame_state: &mut FrameBuildingState,
    prim_store: &PrimitiveStore,
    data_stores: &mut DataStores,
    segments_store: &mut SegmentStorage,
    segment_instances_store: &mut SegmentInstanceStorage,
    clip_mask_instances: &mut Vec<ClipMaskKind>,
    unclipped: &DeviceRect,
    device_pixel_scale: DevicePixelScale,
) -> bool {
    let segments = match instance.kind {
        PrimitiveInstanceKind::TextRun { .. } |
        PrimitiveInstanceKind::Clear { .. } |
        PrimitiveInstanceKind::LineDecoration { .. } |
        PrimitiveInstanceKind::Backdrop { .. } => {
            return false;
        }
        PrimitiveInstanceKind::Image { image_instance_index, .. } => {
            let segment_instance_index = prim_store
                .images[image_instance_index]
                .segment_instance_index;

            if segment_instance_index == SegmentInstanceIndex::UNUSED {
                return false;
            }

            let segment_instance = &segment_instances_store[segment_instance_index];

            &segments_store[segment_instance.segments_range]
        }
        PrimitiveInstanceKind::Picture { segment_instance_index, .. } => {
            // Pictures may not support segment rendering at all (INVALID)
            // or support segment rendering but choose not to due to size
            // or some other factor (UNUSED).
            if segment_instance_index == SegmentInstanceIndex::UNUSED ||
               segment_instance_index == SegmentInstanceIndex::INVALID {
                return false;
            }

            let segment_instance = &segment_instances_store[segment_instance_index];
            &segments_store[segment_instance.segments_range]
        }
        PrimitiveInstanceKind::YuvImage { segment_instance_index, .. } |
        PrimitiveInstanceKind::Rectangle { segment_instance_index, .. } => {
            debug_assert!(segment_instance_index != SegmentInstanceIndex::INVALID);

            if segment_instance_index == SegmentInstanceIndex::UNUSED {
                return false;
            }

            let segment_instance = &segment_instances_store[segment_instance_index];

            &segments_store[segment_instance.segments_range]
        }
        PrimitiveInstanceKind::ImageBorder { data_handle, .. } => {
            let border_data = &data_stores.image_border[data_handle].kind;

            // TODO: This is quite messy - once we remove legacy primitives we
            //       can change this to be a tuple match on (instance, template)
            border_data.brush_segments.as_slice()
        }
        PrimitiveInstanceKind::NormalBorder { data_handle, .. } => {
            let border_data = &data_stores.normal_border[data_handle].kind;

            // TODO: This is quite messy - once we remove legacy primitives we
            //       can change this to be a tuple match on (instance, template)
            border_data.brush_segments.as_slice()
        }
        PrimitiveInstanceKind::LinearGradient { data_handle, .. } => {
            let prim_data = &data_stores.linear_grad[data_handle];

            // TODO: This is quite messy - once we remove legacy primitives we
            //       can change this to be a tuple match on (instance, template)
            if prim_data.brush_segments.is_empty() {
                return false;
            }

            prim_data.brush_segments.as_slice()
        }
        PrimitiveInstanceKind::RadialGradient { data_handle, .. } => {
            let prim_data = &data_stores.radial_grad[data_handle];

            // TODO: This is quite messy - once we remove legacy primitives we
            //       can change this to be a tuple match on (instance, template)
            if prim_data.brush_segments.is_empty() {
                return false;
            }

            prim_data.brush_segments.as_slice()
        }
        PrimitiveInstanceKind::ConicGradient { data_handle, .. } => {
            let prim_data = &data_stores.conic_grad[data_handle];

            // TODO: This is quite messy - once we remove legacy primitives we
            //       can change this to be a tuple match on (instance, template)
            if prim_data.brush_segments.is_empty() {
                return false;
            }

            prim_data.brush_segments.as_slice()
        }
    };

    // If there are no segments, early out to avoid setting a valid
    // clip task instance location below.
    if segments.is_empty() {
        return true;
    }

    // Set where in the clip mask instances array the clip mask info
    // can be found for this primitive. Each segment will push the
    // clip mask information for itself in update_clip_task below.
    prim_info.clip_task_index = ClipTaskIndex(clip_mask_instances.len() as _);

    // If we only built 1 segment, there is no point in re-running
    // the clip chain builder. Instead, just use the clip chain
    // instance that was built for the main primitive. This is a
    // significant optimization for the common case.
    if segments.len() == 1 {
        let clip_mask_kind = update_brush_segment_clip_task(
            &segments[0],
            Some(&prim_info.clip_chain),
            prim_info.clipped_world_rect,
            root_spatial_node_index,
            pic_context.surface_index,
            pic_state,
            frame_context,
            frame_state,
            &mut data_stores.clip,
            unclipped,
            device_pixel_scale,
        );
        clip_mask_instances.push(clip_mask_kind);
    } else {
        let dirty_world_rect = frame_state.current_dirty_region().combined;

        for segment in segments {
            // Build a clip chain for the smaller segment rect. This will
            // often manage to eliminate most/all clips, and sometimes
            // clip the segment completely.
            frame_state.clip_store.set_active_clips_from_clip_chain(
                &prim_info.clip_chain,
                prim_spatial_node_index,
                &frame_context.spatial_tree,
            );

            let segment_clip_chain = frame_state
                .clip_store
                .build_clip_chain_instance(
                    segment.local_rect.translate(prim_origin.to_vector()),
                    &pic_state.map_local_to_pic,
                    &pic_state.map_pic_to_world,
                    &frame_context.spatial_tree,
                    frame_state.gpu_cache,
                    frame_state.resource_cache,
                    device_pixel_scale,
                    &dirty_world_rect,
                    &mut data_stores.clip,
                    false,
                    instance.is_chased(),
                );

            let clip_mask_kind = update_brush_segment_clip_task(
                &segment,
                segment_clip_chain.as_ref(),
                prim_info.clipped_world_rect,
                root_spatial_node_index,
                pic_context.surface_index,
                pic_state,
                frame_context,
                frame_state,
                &mut data_stores.clip,
                unclipped,
                device_pixel_scale,
            );
            clip_mask_instances.push(clip_mask_kind);
        }
    }

    true
}

pub fn update_clip_task(
    instance: &mut PrimitiveInstance,
    prim_origin: &LayoutPoint,
    prim_spatial_node_index: SpatialNodeIndex,
    root_spatial_node_index: SpatialNodeIndex,
    pic_context: &PictureContext,
    pic_state: &mut PictureState,
    frame_context: &FrameBuildingContext,
    frame_state: &mut FrameBuildingState,
    prim_store: &mut PrimitiveStore,
    data_stores: &mut DataStores,
    scratch: &mut PrimitiveScratchBuffer,
) {
    let prim_info = &mut scratch.prim_info[instance.visibility_info.0 as usize];
    let device_pixel_scale = frame_state.surfaces[pic_context.surface_index.0].device_pixel_scale;

    if instance.is_chased() {
        println!("\tupdating clip task with pic rect {:?}", prim_info.clip_chain.pic_clip_rect);
    }

    // Get the device space rect for the primitive if it was unclipped.
    let unclipped = match get_unclipped_device_rect(
        prim_info.clip_chain.pic_clip_rect,
        &pic_state.map_pic_to_raster,
        device_pixel_scale,
    ) {
        Some(rect) => rect,
        None => return,
    };

    build_segments_if_needed(
        instance,
        &prim_info,
        frame_state,
        prim_store,
        data_stores,
        &mut scratch.segments,
        &mut scratch.segment_instances,
    );

    // First try to  render this primitive's mask using optimized brush rendering.
    if update_clip_task_for_brush(
        instance,
        prim_origin,
        prim_info,
        prim_spatial_node_index,
        root_spatial_node_index,
        pic_context,
        pic_state,
        frame_context,
        frame_state,
        prim_store,
        data_stores,
        &mut scratch.segments,
        &mut scratch.segment_instances,
        &mut scratch.clip_mask_instances,
        &unclipped,
        device_pixel_scale,
    ) {
        if instance.is_chased() {
            println!("\tsegment tasks have been created for clipping");
        }
        return;
    }

    if prim_info.clip_chain.needs_mask {
        // Get a minimal device space rect, clipped to the screen that we
        // need to allocate for the clip mask, as well as interpolated
        // snap offsets.
        if let Some(device_rect) = get_clipped_device_rect(
            &unclipped,
            &pic_state.map_raster_to_world,
            prim_info.clipped_world_rect,
            device_pixel_scale,
        ) {
            let (device_rect, device_pixel_scale) = adjust_mask_scale_for_max_size(device_rect, device_pixel_scale);

            let clip_task_id = RenderTask::new_mask(
                device_rect,
                prim_info.clip_chain.clips_range,
                root_spatial_node_index,
                frame_state.clip_store,
                frame_state.gpu_cache,
                frame_state.resource_cache,
                frame_state.render_tasks,
                &mut data_stores.clip,
                device_pixel_scale,
                frame_context.fb_config,
            );
            if instance.is_chased() {
                println!("\tcreated task {:?} with device rect {:?}",
                    clip_task_id, device_rect);
            }
            // Set the global clip mask instance for this primitive.
            let clip_task_index = ClipTaskIndex(scratch.clip_mask_instances.len() as _);
            scratch.clip_mask_instances.push(ClipMaskKind::Mask(clip_task_id));
            prim_info.clip_task_index = clip_task_index;
            frame_state.render_tasks.add_dependency(
                frame_state.surfaces[pic_context.surface_index.0].render_tasks.unwrap().port,
                clip_task_id,
            );
        }
    }
}

/// Write out to the clip mask instances array the correct clip mask
/// config for this segment.
pub fn update_brush_segment_clip_task(
    segment: &BrushSegment,
    clip_chain: Option<&ClipChainInstance>,
    prim_bounding_rect: WorldRect,
    root_spatial_node_index: SpatialNodeIndex,
    surface_index: SurfaceIndex,
    pic_state: &mut PictureState,
    frame_context: &FrameBuildingContext,
    frame_state: &mut FrameBuildingState,
    clip_data_store: &mut ClipDataStore,
    unclipped: &DeviceRect,
    device_pixel_scale: DevicePixelScale,
) -> ClipMaskKind {
    match clip_chain {
        Some(clip_chain) => {
            if !clip_chain.needs_mask ||
               (!segment.may_need_clip_mask && !clip_chain.has_non_local_clips) {
                return ClipMaskKind::None;
            }

            let segment_world_rect = match pic_state.map_pic_to_world.map(&clip_chain.pic_clip_rect) {
                Some(rect) => rect,
                None => return ClipMaskKind::Clipped,
            };

            let segment_world_rect = match segment_world_rect.intersection(&prim_bounding_rect) {
                Some(rect) => rect,
                None => return ClipMaskKind::Clipped,
            };

            // Get a minimal device space rect, clipped to the screen that we
            // need to allocate for the clip mask, as well as interpolated
            // snap offsets.
            let device_rect = match get_clipped_device_rect(
                unclipped,
                &pic_state.map_raster_to_world,
                segment_world_rect,
                device_pixel_scale,
            ) {
                Some(info) => info,
                None => {
                    return ClipMaskKind::Clipped;
                }
            };

            let (device_rect, device_pixel_scale) = adjust_mask_scale_for_max_size(device_rect, device_pixel_scale);

            let clip_task_id = RenderTask::new_mask(
                device_rect.to_i32(),
                clip_chain.clips_range,
                root_spatial_node_index,
                frame_state.clip_store,
                frame_state.gpu_cache,
                frame_state.resource_cache,
                frame_state.render_tasks,
                clip_data_store,
                device_pixel_scale,
                frame_context.fb_config,
            );
            let port = frame_state
                .surfaces[surface_index.0]
                .render_tasks
                .unwrap_or_else(|| panic!("bug: no task for surface {:?}", surface_index))
                .port;
            frame_state.render_tasks.add_dependency(port, clip_task_id);
            ClipMaskKind::Mask(clip_task_id)
        }
        None => {
            ClipMaskKind::Clipped
        }
    }
}


fn write_brush_segment_description(
    prim_local_rect: LayoutRect,
    prim_local_clip_rect: LayoutRect,
    clip_chain: &ClipChainInstance,
    segment_builder: &mut SegmentBuilder,
    clip_store: &ClipStore,
    data_stores: &DataStores,
) -> bool {
    // If the brush is small, we want to skip building segments
    // and just draw it as a single primitive with clip mask.
    if prim_local_rect.size.area() < MIN_BRUSH_SPLIT_AREA {
        return false;
    }

    segment_builder.initialize(
        prim_local_rect,
        None,
        prim_local_clip_rect
    );

    // Segment the primitive on all the local-space clip sources that we can.
    for i in 0 .. clip_chain.clips_range.count {
        let clip_instance = clip_store
            .get_instance_from_range(&clip_chain.clips_range, i);
        let clip_node = &data_stores.clip[clip_instance.handle];

        // If this clip item is positioned by another positioning node, its relative position
        // could change during scrolling. This means that we would need to resegment. Instead
        // of doing that, only segment with clips that have the same positioning node.
        // TODO(mrobinson, #2858): It may make sense to include these nodes, resegmenting only
        // when necessary while scrolling.
        if !clip_instance.flags.contains(ClipNodeFlags::SAME_SPATIAL_NODE) {
            continue;
        }

        let (local_clip_rect, radius, mode) = match clip_node.item.kind {
            ClipItemKind::RoundedRectangle { rect, radius, mode } => {
                (rect, Some(radius), mode)
            }
            ClipItemKind::Rectangle { rect, mode } => {
                (rect, None, mode)
            }
            ClipItemKind::BoxShadow { ref source } => {
                // For inset box shadows, we can clip out any
                // pixels that are inside the shadow region
                // and are beyond the inner rect, as they can't
                // be affected by the blur radius.
                let inner_clip_mode = match source.clip_mode {
                    BoxShadowClipMode::Outset => None,
                    BoxShadowClipMode::Inset => Some(ClipMode::ClipOut),
                };

                // Push a region into the segment builder where the
                // box-shadow can have an effect on the result. This
                // ensures clip-mask tasks get allocated for these
                // pixel regions, even if no other clips affect them.
                segment_builder.push_mask_region(
                    source.prim_shadow_rect,
                    source.prim_shadow_rect.inflate(
                        -0.5 * source.original_alloc_size.width,
                        -0.5 * source.original_alloc_size.height,
                    ),
                    inner_clip_mode,
                );

                continue;
            }
            ClipItemKind::Image { .. } => {
                // If we encounter an image mask, bail out from segment building.
                // It's not possible to know which parts of the primitive are affected
                // by the mask (without inspecting the pixels). We could do something
                // better here in the future if it ever shows up as a performance issue
                // (for instance, at least segment based on the bounding rect of the
                // image mask if it's non-repeating).
                return false;
            }
        };

        segment_builder.push_clip_rect(local_clip_rect, radius, mode);
    }

    true
}

fn build_segments_if_needed(
    instance: &mut PrimitiveInstance,
    prim_info: &PrimitiveVisibility,
    frame_state: &mut FrameBuildingState,
    prim_store: &mut PrimitiveStore,
    data_stores: &DataStores,
    segments_store: &mut SegmentStorage,
    segment_instances_store: &mut SegmentInstanceStorage,
) {
    let prim_clip_chain = &prim_info.clip_chain;

    // Usually, the primitive rect can be found from information
    // in the instance and primitive template.
    let prim_local_rect = data_stores.get_local_prim_rect(
        instance,
        prim_store,
    );

    let segment_instance_index = match instance.kind {
        PrimitiveInstanceKind::Rectangle { ref mut segment_instance_index, .. } |
        PrimitiveInstanceKind::YuvImage { ref mut segment_instance_index, .. } => {
            segment_instance_index
        }
        PrimitiveInstanceKind::Image { data_handle, image_instance_index, .. } => {
            let image_data = &data_stores.image[data_handle].kind;
            let image_instance = &mut prim_store.images[image_instance_index];
            //Note: tiled images don't support automatic segmentation,
            // they strictly produce one segment per visible tile instead.
            if frame_state
                .resource_cache
                .get_image_properties(image_data.key)
                .and_then(|properties| properties.tiling)
                .is_some()
            {
                image_instance.segment_instance_index = SegmentInstanceIndex::UNUSED;
                return;
            }
            &mut image_instance.segment_instance_index
        }
        PrimitiveInstanceKind::Picture { ref mut segment_instance_index, pic_index, .. } => {
            let pic = &mut prim_store.pictures[pic_index.0];

            // If this picture supports segment rendering
            if pic.can_use_segments() {
                // If the segments have been invalidated, ensure the current
                // index of segments is invalid. This ensures that the segment
                // building logic below will be run.
                if !pic.segments_are_valid {
                    *segment_instance_index = SegmentInstanceIndex::INVALID;
                    pic.segments_are_valid = true;
                }

                segment_instance_index
            } else {
                return;
            }
        }
        PrimitiveInstanceKind::TextRun { .. } |
        PrimitiveInstanceKind::NormalBorder { .. } |
        PrimitiveInstanceKind::ImageBorder { .. } |
        PrimitiveInstanceKind::Clear { .. } |
        PrimitiveInstanceKind::LinearGradient { .. } |
        PrimitiveInstanceKind::RadialGradient { .. } |
        PrimitiveInstanceKind::ConicGradient { .. } |
        PrimitiveInstanceKind::LineDecoration { .. } |
        PrimitiveInstanceKind::Backdrop { .. } => {
            // These primitives don't support / need segments.
            return;
        }
    };

    if *segment_instance_index == SegmentInstanceIndex::INVALID {
        let mut segments: SmallVec<[BrushSegment; 8]> = SmallVec::new();

        if write_brush_segment_description(
            prim_local_rect,
            instance.local_clip_rect,
            prim_clip_chain,
            &mut frame_state.segment_builder,
            frame_state.clip_store,
            data_stores,
        ) {
            frame_state.segment_builder.build(|segment| {
                segments.push(
                    BrushSegment::new(
                        segment.rect.translate(-prim_local_rect.origin.to_vector()),
                        segment.has_mask,
                        segment.edge_flags,
                        [0.0; 4],
                        BrushFlags::PERSPECTIVE_INTERPOLATION,
                    ),
                );
            });
        }

        // If only a single segment is produced, there is no benefit to writing
        // a segment instance array. Instead, just use the main primitive rect
        // written into the GPU cache.
        // TODO(gw): This is (sortof) a bandaid - due to a limitation in the current
        //           brush encoding, we can only support a total of up to 2^16 segments.
        //           This should be (more than) enough for any real world case, so for
        //           now we can handle this by skipping cases where we were generating
        //           segments where there is no benefit. The long term / robust fix
        //           for this is to move the segment building to be done as a more
        //           limited nine-patch system during scene building, removing arbitrary
        //           segmentation during frame-building (see bug #1617491).
        if segments.len() <= 1 {
            *segment_instance_index = SegmentInstanceIndex::UNUSED;
        } else {
            let segments_range = segments_store.extend(segments);

            let instance = SegmentedInstance {
                segments_range,
                gpu_cache_handle: GpuCacheHandle::new(),
            };

            *segment_instance_index = segment_instances_store.push(instance);
        };
    }
}

/// Retrieve the exact unsnapped device space rectangle for a primitive.
fn get_unclipped_device_rect(
    prim_rect: PictureRect,
    map_to_raster: &SpaceMapper<PicturePixel, RasterPixel>,
    device_pixel_scale: DevicePixelScale,
) -> Option<DeviceRect> {
    let raster_rect = map_to_raster.map(&prim_rect)?;
    let world_rect = raster_rect * Scale::new(1.0);
    Some(world_rect * device_pixel_scale)
}

/// Given an unclipped device rect, try to find a minimal device space
/// rect to allocate a clip mask for, by clipping to the screen. This
/// function is very similar to get_raster_rects below. It is far from
/// ideal, and should be refactored as part of the support for setting
/// scale per-raster-root.
fn get_clipped_device_rect(
    unclipped: &DeviceRect,
    map_to_world: &SpaceMapper<RasterPixel, WorldPixel>,
    prim_bounding_rect: WorldRect,
    device_pixel_scale: DevicePixelScale,
) -> Option<DeviceRect> {
    let unclipped_raster_rect = {
        let world_rect = *unclipped * Scale::new(1.0);
        let raster_rect = world_rect * device_pixel_scale.inv();

        raster_rect.cast_unit()
    };

    let unclipped_world_rect = map_to_world.map(&unclipped_raster_rect)?;

    let clipped_world_rect = unclipped_world_rect.intersection(&prim_bounding_rect)?;

    let clipped_raster_rect = map_to_world.unmap(&clipped_world_rect)?;

    let clipped_raster_rect = clipped_raster_rect.intersection(&unclipped_raster_rect)?;

    // Ensure that we won't try to allocate a zero-sized clip render task.
    if clipped_raster_rect.is_empty() {
        return None;
    }

    let clipped = raster_rect_to_device_pixels(
        clipped_raster_rect,
        device_pixel_scale,
    );

    Some(clipped)
}

// Ensures that the size of mask render tasks are within MAX_MASK_SIZE.
fn adjust_mask_scale_for_max_size(device_rect: DeviceRect, device_pixel_scale: DevicePixelScale) -> (DeviceIntRect, DevicePixelScale) {
    if device_rect.width() > MAX_MASK_SIZE || device_rect.height() > MAX_MASK_SIZE {
        // round_out will grow by 1 integer pixel if origin is on a
        // fractional position, so keep that margin for error with -1:
        let scale = (MAX_MASK_SIZE - 1.0) /
            f32::max(device_rect.width(), device_rect.height());
        let new_device_pixel_scale = device_pixel_scale * Scale::new(scale);
        let new_device_rect = (device_rect.to_f32() * Scale::new(scale))
            .round_out()
            .to_i32();
        (new_device_rect, new_device_pixel_scale)
    } else {
        (device_rect.to_i32(), device_pixel_scale)
    }
}