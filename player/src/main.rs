//! vr-player: plays a video in VR on Steam Frame.
//!
//! Decoded frames are uploaded to a texture, and every frame each eye is drawn
//! with one full-screen triangle whose fragment shader turns the pixel's view
//! ray into a position on the video: an equirectangular sphere (360), a
//! hemisphere (180) or a flat screen. SteamVR on the Frame has no equirect
//! composition layers, so the app does this projection itself.

mod video;

use std::{ffi::CStr, io::Cursor, sync::Arc, time::Duration};

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

const VIEW_TYPE: xr::ViewConfigurationType = xr::ViewConfigurationType::PRIMARY_STEREO;

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

/// Per-eye shader inputs; layout matches `Eye` in shaders/video.frag.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct EyeConstants {
    rot: [[f32; 4]; 3],
    tans: [f32; 4],
    pos: [f32; 4],
    rect: [f32; 4],
    params: [f32; 4],
}

impl EyeConstants {
    fn bytes(&self) -> &[u8] {
        // SAFETY: plain #[repr(C)] f32 data with no padding.
        unsafe { std::slice::from_raw_parts(self as *const _ as *const u8, size_of::<Self>()) }
    }
}

/// Rows of the rotation matrix for a unit quaternion.
fn rotation_rows(q: xr::Quaternionf) -> [[f32; 4]; 3] {
    let (x, y, z, w) = (q.x, q.y, q.z, q.w);
    [
        [1.0 - 2.0 * (y * y + z * z), 2.0 * (x * y - z * w), 2.0 * (x * z + y * w), 0.0],
        [2.0 * (x * y + z * w), 1.0 - 2.0 * (x * x + z * z), 2.0 * (y * z - x * w), 0.0],
        [2.0 * (x * z - y * w), 2.0 * (y * z + x * w), 1.0 - 2.0 * (x * x + y * y), 0.0],
    ]
}

/// Which part of the video each eye sees: (x, y, w, h) in texture coordinates.
fn eye_rect(stereo: Stereo, eye: usize) -> [f32; 4] {
    let e = eye as f32;
    match stereo {
        Stereo::Mono => [0.0, 0.0, 1.0, 1.0],
        Stereo::SideBySide => [0.5 * e, 0.0, 0.5, 1.0],
        Stereo::TopBottom => [0.0, 0.5 * e, 1.0, 0.5],
    }
}

fn find_memory(props: &vk::PhysicalDeviceMemoryProperties, bits: u32, want: vk::MemoryPropertyFlags) -> Option<u32> {
    (0..props.memory_type_count)
        .find(|&i| bits & (1 << i) != 0 && props.memory_types[i as usize].property_flags.contains(want))
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
    let instance = entry
        .create_instance(
            &xr::ApplicationInfo {
                application_name: "VR Player",
                application_version: 2,
                engine_name: "vr-player",
                engine_version: 2,
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
        .enumerate_environment_blend_modes(system, VIEW_TYPE)
        .map_err(|e| e.to_string())?[0];
    let reqs = instance.graphics_requirements::<xr::Vulkan>(system).map_err(|e| e.to_string())?;
    if xr::Version::new(1, 1, 0) < reqs.min_api_version_supported {
        return Err(format!("runtime needs Vulkan {}", reqs.min_api_version_supported));
    }
    let view_configs = instance
        .enumerate_view_configuration_views(system, VIEW_TYPE)
        .map_err(|e| e.to_string())?;
    let eye_extent = vk::Extent2D {
        width: view_configs[0].recommended_image_rect_width,
        height: view_configs[0].recommended_image_rect_height,
    };
    eprintln!("openxr: eye resolution {}x{}", eye_extent.width, eye_extent.height);

    unsafe {
        // --- Vulkan through OpenXR ------------------------------------------
        let vk_entry = ash::Entry::load().map_err(|e| format!("Vulkan not available: {e}"))?;
        let app_info = vk::ApplicationInfo::default().api_version(vk::make_api_version(0, 1, 1, 0));
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
        let mem_props = vk_instance.get_physical_device_memory_properties(phys);
        let vkerr = |e: vk::Result| e.to_string();

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

        // --- Eye swapchain (2 array layers, one per eye) ----------------------------
        let formats = session.enumerate_swapchain_formats().map_err(|e| e.to_string())?;
        let color_format = [vk::Format::R8G8B8A8_SRGB, vk::Format::B8G8R8A8_SRGB]
            .into_iter()
            .find(|f| formats.contains(&(f.as_raw() as u32)))
            .ok_or("runtime offers no sRGB 8-bit swapchain format")?;
        let mut swapchain = session
            .create_swapchain(&xr::SwapchainCreateInfo {
                create_flags: xr::SwapchainCreateFlags::EMPTY,
                usage_flags: xr::SwapchainUsageFlags::COLOR_ATTACHMENT | xr::SwapchainUsageFlags::SAMPLED,
                format: color_format.as_raw() as _,
                sample_count: 1,
                width: eye_extent.width,
                height: eye_extent.height,
                face_count: 1,
                array_size: 2,
                mip_count: 1,
            })
            .map_err(|e| format!("could not create eye swapchain: {e}"))?;

        let render_pass = device
            .create_render_pass(
                &vk::RenderPassCreateInfo::default()
                    .attachments(&[vk::AttachmentDescription {
                        format: color_format,
                        samples: vk::SampleCountFlags::TYPE_1,
                        load_op: vk::AttachmentLoadOp::DONT_CARE,
                        store_op: vk::AttachmentStoreOp::STORE,
                        initial_layout: vk::ImageLayout::UNDEFINED,
                        final_layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                        ..Default::default()
                    }])
                    .subpasses(&[vk::SubpassDescription::default()
                        .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
                        .color_attachments(&[vk::AttachmentReference {
                            attachment: 0,
                            layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                        }])])
                    .dependencies(&[vk::SubpassDependency {
                        src_subpass: vk::SUBPASS_EXTERNAL,
                        dst_subpass: 0,
                        src_stage_mask: vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT
                            | vk::PipelineStageFlags::TRANSFER,
                        dst_stage_mask: vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT
                            | vk::PipelineStageFlags::FRAGMENT_SHADER,
                        src_access_mask: vk::AccessFlags::TRANSFER_WRITE,
                        dst_access_mask: vk::AccessFlags::COLOR_ATTACHMENT_WRITE | vk::AccessFlags::SHADER_READ,
                        ..Default::default()
                    }]),
                None,
            )
            .map_err(vkerr)?;

        // One framebuffer per (swapchain image, eye).
        let mut framebuffers = Vec::new();
        let mut views_to_free = Vec::new();
        for raw in swapchain.enumerate_images().map_err(|e| e.to_string())? {
            let image = vk::Image::from_raw(raw);
            let mut per_eye = [vk::Framebuffer::null(); 2];
            for (eye, fb) in per_eye.iter_mut().enumerate() {
                let view = device
                    .create_image_view(
                        &vk::ImageViewCreateInfo::default()
                            .image(image)
                            .view_type(vk::ImageViewType::TYPE_2D)
                            .format(color_format)
                            .subresource_range(vk::ImageSubresourceRange {
                                aspect_mask: vk::ImageAspectFlags::COLOR,
                                base_mip_level: 0,
                                level_count: 1,
                                base_array_layer: eye as u32,
                                layer_count: 1,
                            }),
                        None,
                    )
                    .map_err(vkerr)?;
                *fb = device
                    .create_framebuffer(
                        &vk::FramebufferCreateInfo::default()
                            .render_pass(render_pass)
                            .attachments(&[view])
                            .width(eye_extent.width)
                            .height(eye_extent.height)
                            .layers(1),
                        None,
                    )
                    .map_err(vkerr)?;
                views_to_free.push(view);
            }
            framebuffers.push(per_eye);
        }

        // --- Video texture --------------------------------------------------------
        let tex_format = vk::Format::R8G8B8A8_SRGB;
        let texture = device
            .create_image(
                &vk::ImageCreateInfo::default()
                    .image_type(vk::ImageType::TYPE_2D)
                    .format(tex_format)
                    .extent(vk::Extent3D { width, height, depth: 1 })
                    .mip_levels(1)
                    .array_layers(1)
                    .samples(vk::SampleCountFlags::TYPE_1)
                    .tiling(vk::ImageTiling::OPTIMAL)
                    .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED)
                    .initial_layout(vk::ImageLayout::UNDEFINED),
                None,
            )
            .map_err(|e| format!("could not create {width}x{height} video texture: {e}"))?;
        let req = device.get_image_memory_requirements(texture);
        let texture_memory = device
            .allocate_memory(
                &vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(
                    find_memory(&mem_props, req.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)
                        .ok_or("no device memory for the video texture")?,
                ),
                None,
            )
            .map_err(vkerr)?;
        device.bind_image_memory(texture, texture_memory, 0).map_err(vkerr)?;
        let texture_view = device
            .create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(texture)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(tex_format)
                    .subresource_range(vk::ImageSubresourceRange {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        base_mip_level: 0,
                        level_count: 1,
                        base_array_layer: 0,
                        layer_count: 1,
                    }),
                None,
            )
            .map_err(vkerr)?;
        let sampler = device
            .create_sampler(
                &vk::SamplerCreateInfo::default()
                    .mag_filter(vk::Filter::LINEAR)
                    .min_filter(vk::Filter::LINEAR)
                    .address_mode_u(vk::SamplerAddressMode::REPEAT) // 360 video wraps around horizontally
                    .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE),
                None,
            )
            .map_err(vkerr)?;

        // --- Pipeline ---------------------------------------------------------------
        let set_layout = device
            .create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&[vk::DescriptorSetLayoutBinding::default()
                    .binding(0)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::FRAGMENT)]),
                None,
            )
            .map_err(vkerr)?;
        let pipeline_layout = device
            .create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default()
                    .set_layouts(&[set_layout])
                    .push_constant_ranges(&[vk::PushConstantRange {
                        stage_flags: vk::ShaderStageFlags::FRAGMENT,
                        offset: 0,
                        size: size_of::<EyeConstants>() as u32,
                    }]),
                None,
            )
            .map_err(vkerr)?;
        let descriptor_pool = device
            .create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default().max_sets(1).pool_sizes(&[vk::DescriptorPoolSize {
                    ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                    descriptor_count: 1,
                }]),
                None,
            )
            .map_err(vkerr)?;
        let descriptor_set = device
            .allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default().descriptor_pool(descriptor_pool).set_layouts(&[set_layout]),
            )
            .map_err(vkerr)?[0];
        device.update_descriptor_sets(
            &[vk::WriteDescriptorSet::default()
                .dst_set(descriptor_set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&[vk::DescriptorImageInfo {
                    sampler,
                    image_view: texture_view,
                    image_layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                }])],
            &[],
        );

        let shader = |bytes: &[u8]| {
            let code = ash::util::read_spv(&mut Cursor::new(bytes)).map_err(|e| e.to_string())?;
            device
                .create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&code), None)
                .map_err(vkerr)
        };
        let vert = shader(include_bytes!(concat!(env!("OUT_DIR"), "/fullscreen.vert.spv")))?;
        let frag = shader(include_bytes!(concat!(env!("OUT_DIR"), "/video.frag.spv")))?;
        let main_name = CStr::from_bytes_with_nul(b"main\0").unwrap();
        let pipeline = device
            .create_graphics_pipelines(
                vk::PipelineCache::null(),
                &[vk::GraphicsPipelineCreateInfo::default()
                    .stages(&[
                        vk::PipelineShaderStageCreateInfo::default()
                            .stage(vk::ShaderStageFlags::VERTEX)
                            .module(vert)
                            .name(main_name),
                        vk::PipelineShaderStageCreateInfo::default()
                            .stage(vk::ShaderStageFlags::FRAGMENT)
                            .module(frag)
                            .name(main_name),
                    ])
                    .vertex_input_state(&vk::PipelineVertexInputStateCreateInfo::default())
                    .input_assembly_state(
                        &vk::PipelineInputAssemblyStateCreateInfo::default()
                            .topology(vk::PrimitiveTopology::TRIANGLE_LIST),
                    )
                    .viewport_state(
                        &vk::PipelineViewportStateCreateInfo::default().scissor_count(1).viewport_count(1),
                    )
                    .rasterization_state(
                        &vk::PipelineRasterizationStateCreateInfo::default()
                            .cull_mode(vk::CullModeFlags::NONE)
                            .polygon_mode(vk::PolygonMode::FILL)
                            .line_width(1.0),
                    )
                    .multisample_state(
                        &vk::PipelineMultisampleStateCreateInfo::default()
                            .rasterization_samples(vk::SampleCountFlags::TYPE_1),
                    )
                    .color_blend_state(&vk::PipelineColorBlendStateCreateInfo::default().attachments(&[
                        vk::PipelineColorBlendAttachmentState {
                            color_write_mask: vk::ColorComponentFlags::RGBA,
                            ..Default::default()
                        },
                    ]))
                    .dynamic_state(
                        &vk::PipelineDynamicStateCreateInfo::default()
                            .dynamic_states(&[vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR]),
                    )
                    .layout(pipeline_layout)
                    .render_pass(render_pass)
                    .subpass(0)],
                None,
            )
            .map_err(|(_, e)| e.to_string())?[0];
        device.destroy_shader_module(vert, None);
        device.destroy_shader_module(frag, None);

        // --- Staging buffers the decoder writes into ----------------------------------
        let frame_bytes = width as usize * height as usize * 4;
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
                .map_err(vkerr)?;
            let req = device.get_buffer_memory_requirements(buffer);
            let type_index = find_memory(
                &mem_props,
                req.memory_type_bits,
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
            )
            .ok_or("no host-visible memory")?;
            let memory = device
                .allocate_memory(
                    &vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(type_index),
                    None,
                )
                .map_err(|e| format!("out of memory for video frames: {e}"))?;
            device.bind_buffer_memory(buffer, memory, 0).map_err(vkerr)?;
            let ptr = device.map_memory(memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty()).map_err(vkerr)?;
            stages.push((buffer, memory));
            ptrs.push(ptr as *mut u8);
        }
        let slots = Arc::new(video::FrameSlots::new(ptrs, frame_bytes));

        let pool = device
            .create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .queue_family_index(queue_family)
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                None,
            )
            .map_err(vkerr)?;
        let cmd = device
            .allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default().command_pool(pool).command_buffer_count(1),
            )
            .map_err(vkerr)?[0];
        let fence = device.create_fence(&vk::FenceCreateInfo::default(), None).map_err(vkerr)?;

        let playback = video::Playback::start(
            &args.input,
            &info,
            (width, height),
            "rgba",
            args.hw,
            args.start,
            slots.clone(),
        )?;
        eprintln!("video: decoding with {}", if playback.hw { "hardware (iris)" } else { "software" });

        // --- Main loop -------------------------------------------------------------
        let mut events = xr::EventDataBuffer::new();
        let mut running = false;
        let mut quitting = false;
        let mut last_seq = 0;
        let mut have_frame = false;
        let mut texture_layout = vk::ImageLayout::UNDEFINED;
        let mut frames_uploaded: u64 = 0;
        let started = std::time::Instant::now();

        'main: loop {
            while let Some(event) = instance.poll_event(&mut events).map_err(|e| e.to_string())? {
                match event {
                    xr::Event::SessionStateChanged(e) => {
                        eprintln!("openxr: session {:?}", e.state());
                        match e.state() {
                            xr::SessionState::READY => {
                                session.begin(VIEW_TYPE).map_err(|e| e.to_string())?;
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
                    Projection::Flat => Projection::Sphere360,
                };
                eprintln!("input: projection {:?}", args.projection);
            }

            let new_frame = slots.take_latest(last_seq);
            if !state.should_render || (!have_frame && new_frame.is_none()) {
                if new_frame.is_some() {
                    slots.release();
                }
                frame_stream
                    .end(state.predicted_display_time, blend_mode, &[])
                    .map_err(|e| e.to_string())?;
                continue;
            }

            let (_, views) = session
                .locate_views(VIEW_TYPE, state.predicted_display_time, &space)
                .map_err(|e| e.to_string())?;
            let index = swapchain.acquire_image().map_err(|e| e.to_string())? as usize;
            swapchain.wait_image(xr::Duration::INFINITE).map_err(|e| e.to_string())?;

            device
                .begin_command_buffer(
                    cmd,
                    &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )
                .map_err(vkerr)?;

            if let Some((slot, seq)) = new_frame {
                last_seq = seq;
                upload_frame(&device, cmd, stages[slot].0, texture, texture_layout, width, height);
                texture_layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
                have_frame = true;
                frames_uploaded += 1;
                if frames_uploaded % 300 == 0 {
                    eprintln!(
                        "video: {frames_uploaded} frames, {:.1} fps",
                        frames_uploaded as f64 / started.elapsed().as_secs_f64()
                    );
                }
            }

            let video_aspect = {
                let r = eye_rect(args.stereo, 0);
                (width as f32 * r[2]) / (height as f32 * r[3]).max(1.0)
            };
            let mode = match args.projection {
                Projection::Sphere360 => 0.0,
                Projection::Dome180 => 1.0,
                Projection::Flat => 2.0,
            };
            for (eye, view) in views.iter().enumerate().take(2) {
                let c = EyeConstants {
                    rot: rotation_rows(view.pose.orientation),
                    tans: [
                        view.fov.angle_left.tan(),
                        view.fov.angle_right.tan(),
                        view.fov.angle_up.tan(),
                        view.fov.angle_down.tan(),
                    ],
                    pos: [view.pose.position.x, view.pose.position.y, view.pose.position.z, 0.0],
                    rect: eye_rect(args.stereo, eye),
                    params: [mode, args.screen_m, args.screen_m / video_aspect, args.screen_m],
                };
                device.cmd_begin_render_pass(
                    cmd,
                    &vk::RenderPassBeginInfo::default()
                        .render_pass(render_pass)
                        .framebuffer(framebuffers[index][eye])
                        .render_area(vk::Rect2D { offset: vk::Offset2D::default(), extent: eye_extent }),
                    vk::SubpassContents::INLINE,
                );
                device.cmd_set_viewport(
                    cmd,
                    0,
                    &[vk::Viewport {
                        x: 0.0,
                        y: 0.0,
                        width: eye_extent.width as f32,
                        height: eye_extent.height as f32,
                        min_depth: 0.0,
                        max_depth: 1.0,
                    }],
                );
                device.cmd_set_scissor(cmd, 0, &[vk::Rect2D { offset: vk::Offset2D::default(), extent: eye_extent }]);
                device.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, pipeline);
                device.cmd_bind_descriptor_sets(
                    cmd,
                    vk::PipelineBindPoint::GRAPHICS,
                    pipeline_layout,
                    0,
                    &[descriptor_set],
                    &[],
                );
                device.cmd_push_constants(cmd, pipeline_layout, vk::ShaderStageFlags::FRAGMENT, 0, c.bytes());
                device.cmd_draw(cmd, 3, 1, 0, 0);
                device.cmd_end_render_pass(cmd);
            }
            device.end_command_buffer(cmd).map_err(vkerr)?;
            device
                .queue_submit(queue, &[vk::SubmitInfo::default().command_buffers(&[cmd])], fence)
                .map_err(vkerr)?;
            device.wait_for_fences(&[fence], true, u64::MAX).map_err(vkerr)?;
            device.reset_fences(&[fence]).map_err(vkerr)?;
            if new_frame.is_some() {
                slots.release();
            }
            swapchain.release_image().map_err(|e| e.to_string())?;

            let rect = xr::Rect2Di {
                offset: xr::Offset2Di { x: 0, y: 0 },
                extent: xr::Extent2Di { width: eye_extent.width as i32, height: eye_extent.height as i32 },
            };
            let projection_views: Vec<_> = (0..2)
                .map(|eye| {
                    xr::CompositionLayerProjectionView::new()
                        .pose(views[eye].pose)
                        .fov(views[eye].fov)
                        .sub_image(
                            xr::SwapchainSubImage::new()
                                .swapchain(&swapchain)
                                .image_array_index(eye as u32)
                                .image_rect(rect),
                        )
                })
                .collect();
            frame_stream
                .end(
                    state.predicted_display_time,
                    blend_mode,
                    &[&xr::CompositionLayerProjection::new().space(&space).views(&projection_views)],
                )
                .map_err(|e| e.to_string())?;
        }

        drop(playback);
        // OpenXR handles go before the Vulkan objects they use.
        drop((swapchain, space, actions, session, frame_wait, frame_stream));
        let _ = device.device_wait_idle();
        for (buffer, memory) in stages {
            device.destroy_buffer(buffer, None);
            device.free_memory(memory, None);
        }
        for fbs in framebuffers {
            for fb in fbs {
                device.destroy_framebuffer(fb, None);
            }
        }
        for v in views_to_free {
            device.destroy_image_view(v, None);
        }
        device.destroy_pipeline(pipeline, None);
        device.destroy_pipeline_layout(pipeline_layout, None);
        device.destroy_descriptor_pool(descriptor_pool, None);
        device.destroy_descriptor_set_layout(set_layout, None);
        device.destroy_sampler(sampler, None);
        device.destroy_image_view(texture_view, None);
        device.destroy_image(texture, None);
        device.free_memory(texture_memory, None);
        device.destroy_render_pass(render_pass, None);
        device.destroy_fence(fence, None);
        device.destroy_command_pool(pool, None);
        device.destroy_device(None);
        vk_instance.destroy_instance(None);
    }
    Ok(())
}

/// Record: staging buffer -> video texture, ending in SHADER_READ_ONLY_OPTIMAL.
unsafe fn upload_frame(
    device: &ash::Device,
    cmd: vk::CommandBuffer,
    src: vk::Buffer,
    image: vk::Image,
    old_layout: vk::ImageLayout,
    width: u32,
    height: u32,
) {
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
        device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::FRAGMENT_SHADER,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[barrier(
                old_layout,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::AccessFlags::SHADER_READ,
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
            vk::PipelineStageFlags::FRAGMENT_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[barrier(
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                vk::AccessFlags::TRANSFER_WRITE,
                vk::AccessFlags::SHADER_READ,
            )],
        );
    }
}
