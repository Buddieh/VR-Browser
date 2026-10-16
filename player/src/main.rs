//! vr-player: plays a video in VR on Steam Frame.
//!
//! The video is shown with OpenXR composition layers, so the runtime does the
//! projection and reprojection itself: an equirect layer for 360/180 video and
//! a quad layer for flat video. Each eye gets its own layer for side-by-side
//! or top-bottom 3D. The app only has to copy decoded frames into a swapchain.

mod video;

use std::{f32::consts::PI, sync::Arc, time::Duration};

use ash::vk::{self, Handle};
use openxr as xr;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Projection {
    Sphere360,
    Dome180,
    Flat,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Stereo {
    Mono,
    SideBySide,
    TopBottom,
}

struct Args {
    input: String,
    projection: Projection,
    stereo: Stereo,
    hw: bool,
    max_width: u32,
    start: f64,
    screen_m: f32,
}

const USAGE: &str = "usage: vr-player <file-or-url> [--mode 360|180|flat] [--stereo mono|sbs|tb] \
[--sw] [--max-width N] [--start SECONDS] [--screen METERS]

Controls: A = cycle 360 / 180 / flat, B or Y = quit";

fn parse_args() -> Result<Args, String> {
    let mut it = std::env::args().skip(1);
    let mut a = Args {
        input: String::new(),
        projection: Projection::Sphere360,
        stereo: Stereo::Mono,
        hw: true,
        max_width: 4096,
        start: 0.0,
        screen_m: 3.0,
    };
    while let Some(arg) = it.next() {
        let mut val = |name: &str| it.next().ok_or(format!("{name} needs a value"));
        match arg.as_str() {
            "--mode" => {
                a.projection = match val("--mode")?.as_str() {
                    "360" => Projection::Sphere360,
                    "180" => Projection::Dome180,
                    "flat" => Projection::Flat,
                    m => return Err(format!("unknown mode {m}")),
                }
            }
            "--stereo" => {
                a.stereo = match val("--stereo")?.as_str() {
                    "mono" => Stereo::Mono,
                    "sbs" => Stereo::SideBySide,
                    "tb" => Stereo::TopBottom,
                    m => return Err(format!("unknown stereo layout {m}")),
                }
            }
            "--sw" => a.hw = false,
            "--max-width" => a.max_width = val("--max-width")?.parse().map_err(|_| "bad --max-width")?,
            "--start" => a.start = val("--start")?.parse().map_err(|_| "bad --start")?,
            "--screen" => a.screen_m = val("--screen")?.parse().map_err(|_| "bad --screen")?,
            "-h" | "--help" => return Err(String::new()),
            s if s.starts_with("--") => return Err(format!("unknown option {s}")),
            s => a.input = s.to_string(),
        }
    }
    if a.input.is_empty() {
        return Err("no video given".into());
    }
    Ok(a)
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            if !e.is_empty() {
                eprintln!("error: {e}");
            }
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };
    if let Err(e) = run(args) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

struct Stage {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
}

fn run(mut args: Args) -> Result<(), String> {
    let info = video::probe(&args.input)?;
    let (width, height) = video::output_size(&info, args.max_width);
    eprintln!(
        "video: {} {}x{} @ {:.2} fps -> {}x{}, {:?} {:?}",
        info.codec, info.width, info.height, info.fps, width, height, args.projection, args.stereo
    );

    // --- OpenXR instance -------------------------------------------------
    let entry = xr::Entry::linked(&()).map_err(|e| format!("OpenXR loader: {e}"))?;
    let available = entry.enumerate_extensions().map_err(|e| format!("OpenXR: {e}"))?;
    if !available.khr_vulkan_enable2 {
        return Err("the OpenXR runtime does not support Vulkan (XR_KHR_vulkan_enable2)".into());
    }
    let mut enabled = xr::ExtensionSet::default();
    enabled.khr_vulkan_enable2 = true;
    enabled.khr_composition_layer_equirect2 = available.khr_composition_layer_equirect2;
    eprintln!(
        "openxr: equirect2 layers {}",
        if available.khr_composition_layer_equirect2 { "supported" } else { "NOT supported" }
    );
    if !available.khr_composition_layer_equirect2 && args.projection != Projection::Flat {
        eprintln!("openxr: falling back to a flat screen");
        args.projection = Projection::Flat;
    }

    let instance = entry
        .create_instance(
            &xr::ApplicationInfo {
                application_name: "VR Player",
                application_version: 1,
                engine_name: "vr-player",
                engine_version: 1,
                api_version: xr::Version::new(1, 0, 0),
            },
            &enabled,
            &[],
            &(),
        )
        .map_err(|e| format!("could not start OpenXR (is SteamVR running?): {e}"))?;
    let props = instance.properties().map_err(|e| e.to_string())?;
    eprintln!("openxr: runtime {} {}", props.runtime_name, props.runtime_version);

    let system = instance
        .system(xr::FormFactor::HEAD_MOUNTED_DISPLAY)
        .map_err(|e| format!("no headset found: {e}"))?;
    let blend_mode = instance
        .enumerate_environment_blend_modes(system, xr::ViewConfigurationType::PRIMARY_STEREO)
        .map_err(|e| e.to_string())?[0];

    let reqs = instance.graphics_requirements::<xr::Vulkan>(system).map_err(|e| e.to_string())?;
    let vk_version = vk::make_api_version(0, 1, 1, 0);
    if xr::Version::new(1, 1, 0) < reqs.min_api_version_supported {
        return Err(format!("runtime needs Vulkan {}", reqs.min_api_version_supported));
    }

    unsafe {
        // --- Vulkan through OpenXR ------------------------------------------
        let vk_entry = ash::Entry::load().map_err(|e| format!("Vulkan not available: {e}"))?;
        let app_info = vk::ApplicationInfo::default().api_version(vk_version);
        #[allow(clippy::missing_transmute_annotations)]
        let vk_instance = {
            let raw = instance
                .create_vulkan_instance(
                    system,
                    std::mem::transmute(vk_entry.static_fn().get_instance_proc_addr),
                    &vk::InstanceCreateInfo::default().application_info(&app_info) as *const _ as *const _,
                )
                .map_err(|e| e.to_string())?
                .map_err(|e| format!("vkCreateInstance: {e}"))?;
            ash::Instance::load(vk_entry.static_fn(), vk::Instance::from_raw(raw as _))
        };
        let phys = vk::PhysicalDevice::from_raw(
            instance
                .vulkan_graphics_device(system, vk_instance.handle().as_raw() as _)
                .map_err(|e| e.to_string())? as _,
        );
        let queue_family = vk_instance
            .get_physical_device_queue_family_properties(phys)
            .iter()
            .position(|q| q.queue_flags.contains(vk::QueueFlags::GRAPHICS))
            .ok_or("no graphics queue")? as u32;
        #[allow(clippy::missing_transmute_annotations)]
        let device = {
            let raw = instance
                .create_vulkan_device(
                    system,
                    std::mem::transmute(vk_entry.static_fn().get_instance_proc_addr),
                    phys.as_raw() as _,
                    &vk::DeviceCreateInfo::default().queue_create_infos(&[vk::DeviceQueueCreateInfo::default()
                        .queue_family_index(queue_family)
                        .queue_priorities(&[1.0])]) as *const _ as *const _,
                )
                .map_err(|e| e.to_string())?
                .map_err(|e| format!("vkCreateDevice: {e}"))?;
            ash::Device::load(vk_instance.fp_v1_0(), vk::Device::from_raw(raw as _))
        };
        let queue = device.get_device_queue(queue_family, 0);

        // --- Session, space, input ---------------------------------------------
        let (session, mut frame_wait, mut frame_stream) = instance
            .create_session::<xr::Vulkan>(
                system,
                &xr::vulkan::SessionCreateInfo {
                    instance: vk_instance.handle().as_raw() as _,
                    physical_device: phys.as_raw() as _,
                    device: device.handle().as_raw() as _,
                    queue_family_index: queue_family,
                    queue_index: 0,
                },
            )
            .map_err(|e| format!("could not create VR session: {e}"))?;
        let space = session
            .create_reference_space(xr::ReferenceSpaceType::LOCAL, xr::Posef::IDENTITY)
            .map_err(|e| e.to_string())?;

        let actions = instance.create_action_set("player", "Player", 0).map_err(|e| e.to_string())?;
        let quit = actions.create_action::<bool>("quit", "Quit", &[]).map_err(|e| e.to_string())?;
        let cycle = actions.create_action::<bool>("cycle", "Cycle 360/180/flat", &[]).map_err(|e| e.to_string())?;
        let path = |p: &str| instance.string_to_path(p).unwrap();
        for (profile, quit_paths, cycle_path) in [
            (
                "/interaction_profiles/oculus/touch_controller",
                &["/user/hand/right/input/b/click", "/user/hand/left/input/y/click"][..],
                "/user/hand/right/input/a/click",
            ),
            (
                "/interaction_profiles/valve/index_controller",
                &["/user/hand/right/input/b/click", "/user/hand/left/input/b/click"][..],
                "/user/hand/right/input/a/click",
            ),
            (
                "/interaction_profiles/khr/simple_controller",
                &["/user/hand/right/input/menu/click"][..],
                "/user/hand/right/input/select/click",
            ),
        ] {
            let mut b: Vec<xr::Binding> = quit_paths.iter().map(|p| xr::Binding::new(&quit, path(p))).collect();
            b.push(xr::Binding::new(&cycle, path(cycle_path)));
            if let Err(e) = instance.suggest_interaction_profile_bindings(path(profile), &b) {
                eprintln!("input: bindings for {profile} rejected: {e}");
            }
        }
        session.attach_action_sets(&[&actions]).map_err(|e| e.to_string())?;

        // --- Swapchain sized to the video --------------------------------------
        let formats = session.enumerate_swapchain_formats().map_err(|e| e.to_string())?;
        let (format, pix_fmt) = [
            (vk::Format::R8G8B8A8_SRGB, "rgba"),
            (vk::Format::B8G8R8A8_SRGB, "bgra"),
            (vk::Format::R8G8B8A8_UNORM, "rgba"),
            (vk::Format::B8G8R8A8_UNORM, "bgra"),
        ]
        .into_iter()
        .find(|(f, _)| formats.contains(&(f.as_raw() as u32)))
        .ok_or("runtime offers no 8-bit RGBA swapchain format")?;
        eprintln!("openxr: swapchain format {} ({pix_fmt})", format.as_raw());

        let mut swapchain = session
            .create_swapchain(&xr::SwapchainCreateInfo {
                create_flags: xr::SwapchainCreateFlags::EMPTY,
                usage_flags: xr::SwapchainUsageFlags::TRANSFER_DST
                    | xr::SwapchainUsageFlags::SAMPLED
                    | xr::SwapchainUsageFlags::COLOR_ATTACHMENT,
                format: format.as_raw() as _,
                sample_count: 1,
                width,
                height,
                face_count: 1,
                array_size: 1,
                mip_count: 1,
            })
            .map_err(|e| format!("could not create {width}x{height} swapchain: {e}"))?;
        let images: Vec<vk::Image> = swapchain
            .enumerate_images()
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(vk::Image::from_raw)
            .collect();

        // --- Staging buffers the decoder writes into -----------------------------
        let frame_bytes = width as usize * height as usize * 4;
        let mem_props = vk_instance.get_physical_device_memory_properties(phys);
        let mut stages = Vec::new();
        let mut ptrs = Vec::new();
        for _ in 0..3 {
            let buffer = device
                .create_buffer(
                    &vk::BufferCreateInfo::default()
                        .size(frame_bytes as u64)
                        .usage(vk::BufferUsageFlags::TRANSFER_SRC),
                    None,
                )
                .map_err(|e| e.to_string())?;
            let req = device.get_buffer_memory_requirements(buffer);
            let want = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
            let type_index = (0..mem_props.memory_type_count)
                .find(|&i| {
                    req.memory_type_bits & (1 << i) != 0
                        && mem_props.memory_types[i as usize].property_flags.contains(want)
                })
                .ok_or("no host-visible memory")?;
            let memory = device
                .allocate_memory(
                    &vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(type_index),
                    None,
                )
                .map_err(|e| format!("out of memory for video frames: {e}"))?;
            device.bind_buffer_memory(buffer, memory, 0).map_err(|e| e.to_string())?;
            let ptr = device
                .map_memory(memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())
                .map_err(|e| e.to_string())? as *mut u8;
            stages.push(Stage { buffer, memory });
            ptrs.push(ptr);
        }
        let slots = Arc::new(video::FrameSlots::new(ptrs, frame_bytes));

        let pool = device
            .create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .queue_family_index(queue_family)
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                None,
            )
            .map_err(|e| e.to_string())?;
        let cmd = device
            .allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default().command_pool(pool).command_buffer_count(1),
            )
            .map_err(|e| e.to_string())?[0];
        let fence = device.create_fence(&vk::FenceCreateInfo::default(), None).map_err(|e| e.to_string())?;

        let playback =
            video::Playback::start(&args.input, &info, (width, height), pix_fmt, args.hw, args.start, slots.clone())?;
        eprintln!("video: decoding with {}", if playback.hw { "hardware (iris)" } else { "software" });

        // --- Main loop -------------------------------------------------------------
        let mut events = xr::EventDataBuffer::new();
        let mut running = false;
        let mut quitting = false;
        let mut last_seq = 0;
        let mut have_image = false;
        let mut frames_shown: u64 = 0;
        let started = std::time::Instant::now();

        'main: loop {
            while let Some(event) = instance.poll_event(&mut events).map_err(|e| e.to_string())? {
                match event {
                    xr::Event::SessionStateChanged(e) => {
                        eprintln!("openxr: session {:?}", e.state());
                        match e.state() {
                            xr::SessionState::READY => {
                                session
                                    .begin(xr::ViewConfigurationType::PRIMARY_STEREO)
                                    .map_err(|e| e.to_string())?;
                                running = true;
                            }
                            xr::SessionState::STOPPING => {
                                session.end().map_err(|e| e.to_string())?;
                                running = false;
                            }
                            xr::SessionState::EXITING | xr::SessionState::LOSS_PENDING => break 'main,
                            _ => {}
                        }
                    }
                    xr::Event::InstanceLossPending(_) => break 'main,
                    _ => {}
                }
            }
            if !running {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }

            let state = frame_wait.wait().map_err(|e| e.to_string())?;
            frame_stream.begin().map_err(|e| e.to_string())?;

            session.sync_actions(&[(&actions).into()]).map_err(|e| e.to_string())?;
            let pressed = |a: &xr::Action<bool>| {
                a.state(&session, xr::Path::NULL)
                    .map(|s| s.is_active && s.changed_since_last_sync && s.current_state)
                    .unwrap_or(false)
            };
            if pressed(&quit) && !quitting {
                quitting = true;
                let _ = session.request_exit();
            }
            if pressed(&cycle) {
                args.projection = match args.projection {
                    Projection::Sphere360 => Projection::Dome180,
                    Projection::Dome180 => Projection::Flat,
                    Projection::Flat if enabled.khr_composition_layer_equirect2 => Projection::Sphere360,
                    Projection::Flat => Projection::Flat,
                };
                eprintln!("input: projection {:?}", args.projection);
            }

            // Copy the newest decoded frame into the swapchain.
            if let Some((slot, seq)) = slots.take_latest(last_seq) {
                last_seq = seq;
                let index = swapchain.acquire_image().map_err(|e| e.to_string())? as usize;
                swapchain.wait_image(xr::Duration::INFINITE).map_err(|e| e.to_string())?;
                record_copy(&device, cmd, stages[slot].buffer, images[index], width, height)?;
                device
                    .queue_submit(queue, &[vk::SubmitInfo::default().command_buffers(&[cmd])], fence)
                    .map_err(|e| e.to_string())?;
                device.wait_for_fences(&[fence], true, u64::MAX).map_err(|e| e.to_string())?;
                device.reset_fences(&[fence]).map_err(|e| e.to_string())?;
                slots.release();
                swapchain.release_image().map_err(|e| e.to_string())?;
                have_image = true;
                frames_shown += 1;
                if frames_shown % 300 == 0 {
                    eprintln!(
                        "video: {frames_shown} frames, {:.1} fps shown",
                        frames_shown as f64 / started.elapsed().as_secs_f64()
                    );
                }
            }

            if !state.should_render || !have_image {
                frame_stream
                    .end(state.predicted_display_time, blend_mode, &[])
                    .map_err(|e| e.to_string())?;
                continue;
            }

            // One layer per eye (or one for both eyes when mono).
            let full = (0, 0, width as i32, height as i32);
            let (w2, h2) = (width as i32 / 2, height as i32 / 2);
            let eyes: Vec<(xr::EyeVisibility, (i32, i32, i32, i32))> = match args.stereo {
                Stereo::Mono => vec![(xr::EyeVisibility::BOTH, full)],
                Stereo::SideBySide => vec![
                    (xr::EyeVisibility::LEFT, (0, 0, w2, height as i32)),
                    (xr::EyeVisibility::RIGHT, (w2, 0, w2, height as i32)),
                ],
                Stereo::TopBottom => vec![
                    (xr::EyeVisibility::LEFT, (0, 0, width as i32, h2)),
                    (xr::EyeVisibility::RIGHT, (0, h2, width as i32, h2)),
                ],
            };
            let sub = |(x, y, w, h): (i32, i32, i32, i32)| {
                xr::SwapchainSubImage::new().swapchain(&swapchain).image_rect(xr::Rect2Di {
                    offset: xr::Offset2Di { x, y },
                    extent: xr::Extent2Di { width: w, height: h },
                })
            };

            let mut equirects = Vec::new();
            let mut quads = Vec::new();
            for (eye, rect) in eyes {
                match args.projection {
                    Projection::Sphere360 | Projection::Dome180 => {
                        let span = if args.projection == Projection::Sphere360 { 2.0 * PI } else { PI };
                        equirects.push(
                            xr::CompositionLayerEquirect2KHR::new()
                                .space(&space)
                                .eye_visibility(eye)
                                .sub_image(sub(rect))
                                .pose(xr::Posef::IDENTITY)
                                .radius(0.0)
                                .central_horizontal_angle(span)
                                .upper_vertical_angle(PI / 2.0)
                                .lower_vertical_angle(-PI / 2.0),
                        );
                    }
                    Projection::Flat => {
                        let aspect = rect.2 as f32 / rect.3.max(1) as f32;
                        let w = args.screen_m;
                        quads.push(
                            xr::CompositionLayerQuad::new()
                                .space(&space)
                                .eye_visibility(eye)
                                .sub_image(sub(rect))
                                .pose(xr::Posef {
                                    orientation: xr::Quaternionf::IDENTITY,
                                    position: xr::Vector3f { x: 0.0, y: 0.0, z: -w },
                                })
                                .size(xr::Extent2Df { width: w, height: w / aspect }),
                        );
                    }
                }
            }
            let layers: Vec<&xr::CompositionLayerBase<xr::Vulkan>> =
                equirects.iter().map(|l| &**l).chain(quads.iter().map(|l| &**l)).collect();
            frame_stream
                .end(state.predicted_display_time, blend_mode, &layers)
                .map_err(|e| e.to_string())?;
        }

        drop(playback);
        // OpenXR handles go before the Vulkan objects they use.
        drop((swapchain, space, session, frame_wait, frame_stream));
        let _ = device.device_wait_idle();
        for s in &stages {
            device.destroy_buffer(s.buffer, None);
            device.free_memory(s.memory, None);
        }
        device.destroy_fence(fence, None);
        device.destroy_command_pool(pool, None);
        device.destroy_device(None);
        vk_instance.destroy_instance(None);
    }
    Ok(())
}

/// Record: staging buffer -> swapchain image, leaving the image in the
/// COLOR_ATTACHMENT_OPTIMAL layout OpenXR expects back.
unsafe fn record_copy(
    device: &ash::Device,
    cmd: vk::CommandBuffer,
    src: vk::Buffer,
    image: vk::Image,
    width: u32,
    height: u32,
) -> Result<(), String> {
    let range = vk::ImageSubresourceRange {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        base_mip_level: 0,
        level_count: 1,
        base_array_layer: 0,
        layer_count: 1,
    };
    let barrier = |old, new, src_access, dst_access| {
        vk::ImageMemoryBarrier::default()
            .old_layout(old)
            .new_layout(new)
            .src_access_mask(src_access)
            .dst_access_mask(dst_access)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(image)
            .subresource_range(range)
    };
    unsafe {
        device
            .begin_command_buffer(
                cmd,
                &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
            .map_err(|e| e.to_string())?;
        device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::TOP_OF_PIPE,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[barrier(
                vk::ImageLayout::UNDEFINED,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::AccessFlags::empty(),
                vk::AccessFlags::TRANSFER_WRITE,
            )],
        );
        device.cmd_copy_buffer_to_image(
            cmd,
            src,
            image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            &[vk::BufferImageCopy {
                buffer_offset: 0,
                buffer_row_length: 0,
                buffer_image_height: 0,
                image_subresource: vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                },
                image_offset: vk::Offset3D::default(),
                image_extent: vk::Extent3D { width, height, depth: 1 },
            }],
        );
        device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[barrier(
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                vk::AccessFlags::TRANSFER_WRITE,
                vk::AccessFlags::COLOR_ATTACHMENT_READ,
            )],
        );
        device.end_command_buffer(cmd).map_err(|e| e.to_string())
    }
}
