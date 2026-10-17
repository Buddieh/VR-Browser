//! vr-player: plays a video in VR on Steam Frame.
//!
//! Decoded frames are uploaded to two textures (luma and chroma), and every frame each eye is drawn
//! with one full-screen triangle whose fragment shader turns the pixel's view
//! ray into a position on the video: an equirectangular sphere (360), a
//! hemisphere (180), YouTube's cubemap or a flat screen. SteamVR on the Frame
//! has no equirect composition layers, so the app does this projection itself.
//! A control panel (ui.rs) floats in front of the viewer as a quad layer.

mod source;
mod ui;
mod video;

use std::{
    ffi::CStr,
    io::Cursor,
    sync::Arc,
    time::{Duration, Instant},
};

use ash::vk::{self, Handle};
use openxr as xr;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Projection {
    Sphere360,
    /// YouTube's equi-angular cubemap (EAC).
    Cube360,
    Dome180,
    Flat,
}

impl Projection {
    fn next(self) -> Self {
        match self {
            Projection::Sphere360 => Projection::Cube360,
            Projection::Cube360 => Projection::Dome180,
            Projection::Dome180 => Projection::Flat,
            Projection::Flat => Projection::Sphere360,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Projection::Sphere360 => "360",
            Projection::Cube360 => "360 cube",
            Projection::Dome180 => "180",
            Projection::Flat => "Flat",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Stereo {
    Mono,
    SideBySide,
    TopBottom,
}

impl Stereo {
    fn next(self) -> Self {
        match self {
            Stereo::Mono => Stereo::SideBySide,
            Stereo::SideBySide => Stereo::TopBottom,
            Stereo::TopBottom => Stereo::Mono,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Stereo::Mono => "Off",
            Stereo::SideBySide => "SBS",
            Stereo::TopBottom => "TB",
        }
    }
}

struct Args {
    input: String,
    /// `None` = detect from the video's metadata or title.
    projection: Option<Projection>,
    stereo: Option<Stereo>,
    hw: bool,
    max_width: u32,
    start: f64,
    screen_m: f32,
}

const USAGE: &str = "usage: vr-player <file-or-url> [--mode 360|cube|180|flat] [--stereo mono|sbs|tb] \
[--sw] [--max-width N] [--start SECONDS] [--screen METERS]

The input can be a video file, a direct video URL, or a web page (YouTube and
other sites supported by yt-dlp). Without --mode/--stereo the layout is
detected from the video's metadata or title.

Controls: point at the panel and pull the trigger to click. A or X = pause,
thumbstick left/right = skip 10 seconds, B or Y = show or hide the panel.";

const VIEW_TYPE: xr::ViewConfigurationType = xr::ViewConfigurationType::PRIMARY_STEREO;

fn parse_args() -> Result<Args, String> {
    let mut it = std::env::args().skip(1);
    let mut a = Args {
        input: String::new(),
        projection: None,
        stereo: None,
        hw: true,
        max_width: 4096,
        start: 0.0,
        screen_m: 3.0,
    };
    while let Some(arg) = it.next() {
        let mut val = |name: &str| it.next().ok_or(format!("{name} needs a value"));
        match arg.as_str() {
            "--mode" => {
                a.projection = Some(match val("--mode")?.as_str() {
                    "360" => Projection::Sphere360,
                    "cube" | "eac" => Projection::Cube360,
                    "180" => Projection::Dome180,
                    "flat" => Projection::Flat,
                    m => return Err(format!("unknown mode {m}")),
                })
            }
            "--stereo" => {
                a.stereo = Some(match val("--stereo")?.as_str() {
                    "mono" => Stereo::Mono,
                    "sbs" => Stereo::SideBySide,
                    "tb" => Stereo::TopBottom,
                    m => return Err(format!("unknown stereo layout {m}")),
                })
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
    color: [f32; 4],
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

fn quat_mul(a: xr::Quaternionf, b: xr::Quaternionf) -> xr::Quaternionf {
    xr::Quaternionf {
        x: a.w * b.x + a.x * b.w + a.y * b.z - a.z * b.y,
        y: a.w * b.y - a.x * b.z + a.y * b.w + a.z * b.x,
        z: a.w * b.z + a.x * b.y - a.y * b.x + a.z * b.w,
        w: a.w * b.w - a.x * b.x - a.y * b.y - a.z * b.z,
    }
}

fn quat_inv(q: xr::Quaternionf) -> xr::Quaternionf {
    xr::Quaternionf { x: -q.x, y: -q.y, z: -q.z, w: q.w }
}

fn rotate(q: xr::Quaternionf, v: xr::Vector3f) -> xr::Vector3f {
    // v + 2w(u x v) + 2u x (u x v), with u the quaternion's vector part
    let cross = |a: [f32; 3], b: [f32; 3]| [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]];
    let u = [q.x, q.y, q.z];
    let t = cross(u, [v.x, v.y, v.z]).map(|c| 2.0 * c);
    let ut = cross(u, t);
    xr::Vector3f { x: v.x + q.w * t[0] + ut[0], y: v.y + q.w * t[1] + ut[1], z: v.z + q.w * t[2] + ut[2] }
}

fn add(a: xr::Vector3f, b: xr::Vector3f) -> xr::Vector3f {
    xr::Vector3f { x: a.x + b.x, y: a.y + b.y, z: a.z + b.z }
}

/// The turn around the vertical axis that faces where `q` looks.
fn yaw_of(q: xr::Quaternionf) -> xr::Quaternionf {
    let f = rotate(q, xr::Vector3f { x: 0.0, y: 0.0, z: -1.0 });
    let a = (-f.x).atan2(-f.z);
    xr::Quaternionf { x: 0.0, y: (a / 2.0).sin(), z: 0.0, w: (a / 2.0).cos() }
}

/// Where a controller's pointing ray meets the panel, in panel pixels.
fn panel_hit(panel: xr::Posef, aim: xr::Posef) -> Option<(f32, f32)> {
    let inv = quat_inv(panel.orientation);
    let rel = xr::Vector3f {
        x: aim.position.x - panel.position.x,
        y: aim.position.y - panel.position.y,
        z: aim.position.z - panel.position.z,
    };
    let o = rotate(inv, rel);
    let d = rotate(inv, rotate(aim.orientation, xr::Vector3f { x: 0.0, y: 0.0, z: -1.0 }));
    if d.z >= -1e-4 || o.z <= 0.0 {
        return None; // pointing away, or behind the panel
    }
    let t = -o.z / d.z;
    let (u, v) = ((o.x + t * d.x) / ui::WIDTH_M + 0.5, 0.5 - (o.y + t * d.y) / ui::HEIGHT_M);
    ((0.0..=1.0).contains(&u) && (0.0..=1.0).contains(&v)).then(|| (u * ui::W as f32, v * ui::H as f32))
}

/// Restart playback at `target` seconds, keeping it paused if it was.
fn seek(
    decoder: &video::Decoder,
    playback: &mut video::Playback,
    slots: &Arc<video::FrameSlots>,
    target: f64,
    duration: Option<f64>,
) {
    let target = target.clamp(0.0, duration.map(|d| (d - 1.0).max(0.0)).unwrap_or(f64::MAX));
    let paused = playback.paused() && !playback.ended();
    eprintln!("input: seek to {target:.1}s");
    playback.stop();
    match decoder.start(target, slots.clone(), paused) {
        Ok(p) => *playback = p,
        Err(e) => eprintln!("video: could not seek: {e}"),
    }
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

fn run(args: Args) -> Result<(), String> {
    let src = source::resolve(&args.input, args.max_width)?;
    let info = video::probe(&src.video)?;
    let (width, height) = video::output_size(&info, args.max_width);

    // Layout: command line, then stream metadata, then the title; else a flat screen.
    let meta = source::probe_hints(&src.video);
    let named = source::guess_from_name(&src.title);
    let mut projection = args.projection.or(meta.projection).or(named.projection).unwrap_or(Projection::Flat);
    let mut stereo = args.stereo.or(meta.stereo).or(named.stereo).unwrap_or(Stereo::Mono);
    // A guessed 360 (or untagged web video) may really be YouTube's cubemap;
    // the first detailed frames decide.
    let mut check_cube = args.projection.is_none()
        && meta.projection.is_none()
        && (projection == Projection::Sphere360 || (projection == Projection::Flat && src.audio.is_some()));
    eprintln!(
        "video: {} {}x{} @ {:.2} fps -> {}x{}, {:?} {:?}",
        info.codec, info.width, info.height, info.fps, width, height, projection, stereo
    );
    let audio = src.audio.clone().or_else(|| info.has_audio.then(|| src.video.clone()));

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
        let path = |p: &str| instance.string_to_path(p).unwrap();
        let hands = [path("/user/hand/left"), path("/user/hand/right")];
        let act = |name: &str, label: &str| actions.create_action::<bool>(name, label, &[]).map_err(|e| e.to_string());
        let select = actions.create_action::<bool>("select", "Click", &hands).map_err(|e| e.to_string())?;
        let aim = actions.create_action::<xr::Posef>("aim", "Pointer", &hands).map_err(|e| e.to_string())?;
        let pause = act("pause", "Play or pause")?;
        let menu = act("menu", "Show or hide the panel")?;
        let stick = actions.create_action::<xr::Vector2f>("skip", "Skip 10 seconds", &[]).map_err(|e| e.to_string())?;
        // (profile, select, pause, menu, thumbstick); paths are per hand.
        for (profile, select_in, pause_in, menu_in, stick_in) in [
            (
                "/interaction_profiles/oculus/touch_controller",
                "trigger/value",
                &["right/input/a/click", "left/input/x/click"][..],
                &["right/input/b/click", "left/input/y/click"][..],
                Some("thumbstick"),
            ),
            (
                "/interaction_profiles/valve/index_controller",
                "trigger/click",
                &["right/input/a/click", "left/input/a/click"][..],
                &["right/input/b/click", "left/input/b/click"][..],
                Some("thumbstick"),
            ),
            ("/interaction_profiles/khr/simple_controller", "select/click", &[][..], &["right/input/menu/click"][..], None),
        ] {
            let mut b = Vec::new();
            for hand in ["left", "right"] {
                b.push(xr::Binding::new(&select, path(&format!("/user/hand/{hand}/input/{select_in}"))));
                b.push(xr::Binding::new(&aim, path(&format!("/user/hand/{hand}/input/aim/pose"))));
                if let Some(st) = stick_in {
                    b.push(xr::Binding::new(&stick, path(&format!("/user/hand/{hand}/input/{st}"))));
                }
            }
            b.extend(pause_in.iter().map(|p| xr::Binding::new(&pause, path(&format!("/user/hand/{p}")))));
            b.extend(menu_in.iter().map(|p| xr::Binding::new(&menu, path(&format!("/user/hand/{p}")))));
            if let Err(e) = instance.suggest_interaction_profile_bindings(path(profile), &b) {
                eprintln!("input: bindings for {profile} rejected: {e}");
            }
        }
        session.attach_action_sets(&[&actions]).map_err(|e| e.to_string())?;
        let aim_spaces = [
            aim.create_space(&session, hands[0], xr::Posef::IDENTITY).map_err(|e| e.to_string())?,
            aim.create_space(&session, hands[1], xr::Posef::IDENTITY).map_err(|e| e.to_string())?,
        ];
        let head = session
            .create_reference_space(xr::ReferenceSpaceType::VIEW, xr::Posef::IDENTITY)
            .map_err(|e| e.to_string())?;

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

        // --- Control panel: its own swapchain, filled by copies from a mapped buffer ---
        let mut panel_swapchain = session
            .create_swapchain(&xr::SwapchainCreateInfo {
                create_flags: xr::SwapchainCreateFlags::EMPTY,
                usage_flags: xr::SwapchainUsageFlags::COLOR_ATTACHMENT
                    | xr::SwapchainUsageFlags::TRANSFER_DST
                    | xr::SwapchainUsageFlags::SAMPLED,
                format: color_format.as_raw() as _,
                sample_count: 1,
                width: ui::W as u32,
                height: ui::H as u32,
                face_count: 1,
                array_size: 1,
                mip_count: 1,
            })
            .map_err(|e| format!("could not create panel swapchain: {e}"))?;
        let panel_images: Vec<vk::Image> = panel_swapchain
            .enumerate_images()
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(vk::Image::from_raw)
            .collect();
        let panel_bytes = ui::W * ui::H * 4;
        let panel_buffer = device
            .create_buffer(
                &vk::BufferCreateInfo::default().size(panel_bytes as u64).usage(vk::BufferUsageFlags::TRANSFER_SRC),
                None,
            )
            .map_err(vkerr)?;
        let panel_memory = {
            let req = device.get_buffer_memory_requirements(panel_buffer);
            let type_index = find_memory(
                &mem_props,
                req.memory_type_bits,
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
            )
            .ok_or("no host-visible memory")?;
            device
                .allocate_memory(
                    &vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(type_index),
                    None,
                )
                .map_err(vkerr)?
        };
        device.bind_buffer_memory(panel_buffer, panel_memory, 0).map_err(vkerr)?;
        let panel_pixels = std::slice::from_raw_parts_mut(
            device.map_memory(panel_memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty()).map_err(vkerr)? as *mut u8,
            panel_bytes,
        );
        let panel = ui::Panel::new(color_format == vk::Format::B8G8R8A8_SRGB);

        // --- Video textures: full-size luma (R8) and half-size chroma (R8G8) ---------
        let mut planes = Vec::new();
        for (format, w, h) in [(vk::Format::R8_UNORM, width, height), (vk::Format::R8G8_UNORM, width / 2, height / 2)] {
            let image = device
                .create_image(
                    &vk::ImageCreateInfo::default()
                        .image_type(vk::ImageType::TYPE_2D)
                        .format(format)
                        .extent(vk::Extent3D { width: w, height: h, depth: 1 })
                        .mip_levels(1)
                        .array_layers(1)
                        .samples(vk::SampleCountFlags::TYPE_1)
                        .tiling(vk::ImageTiling::OPTIMAL)
                        .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED)
                        .initial_layout(vk::ImageLayout::UNDEFINED),
                    None,
                )
                .map_err(|e| format!("could not create {w}x{h} video texture: {e}"))?;
            let req = device.get_image_memory_requirements(image);
            let memory = device
                .allocate_memory(
                    &vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(
                        find_memory(&mem_props, req.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)
                            .ok_or("no device memory for the video texture")?,
                    ),
                    None,
                )
                .map_err(vkerr)?;
            device.bind_image_memory(image, memory, 0).map_err(vkerr)?;
            let view = device
                .create_image_view(
                    &vk::ImageViewCreateInfo::default()
                        .image(image)
                        .view_type(vk::ImageViewType::TYPE_2D)
                        .format(format)
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
            planes.push(Plane { image, memory, view, width: w, height: h });
        }
        // Chroma starts right after the luma bytes in each decoded frame.
        let chroma_offset = width as u64 * height as u64;
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
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&[0, 1].map(|binding| {
                    vk::DescriptorSetLayoutBinding::default()
                        .binding(binding)
                        .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                        .descriptor_count(1)
                        .stage_flags(vk::ShaderStageFlags::FRAGMENT)
                })),
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
                    descriptor_count: 2,
                }]),
                None,
            )
            .map_err(vkerr)?;
        let descriptor_set = device
            .allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default().descriptor_pool(descriptor_pool).set_layouts(&[set_layout]),
            )
            .map_err(vkerr)?[0];
        let image_infos = planes.iter().map(|p| {
            [vk::DescriptorImageInfo {
                sampler,
                image_view: p.view,
                image_layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            }]
        });
        let image_infos: Vec<_> = image_infos.collect();
        let writes: Vec<_> = image_infos
            .iter()
            .enumerate()
            .map(|(binding, info)| {
                vk::WriteDescriptorSet::default()
                    .dst_set(descriptor_set)
                    .dst_binding(binding as u32)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .image_info(info)
            })
            .collect();
        device.update_descriptor_sets(&writes, &[]);

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
        // NV12: luma plus quarter-size chroma with two bytes per sample.
        let frame_bytes = width as usize * height as usize * 3 / 2;
        let mut stages = Vec::new();
        let mut ptrs = Vec::new();
        // A few frames of slack so a slow moment in decoding doesn't show.
        for _ in 0..6 {
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

        let decoder = video::Decoder::new(&src.video, audio.as_deref(), &info, (width, height), args.hw)?;
        eprintln!("video: decoding with {}", if decoder.is_hw() { "hardware (iris)" } else { "software" });
        let mut playback = decoder.start(args.start, slots.clone(), false)?;
        let duration = info.duration;
        let title = if src.title.is_empty() { args.input.clone() } else { src.title.clone() };

        // --- Main loop -------------------------------------------------------------
        let mut events = xr::EventDataBuffer::new();
        let mut running = false;
        let mut quitting = false;
        let mut have_frame = false;
        let mut texture_layout = vk::ImageLayout::UNDEFINED;
        let mut frames_uploaded: u64 = 0;
        let mut last_display: Option<i64> = None;
        let mut timing = Timing::default();
        let mut started = Instant::now();
        // Turn applied to the video so "Recenter" puts its front where you look.
        let mut recenter = xr::Quaternionf { x: 0.0, y: 0.0, z: 0.0, w: 1.0 };
        let mut panel_shown = true;
        let mut panel_pose: Option<xr::Posef> = None;
        // What the panel swapchain holds; None until the first draw.
        let mut panel_view: Option<ui::View> = None;
        let mut last_activity = Instant::now();
        let mut stick_latched = false;
        let mut was_ended = false;
        let tracked = xr::SpaceLocationFlags::ORIENTATION_VALID | xr::SpaceLocationFlags::POSITION_VALID;
        let place_panel = |head: xr::Posef| {
            let yaw = yaw_of(head.orientation);
            xr::Posef { orientation: yaw, position: add(head.position, rotate(yaw, xr::Vector3f { x: 0.0, y: -0.3, z: -1.3 })) }
        };

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
            let work_start = Instant::now();
            // Display timing: a gap of more than one display period between
            // our frames means the compositor showed an old one again.
            let period = state.predicted_display_period.as_nanos().max(1);
            if let Some(last) = last_display {
                if state.predicted_display_time.as_nanos() - last > period * 3 / 2 {
                    timing.missed += 1;
                }
            }
            last_display = Some(state.predicted_display_time.as_nanos());
            timing.frames += 1;
            if timing.frames == 900 {
                eprintln!(
                    "render: {} display frames at {:.1} ms, {} missed, slowest frame took {:.1} ms",
                    timing.frames,
                    period as f64 / 1e6,
                    timing.missed,
                    timing.slowest.as_secs_f64() * 1e3
                );
                timing = Timing::default();
            }
            frame_stream.begin().map_err(|e| e.to_string())?;
            let now = state.predicted_display_time;

            // --- Input ---------------------------------------------------------------
            session.sync_actions(&[(&actions).into()]).map_err(|e| e.to_string())?;
            let pressed = |a: &xr::Action<bool>, hand: xr::Path| {
                a.state(&session, hand)
                    .map(|s| s.is_active && s.changed_since_last_sync && s.current_state)
                    .unwrap_or(false)
            };
            let head_pose = head.locate(&space, now).ok().filter(|l| l.location_flags.contains(tracked)).map(|l| l.pose);
            if panel_shown && panel_pose.is_none() {
                panel_pose = head_pose.map(place_panel);
            }
            let mut pointers = [None, None];
            if panel_shown && let Some(pp) = panel_pose {
                for (hand, aim_space) in aim_spaces.iter().enumerate() {
                    if let Ok(l) = aim_space.locate(&space, now)
                        && l.location_flags.contains(tracked)
                    {
                        pointers[hand] = panel_hit(pp, l.pose);
                    }
                }
            }
            if pointers.iter().any(Option::is_some) {
                last_activity = Instant::now();
            }
            let hover = pointers.iter().flatten().find_map(|&(x, y)| ui::hit(x, y));

            let mut commands = Vec::new();
            let mut toggle_panel = pressed(&menu, xr::Path::NULL);
            for (hand, path) in hands.iter().enumerate() {
                if pressed(&select, *path) {
                    match pointers[hand] {
                        Some((x, y)) => commands.extend(ui::hit(x, y)),
                        // Clicking anywhere off the panel shows or hides it.
                        None => toggle_panel = true,
                    }
                }
            }
            if pressed(&pause, xr::Path::NULL) {
                commands.push(ui::Control::PlayPause);
            }
            let stick_x = stick
                .state(&session, xr::Path::NULL)
                .map(|s| if s.is_active { s.current_state.x } else { 0.0 })
                .unwrap_or(0.0);
            if !stick_latched && stick_x.abs() > 0.7 {
                stick_latched = true;
                commands.push(if stick_x > 0.0 { ui::Control::Forward } else { ui::Control::Back });
            } else if stick_x.abs() < 0.3 {
                stick_latched = false;
            }
            if toggle_panel {
                panel_shown = !panel_shown;
                if panel_shown {
                    panel_pose = head_pose.map(place_panel).or(panel_pose);
                }
                last_activity = Instant::now();
            }
            for command in commands {
                last_activity = Instant::now();
                let pos = playback.position();
                match command {
                    ui::Control::Back => seek(&decoder, &mut playback, &slots, pos - 10.0, duration),
                    ui::Control::Forward => seek(&decoder, &mut playback, &slots, pos + 10.0, duration),
                    ui::Control::Seek(f) => {
                        if let Some(d) = duration {
                            seek(&decoder, &mut playback, &slots, f as f64 * d, duration);
                        }
                    }
                    ui::Control::PlayPause => {
                        if playback.ended() {
                            seek(&decoder, &mut playback, &slots, 0.0, duration);
                        } else {
                            let paused = !playback.paused();
                            playback.set_paused(paused);
                            eprintln!("input: {}", if paused { "paused" } else { "playing" });
                        }
                    }
                    ui::Control::Picture => {
                        projection = projection.next();
                        check_cube = false;
                        eprintln!("input: projection {projection:?}");
                    }
                    ui::Control::Stereo => {
                        stereo = stereo.next();
                        eprintln!("input: stereo {stereo:?}");
                    }
                    ui::Control::Recenter => {
                        if let Some(h) = head_pose {
                            recenter = yaw_of(h.orientation);
                            panel_pose = Some(place_panel(h));
                        }
                    }
                    ui::Control::Quit => {
                        if !quitting {
                            quitting = true;
                            let _ = session.request_exit();
                        }
                    }
                }
            }
            // Show the panel when the video ends; hide it after a while of no use.
            if playback.ended() && !was_ended {
                panel_shown = true;
                panel_pose = head_pose.map(place_panel).or(panel_pose);
            }
            was_ended = playback.ended();
            if panel_shown && !playback.paused() && !playback.ended() && last_activity.elapsed() > Duration::from_secs(8) {
                panel_shown = false;
            }

            // The newest decoded frame whose time has come (the first one of a
            // new playback at once, so a seek shows its picture straight away).
            let due = playback.clock().unwrap_or(f64::NEG_INFINITY);
            let new_frame = slots.take_due(due, playback.fresh());
            if !state.should_render || (!have_frame && new_frame.is_none()) {
                if let Some((slot, _)) = new_frame {
                    slots.release(slot);
                }
                frame_stream.end(now, blend_mode, &[]).map_err(|e| e.to_string())?;
                continue;
            }

            let (_, views) = session.locate_views(VIEW_TYPE, now, &space).map_err(|e| e.to_string())?;
            let index = swapchain.acquire_image().map_err(|e| e.to_string())? as usize;
            swapchain.wait_image(xr::Duration::INFINITE).map_err(|e| e.to_string())?;

            device
                .begin_command_buffer(
                    cmd,
                    &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )
                .map_err(vkerr)?;

            if let Some((slot, pts)) = new_frame {
                playback.showing(pts);
                if check_cube && frames_uploaded % 15 == 0 {
                    match video::looks_like_eac(slots.slot(slot), width as usize, height as usize) {
                        Some(cube) => {
                            check_cube = false;
                            if cube {
                                projection = Projection::Cube360;
                                eprintln!("video: picture looks like YouTube's 360 cubemap");
                            }
                        }
                        None if frames_uploaded > 900 => check_cube = false,
                        None => {}
                    }
                }
                upload_frame(&device, cmd, stages[slot].0, &planes, chroma_offset, texture_layout);
                texture_layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
                have_frame = true;
                frames_uploaded += 1;
                if frames_uploaded % 300 == 0 {
                    // Rate over the last 300 frames, so pauses and seeks don't skew it.
                    eprintln!(
                        "video: {frames_uploaded} frames, {:.1} fps, {} dropped as late so far",
                        300.0 / started.elapsed().as_secs_f64(),
                        slots.skipped()
                    );
                    started = Instant::now();
                }
            }

            // Redraw the panel only when what it shows has changed.
            let mut panel_acquired = false;
            if panel_shown && panel_pose.is_some() {
                let view = ui::View {
                    title: title.clone(),
                    position: playback.position() as u32,
                    duration: duration.map(|d| d as u32),
                    paused: playback.paused(),
                    ended: playback.ended(),
                    picture: projection.label(),
                    stereo: stereo.label(),
                    hover,
                    cursors: pointers.iter().flatten().map(|&(x, y)| (x as i32, y as i32)).collect(),
                };
                if panel_view.as_ref() != Some(&view) {
                    let i = panel_swapchain.acquire_image().map_err(|e| e.to_string())? as usize;
                    panel_swapchain.wait_image(xr::Duration::INFINITE).map_err(|e| e.to_string())?;
                    panel.draw(&view, panel_pixels);
                    upload_panel(&device, cmd, panel_buffer, panel_images[i]);
                    panel_acquired = true;
                    panel_view = Some(view);
                }
            }

            let video_aspect = {
                let r = eye_rect(stereo, 0);
                (width as f32 * r[2]) / (height as f32 * r[3]).max(1.0)
            };
            let mode = match projection {
                Projection::Sphere360 => 0.0,
                Projection::Dome180 => 1.0,
                Projection::Flat => 2.0,
                Projection::Cube360 => 3.0,
            };
            let unturn = quat_inv(recenter);
            for (eye, view) in views.iter().enumerate().take(2) {
                let pos = rotate(unturn, view.pose.position);
                let c = EyeConstants {
                    rot: rotation_rows(quat_mul(unturn, view.pose.orientation)),
                    tans: [
                        view.fov.angle_left.tan(),
                        view.fov.angle_right.tan(),
                        view.fov.angle_up.tan(),
                        view.fov.angle_down.tan(),
                    ],
                    pos: [pos.x, pos.y, pos.z, 0.0],
                    rect: eye_rect(stereo, eye),
                    params: [mode, args.screen_m, args.screen_m / video_aspect, args.screen_m],
                    color: [info.bt601 as u32 as f32, info.full_range as u32 as f32, 0.0, 0.0],
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
            if let Some((slot, _)) = new_frame {
                slots.release(slot);
            }
            swapchain.release_image().map_err(|e| e.to_string())?;
            if panel_acquired {
                panel_swapchain.release_image().map_err(|e| e.to_string())?;
            }

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
            let projection_layer = xr::CompositionLayerProjection::new().space(&space).views(&projection_views);
            let quad_layer;
            let mut layers: Vec<&xr::CompositionLayerBase<xr::Vulkan>> = Vec::new();
            layers.push(&projection_layer);
            if panel_shown && panel_view.is_some() && let Some(pose) = panel_pose {
                quad_layer = xr::CompositionLayerQuad::new()
                    .space(&space)
                    .eye_visibility(xr::EyeVisibility::BOTH)
                    .sub_image(xr::SwapchainSubImage::new().swapchain(&panel_swapchain).image_rect(xr::Rect2Di {
                        offset: xr::Offset2Di { x: 0, y: 0 },
                        extent: xr::Extent2Di { width: ui::W as i32, height: ui::H as i32 },
                    }))
                    .pose(pose)
                    .size(xr::Extent2Df { width: ui::WIDTH_M, height: ui::HEIGHT_M });
                layers.push(&quad_layer);
            }
            frame_stream.end(now, blend_mode, &layers).map_err(|e| e.to_string())?;
            timing.slowest = timing.slowest.max(work_start.elapsed());
        }

        drop(playback);
        // OpenXR handles go before the Vulkan objects they use.
        drop((aim_spaces, head, panel_swapchain, swapchain, space, actions, session, frame_wait, frame_stream));
        let _ = device.device_wait_idle();
        device.destroy_buffer(panel_buffer, None);
        device.free_memory(panel_memory, None);
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
        for p in planes {
            device.destroy_image_view(p.view, None);
            device.destroy_image(p.image, None);
            device.free_memory(p.memory, None);
        }
        device.destroy_render_pass(render_pass, None);
        device.destroy_fence(fence, None);
        device.destroy_command_pool(pool, None);
        device.destroy_device(None);
        vk_instance.destroy_instance(None);
    }
    Ok(())
}

/// Record: panel pixels -> panel swapchain image, left as a color attachment
/// as OpenXR expects when the image is released.
unsafe fn upload_panel(device: &ash::Device, cmd: vk::CommandBuffer, src: vk::Buffer, image: vk::Image) {
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
                image_extent: vk::Extent3D { width: ui::W as u32, height: ui::H as u32, depth: 1 },
            }],
        );
        device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::ALL_COMMANDS,
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
    }
}

/// Display-side timing over the last stretch of frames, for the log.
#[derive(Default)]
struct Timing {
    frames: u32,
    missed: u32,
    slowest: Duration,
}

/// One plane of the video texture pair.
struct Plane {
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    width: u32,
    height: u32,
}

/// Record: staging buffer (NV12) -> luma and chroma textures, ending in
/// SHADER_READ_ONLY_OPTIMAL.
unsafe fn upload_frame(
    device: &ash::Device,
    cmd: vk::CommandBuffer,
    src: vk::Buffer,
    planes: &[Plane],
    chroma_offset: u64,
    old_layout: vk::ImageLayout,
) {
    let range = vk::ImageSubresourceRange {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        base_mip_level: 0,
        level_count: 1,
        base_array_layer: 0,
        layer_count: 1,
    };
    let barriers = |old, new, src_access, dst_access| -> Vec<vk::ImageMemoryBarrier> {
        planes
            .iter()
            .map(|p| {
                vk::ImageMemoryBarrier::default()
                    .old_layout(old)
                    .new_layout(new)
                    .src_access_mask(src_access)
                    .dst_access_mask(dst_access)
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .image(p.image)
                    .subresource_range(range)
            })
            .collect()
    };
    unsafe {
        device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::FRAGMENT_SHADER,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &barriers(
                old_layout,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::AccessFlags::SHADER_READ,
                vk::AccessFlags::TRANSFER_WRITE,
            ),
        );
        for (p, offset) in planes.iter().zip([0, chroma_offset]) {
            device.cmd_copy_buffer_to_image(
                cmd,
                src,
                p.image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[vk::BufferImageCopy {
                    buffer_offset: offset,
                    buffer_row_length: 0,
                    buffer_image_height: 0,
                    image_subresource: vk::ImageSubresourceLayers {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        mip_level: 0,
                        base_array_layer: 0,
                        layer_count: 1,
                    },
                    image_offset: vk::Offset3D::default(),
                    image_extent: vk::Extent3D { width: p.width, height: p.height, depth: 1 },
                }],
            );
        }
        device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::FRAGMENT_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &barriers(
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                vk::AccessFlags::TRANSFER_WRITE,
                vk::AccessFlags::SHADER_READ,
            ),
        );
    }
}
