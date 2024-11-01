//! A Dear ImGui integration for the Bevy game engine.
//!
//! # Minimal Example
//!
//! ```no_run
//! use bevy::prelude::*;
//! use bevy_mod_imgui::prelude::*;
//!
//! #[derive(Resource)]
//! struct ImguiState {
//!     demo_window_open: bool,
//! }
//!
//! fn main() {
//!     let mut app = App::new();
//!     app.insert_resource(ClearColor(Color::rgba(0.2, 0.2, 0.2, 1.0)))
//!         .insert_resource(ImguiState {
//!             demo_window_open: true,
//!         })
//!         .add_plugins(DefaultPlugins)
//!         .add_plugins(bevy_mod_imgui::ImguiPlugin::default())
//!         .add_systems(Startup, |mut commands: Commands| {
//!             commands.spawn(Camera3dBundle::default());
//!         })
//!         .add_systems(Update, imgui_example_ui);
//!     app.run();
//! }
//!
//! fn imgui_example_ui(mut context: NonSendMut<ImguiContext>, mut state: ResMut<ImguiState>) {
//!     let ui = context.ui();
//!     if state.demo_window_open {
//!         ui.show_demo_window(&mut state.demo_window_open);
//!     }
//! }
//! ```

use bevy::{
    asset::StrongHandle,
    core_pipeline::core_2d::graph::{Core2d, Node2d},
    core_pipeline::core_3d::graph::{Core3d, Node3d},
    ecs::system::SystemState,
    input::{
        keyboard::{Key, KeyboardInput},
        ButtonState,
    },
    prelude::*,
    render::{
        render_asset::RenderAssets,
        render_graph::{Node, NodeRunError, RenderGraphApp, RenderGraphContext, RenderLabel},
        renderer::{RenderContext, RenderDevice, RenderQueue},
        texture::GpuImage,
        view::ExtractedWindows,
        MainWorld, Render, RenderApp, RenderSet,
    },
    window::PrimaryWindow,
};
use imgui::{FontSource, OwnedDrawData, TextureId};
mod imgui_wgpu_rs_local;
use imgui_wgpu_rs_local::{Renderer, RendererConfig, Texture};
use std::{
    cell::RefCell,
    collections::HashMap,
    ops::{Deref, DerefMut},
    path::PathBuf,
    ptr::null_mut,
    rc::Rc,
    sync::{Arc, Mutex},
};
use wgpu::{
    CommandEncoder, LoadOp, Operations, RenderPass, RenderPassColorAttachment,
    RenderPassDescriptor, StoreOp, TextureFormat,
};

/// The ImGui context resource.
///
/// This should be added to your Bevy app as a `NonSendMut` resource (as it is not thread safe).
///
/// You can use this object to obtain a reference to the underlying `imgui::Ui` object for submitting
/// UI elements to imgui. This should be done during the Update and PostUpdate phase only.
pub struct ImguiContext {
    ctx: Rc<Mutex<imgui::Context>>,
    ui: *mut imgui::Ui,
    textures: HashMap<imgui::TextureId, Arc<StrongHandle>>,
    textures_to_add: Vec<imgui::TextureId>,
    textures_to_remove: Vec<imgui::TextureId>,
    textures_next_free_id: usize,
}

impl ImguiContext {
    /// Provides mutable access to the underlying `imgui::Ui` object.
    ///
    /// Use this to submit UI elements to imgui.
    pub fn ui(&mut self) -> &mut imgui::Ui {
        unsafe { &mut *self.ui }
    }

    /// Register a Bevy texture with ImGui. The provided Handle must be strong, and
    /// the texture will be kept alive until `unregister_bevy_texture` is called to
    /// release the texture.
    /// This function returns an `imgui::TextureId` that can be immediately used with
    /// the underlying ImGui context.
    pub fn register_bevy_texture(&mut self, handle: Handle<Image>) -> imgui::TextureId {
        // We require strong handles here to ensure the image is alive at the point that
        // it is registered. Once it is registered, the we maintain a strong handle to
        // the asset until it is unregistered in order to ensure the texture is always
        // available for imgui to use
        if let Handle::Strong(strong) = handle {
            let result = TextureId::new(self.textures_next_free_id);
            self.textures.insert(result, strong.clone());
            self.textures_to_add.push(result);
            self.textures_next_free_id += 1;
            result
        } else {
            panic!("register_bevy_texture requires a strong Handle<Image>");
        }
    }

    /// Unregister a Bevy texture with ImGui. The texture must have previously been
    /// registered with `register_bevy_texture` - this function expects the
    /// `imgui::TextureId` returned by `register_bevy_texture` to be to be passed here.
    pub fn unregister_bevy_texture(&mut self, texture_id: &TextureId) {
        self.textures.remove(texture_id);
        self.textures_to_remove.push(*texture_id);
    }
}

#[derive(Resource)]
struct ImguiRenderContext {
    ctx: Rc<Mutex<imgui::Context>>,
    renderer: RefCell<Renderer>,
    texture_format: TextureFormat,
    draw: imgui::OwnedDrawData,
    plugin: ImguiPlugin,
    display_scale: f32,
    textures_to_add: HashMap<TextureId, Arc<StrongHandle>>,
    textures_to_remove: Vec<TextureId>,
}

unsafe impl Send for ImguiRenderContext {}
unsafe impl Sync for ImguiRenderContext {}

/// The label used by the render node responsible for rendering ImGui
#[derive(Debug, Hash, PartialEq, Eq, Clone, RenderLabel)]
pub struct ImGuiNodeLabel;

struct ImGuiNode;

impl ImGuiNode {
    fn create_render_pass<'a>(
        command_encoder: &'a mut CommandEncoder,
        world: &'a World,
    ) -> Result<RenderPass<'a>, ()> {
        let extracted_windows = &world.get_resource::<ExtractedWindows>().unwrap();
        let Some(primary) = extracted_windows.primary else {
            return Err(()); // No primary window
        };
        let Some(extracted_window) = extracted_windows.windows.get(&primary) else {
            return Err(()); // No primary window
        };
        let swap_chain_texture_view = if let Some(swap_chain_texture_view) =
            extracted_window.swap_chain_texture_view.as_ref()
        {
            swap_chain_texture_view
        } else {
            return Err(()); // No swapchain texture
        };

        Ok(command_encoder.begin_render_pass(&RenderPassDescriptor {
            label: None,
            color_attachments: &[Some(RenderPassColorAttachment {
                view: swap_chain_texture_view,
                resolve_target: None,
                ops: Operations {
                    load: LoadOp::Load,
                    store: StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            ..Default::default()
        }))
    }
}

impl Node for ImGuiNode {
    fn run(
        &self,
        _graph: &mut RenderGraphContext,
        render_context: &mut RenderContext,
        world: &World,
    ) -> Result<(), NodeRunError> {
        let context = world.resource::<ImguiRenderContext>();
        let queue = world.get_resource::<RenderQueue>().unwrap();
        let render_device = world.get_resource::<RenderDevice>().unwrap();
        let command_encoder = render_context.command_encoder();
        let wgpu_device = render_device.wgpu_device();
        let mut renderer = context.renderer.borrow_mut();
        if let Ok(mut rpass) = ImGuiNode::create_render_pass(command_encoder, world) {
            if let Some(draw_data) = context.draw.draw_data() {
                renderer
                    .render(draw_data, queue, wgpu_device, &mut rpass)
                    .unwrap();
            }
        }
        Ok(())
    }
}

impl FromWorld for ImGuiNode {
    fn from_world(_world: &mut World) -> ImGuiNode {
        ImGuiNode {}
    }
}

// Adds an Image's render resources to the renderer
fn add_image_to_renderer(
    texture_id: &TextureId,
    strong: &Arc<StrongHandle>,
    gpu_images: &RenderAssets<GpuImage>,
    renderer: &mut Renderer,
    device: &RenderDevice,
) {
    let handle = Handle::<Image>::Strong(strong.clone());
    if let Some(gpu_image) = gpu_images.get(&handle) {
        let texture_arc = unsafe {
            let const_ptr = gpu_image.texture.deref() as *const wgpu::Texture;
            std::sync::Arc::increment_strong_count(const_ptr);
            std::sync::Arc::from_raw(const_ptr as *mut wgpu::Texture)
        };

        let view_arc = unsafe {
            let const_ptr = gpu_image.texture_view.deref() as *const wgpu::TextureView;
            std::sync::Arc::increment_strong_count(const_ptr);
            std::sync::Arc::from_raw(const_ptr as *mut wgpu::TextureView)
        };

        let config = imgui_wgpu_rs_local::RawTextureConfig {
            label: Some("Bevy Texture for ImGui"),
            sampler_desc: wgpu::SamplerDescriptor {
                label: Some("Bevy Texture Sampler for ImGui"),
                address_mode_u: wgpu::AddressMode::ClampToEdge,
                address_mode_v: wgpu::AddressMode::ClampToEdge,
                address_mode_w: wgpu::AddressMode::ClampToEdge,
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                mipmap_filter: wgpu::FilterMode::Linear,
                lod_min_clamp: 0.0,
                lod_max_clamp: 100.0,
                compare: None,
                anisotropy_clamp: 1,
                border_color: None,
            },
        };

        let texture = Texture::from_raw_parts(
            device.wgpu_device(),
            renderer,
            texture_arc,
            view_arc.clone(),
            None,
            Some(&config),
            wgpu::Extent3d {
                width: gpu_image.texture.width(),
                height: gpu_image.texture.height(),
                ..Default::default()
            },
        );

        renderer.textures.replace(*texture_id, texture);
    } else {
        // We require the texture to be loaded before it is registered because otherwise, its
        // corresponding TextureId could start being used in Imgui calls, at which point we
        // don't have enough knowledge of the intended user interface to cope with unloaded / failed
        // images (e.g. we could display a checkerboard texture, but an equally valid desired
        // behaviour could be to not emit the controls that draw the image in the first place).
        // Because we don't have enough context to act accordingly here, we choose instead to
        // ensure the caller (who owns the images) ensures that textures are loaded before registering.
        panic!("Could not obtain GPU image for texture. Please ensure textures are loaded prior to registering them with imgui");
    }
}

// Update the display scale and reload the font accordingly.
// This must be performed during Extract as it is the only safe
// point where we can update the context AND regenerate the font atlas
fn update_display_scale(
    previous_display_scale: f32,
    display_scale: f32,
    plugin_settings: &ImguiPlugin,
    context: &mut ImguiContext,
    renderer: &mut Renderer,
    device: &RenderDevice,
    queue: &RenderQueue,
) {
    let mut ctx = context.ctx.lock().unwrap();
    let font_scale = if plugin_settings.apply_display_scale_to_font_size {
        display_scale
    } else {
        1.0
    };

    let font_oversample_scale = if plugin_settings.apply_display_scale_to_font_oversample {
        display_scale.ceil() as i32
    } else {
        1
    };

    let io = ctx.deref_mut().io_mut();
    io.display_framebuffer_scale = [display_scale, display_scale];
    io.font_global_scale = 1.0 / font_scale;

    // Reload font.
    ctx.fonts().clear();
    ctx.fonts().add_font(&[FontSource::DefaultFontData {
        config: Some(imgui::FontConfig {
            size_pixels: f32::floor(plugin_settings.font_size * font_scale), // Round down to nearest integer, as per https://github.com/ocornut/imgui/blob/master/docs/FAQ.md#q-how-should-i-handle-dpi-in-my-application
            oversample_h: plugin_settings.font_oversample_h * font_oversample_scale,
            oversample_v: plugin_settings.font_oversample_v * font_oversample_scale,
            ..default()
        }),
    }]);

    // We have no means of iterating over the textures in Textures, so remove all
    // of the textures we previously added...
    for texture_id in context.textures.keys() {
        renderer.textures.remove(*texture_id);
    }

    // At this point, the only texture left is the font texture. This function
    // may update this...
    renderer.reload_font_texture(ctx.deref_mut(), device.wgpu_device(), queue);

    // Flag all textures as needing to be re-added
    for texture_id in context.textures.keys() {
        context.textures_to_add.push(*texture_id);
    }

    // The only new texture that may have been created is the font texture, so the next
    // free id is either the current free id, or one past the font texture
    let font_texture_id = ctx.fonts().tex_id;
    context.textures_next_free_id =
        usize::max(font_texture_id.id() + 1, context.textures_next_free_id);

    // Update style for DPI change, as per:
    // https://github.com/ocornut/imgui/blob/master/docs/FAQ.md#q-how-should-i-handle-dpi-in-my-application
    ctx.style_mut()
        .scale_all_sizes(display_scale / previous_display_scale);
}

/// Configuration settings for this plugin
#[derive(Clone)]
pub struct ImguiPlugin {
    /// Sets the path to the ini file (default is "imgui.ini").
    /// Pass None to disable automatic .Ini saving
    pub ini_filename: Option<PathBuf>,

    /// The unscaled font size to use (default is 13).
    pub font_size: f32,

    /// The number of horizontal font samples to perform. Must be >= 1 (default is 1).
    pub font_oversample_h: i32,

    /// The number of vertical font samples to perform. Must be >= 1 (default is 1).
    pub font_oversample_v: i32,

    /// Whether to apply the window display scale to the font size (default is true).
    pub apply_display_scale_to_font_size: bool,

    /// Whether to apply the window display scale to the number of font samples (default is true).
    pub apply_display_scale_to_font_oversample: bool,
}

impl Default for ImguiPlugin {
    fn default() -> Self {
        Self {
            ini_filename: Default::default(),
            font_size: 13.0,
            font_oversample_h: 1,
            font_oversample_v: 1,
            apply_display_scale_to_font_size: true,
            apply_display_scale_to_font_oversample: true,
        }
    }
}

impl Plugin for ImguiPlugin {
    fn build(&self, _app: &mut App) {}

    fn finish(&self, app: &mut App) {
        let mut ctx = imgui::Context::create();
        ctx.set_ini_filename(self.ini_filename.clone());

        for key_index in 0..imgui::Key::COUNT {
            ctx.io_mut()[imgui::Key::VARIANTS[key_index]] = key_index as _;
        }

        let display_scale = {
            let mut system_state: SystemState<Query<&Window, With<PrimaryWindow>>> =
                SystemState::new(app.world_mut());
            let primary_window = system_state.get(app.world());
            primary_window.get_single().unwrap().scale_factor()
        };

        let rc = Rc::new(Mutex::new(ctx));
        let mut context = ImguiContext {
            ctx: rc.clone(),
            ui: null_mut(),
            textures: HashMap::new(),
            textures_next_free_id: 0,
            textures_to_add: Vec::new(),
            textures_to_remove: Vec::new(),
        };

        if let Some(render_app) = app.get_sub_app_mut(RenderApp) {
            let mut system_state: SystemState<(Res<RenderDevice>, Res<RenderQueue>)> =
                SystemState::new(render_app.world_mut());
            let (device, queue) = system_state.get_mut(render_app.world_mut());
            let ctx_rc = rc.clone();

            // Here we create a new ImGui renderer with a default format. At this point,
            // we don't know what format the window surface is going to be set up with,
            // and yet we need to initialise the renderer so that the texture glyphs
            // are created before new_frame is called on the imgui context.
            //
            // This will give us a functonal imgui context. If, at the point at which we
            // extract the scene, we realise that the window has an incompatible
            // format, the renderer will be recreated with a compatible format.
            let renderer_config = RendererConfig::default();
            let texture_format = renderer_config.texture_format;
            let mut renderer = Renderer::new(
                &mut ctx_rc.lock().unwrap(),
                device.wgpu_device(),
                &queue,
                renderer_config,
            );
            update_display_scale(
                1.0,
                display_scale,
                self,
                &mut context,
                &mut renderer,
                &device,
                &queue,
            );

            render_app.add_render_graph_node::<ImGuiNode>(Core2d, ImGuiNodeLabel);

            render_app.add_render_graph_edges(Core2d, (Node2d::EndMainPass, ImGuiNodeLabel));

            render_app.add_render_graph_edges(
                Core2d,
                (Node2d::EndMainPassPostProcessing, ImGuiNodeLabel),
            );

            render_app.add_render_graph_edges(Core2d, (Node2d::Upscaling, ImGuiNodeLabel));

            render_app.add_render_graph_node::<ImGuiNode>(Core3d, ImGuiNodeLabel);

            render_app.add_render_graph_edges(Core3d, (Node3d::EndMainPass, ImGuiNodeLabel));

            render_app.add_render_graph_edges(
                Core3d,
                (Node3d::EndMainPassPostProcessing, ImGuiNodeLabel),
            );

            render_app.add_render_graph_edges(Core3d, (Node3d::Upscaling, ImGuiNodeLabel));

            render_app.insert_resource(ImguiRenderContext {
                ctx: ctx_rc,
                renderer: RefCell::new(renderer),
                texture_format,
                draw: OwnedDrawData::default(),
                plugin: self.clone(),
                display_scale,
                textures_to_add: HashMap::new(),
                textures_to_remove: Vec::new(),
            });

            render_app.add_systems(ExtractSchedule, imgui_extract_frame_system);
            render_app.add_systems(
                Render,
                imgui_update_textures_system.in_set(RenderSet::Prepare),
            );
        } else {
            return;
        }

        app.insert_non_send_resource(context);

        app.add_systems(PreUpdate, imgui_new_frame_system);
    }
}

fn imgui_new_frame_system(
    mut context: NonSendMut<ImguiContext>,
    primary_window: Query<(Entity, &Window), With<PrimaryWindow>>,
    keyboard: Res<ButtonInput<KeyCode>>,
    mouse: Res<ButtonInput<bevy::input::mouse::MouseButton>>,
    mut received_chars: EventReader<KeyboardInput>,
    mut mouse_wheel: EventReader<bevy::input::mouse::MouseWheel>,
) {
    const UNKNOWN_KEYCODE: KeyCode = KeyCode::F35;
    const IMGUI_TO_BEVY_KEYS: [bevy::input::keyboard::KeyCode; imgui::Key::COUNT] = [
        KeyCode::Tab,
        KeyCode::ArrowLeft,
        KeyCode::ArrowRight,
        KeyCode::ArrowUp,
        KeyCode::ArrowDown,
        KeyCode::PageUp,
        KeyCode::PageDown,
        KeyCode::Home,
        KeyCode::End,
        KeyCode::Insert,
        KeyCode::Delete,
        KeyCode::Backspace,
        KeyCode::Space,
        KeyCode::Enter,
        KeyCode::Escape,
        KeyCode::ControlLeft,
        KeyCode::ShiftLeft,
        KeyCode::AltLeft,
        KeyCode::SuperLeft,
        KeyCode::ControlRight,
        KeyCode::ShiftRight,
        KeyCode::AltRight,
        KeyCode::SuperRight,
        KeyCode::ContextMenu, // sys::ImGuiKey_Menu
        KeyCode::Digit0,
        KeyCode::Digit1,
        KeyCode::Digit2,
        KeyCode::Digit3,
        KeyCode::Digit4,
        KeyCode::Digit5,
        KeyCode::Digit6,
        KeyCode::Digit7,
        KeyCode::Digit8,
        KeyCode::Digit9,
        KeyCode::KeyA,
        KeyCode::KeyB,
        KeyCode::KeyC,
        KeyCode::KeyD,
        KeyCode::KeyE,
        KeyCode::KeyF,
        KeyCode::KeyG,
        KeyCode::KeyH,
        KeyCode::KeyI,
        KeyCode::KeyJ,
        KeyCode::KeyK,
        KeyCode::KeyL,
        KeyCode::KeyM,
        KeyCode::KeyN,
        KeyCode::KeyO,
        KeyCode::KeyP,
        KeyCode::KeyQ,
        KeyCode::KeyR,
        KeyCode::KeyS,
        KeyCode::KeyT,
        KeyCode::KeyU,
        KeyCode::KeyV,
        KeyCode::KeyW,
        KeyCode::KeyX,
        KeyCode::KeyY,
        KeyCode::KeyZ,
        KeyCode::F1,
        KeyCode::F2,
        KeyCode::F3,
        KeyCode::F4,
        KeyCode::F5,
        KeyCode::F6,
        KeyCode::F7,
        KeyCode::F8,
        KeyCode::F9,
        KeyCode::F10,
        KeyCode::F11,
        KeyCode::F12,
        KeyCode::Quote,
        KeyCode::Comma,
        KeyCode::Minus,
        KeyCode::Period,
        KeyCode::Slash,
        KeyCode::Semicolon,
        KeyCode::Equal,
        KeyCode::BracketLeft,
        KeyCode::Backslash,
        KeyCode::BracketRight,
        KeyCode::Backquote,
        KeyCode::CapsLock,
        KeyCode::ScrollLock,
        KeyCode::NumLock,
        KeyCode::PrintScreen,
        KeyCode::Pause,
        KeyCode::Numpad0,
        KeyCode::Numpad1,
        KeyCode::Numpad2,
        KeyCode::Numpad3,
        KeyCode::Numpad4,
        KeyCode::Numpad5,
        KeyCode::Numpad6,
        KeyCode::Numpad7,
        KeyCode::Numpad8,
        KeyCode::Numpad9,
        KeyCode::NumpadDecimal,
        KeyCode::NumpadDivide,
        KeyCode::NumpadMultiply,
        KeyCode::NumpadSubtract,
        KeyCode::NumpadAdd,
        KeyCode::NumpadEnter,
        KeyCode::NumpadEqual,
        UNKNOWN_KEYCODE, // GamepadStart = sys::ImGuiKey_GamepadStart,
        UNKNOWN_KEYCODE, // GamepadBack = sys::ImGuiKey_GamepadBack,
        UNKNOWN_KEYCODE, // GamepadFaceLeft = sys::ImGuiKey_GamepadFaceLeft,
        UNKNOWN_KEYCODE, // GamepadFaceRight = sys::ImGuiKey_GamepadFaceRight,
        UNKNOWN_KEYCODE, // GamepadFaceUp = sys::ImGuiKey_GamepadFaceUp,
        UNKNOWN_KEYCODE, // GamepadFaceDown = sys::ImGuiKey_GamepadFaceDown,
        UNKNOWN_KEYCODE, // GamepadDpadLeft = sys::ImGuiKey_GamepadDpadLeft,
        UNKNOWN_KEYCODE, // GamepadDpadRight = sys::ImGuiKey_GamepadDpadRight,
        UNKNOWN_KEYCODE, // GamepadDpadUp = sys::ImGuiKey_GamepadDpadUp,
        UNKNOWN_KEYCODE, // GamepadDpadDown = sys::ImGuiKey_GamepadDpadDown,
        UNKNOWN_KEYCODE, // GamepadL1 = sys::ImGuiKey_GamepadL1,
        UNKNOWN_KEYCODE, // GamepadR1 = sys::ImGuiKey_GamepadR1,
        UNKNOWN_KEYCODE, // GamepadL2 = sys::ImGuiKey_GamepadL2,
        UNKNOWN_KEYCODE, // GamepadR2 = sys::ImGuiKey_GamepadR2,
        UNKNOWN_KEYCODE, // GamepadL3 = sys::ImGuiKey_GamepadL3,
        UNKNOWN_KEYCODE, // GamepadR3 = sys::ImGuiKey_GamepadR3,
        UNKNOWN_KEYCODE, // GamepadLStickLeft = sys::ImGuiKey_GamepadLStickLeft,
        UNKNOWN_KEYCODE, // GamepadLStickRight = sys::ImGuiKey_GamepadLStickRight,
        UNKNOWN_KEYCODE, // GamepadLStickUp = sys::ImGuiKey_GamepadLStickUp,
        UNKNOWN_KEYCODE, // GamepadLStickDown = sys::ImGuiKey_GamepadLStickDown,
        UNKNOWN_KEYCODE, // GamepadRStickLeft = sys::ImGuiKey_GamepadRStickLeft,
        UNKNOWN_KEYCODE, // GamepadRStickRight = sys::ImGuiKey_GamepadRStickRight,
        UNKNOWN_KEYCODE, // GamepadRStickUp = sys::ImGuiKey_GamepadRStickUp,
        UNKNOWN_KEYCODE, // GamepadRStickDown = sys::ImGuiKey_GamepadRStickDown,
        UNKNOWN_KEYCODE, // MouseLeft = sys::ImGuiKey_MouseLeft,
        UNKNOWN_KEYCODE, // MouseRight = sys::ImGuiKey_MouseRight,
        UNKNOWN_KEYCODE, // MouseMiddle = sys::ImGuiKey_MouseMiddle,
        UNKNOWN_KEYCODE, // MouseX1 = sys::ImGuiKey_MouseX1,
        UNKNOWN_KEYCODE, // MouseX2 = sys::ImGuiKey_MouseX2,
        UNKNOWN_KEYCODE, // MouseWheelX = sys::ImGuiKey_MouseWheelX,
        UNKNOWN_KEYCODE, // MouseWheelY = sys::ImGuiKey_MouseWheelY,
        UNKNOWN_KEYCODE, // ReservedForModCtrl = sys::ImGuiKey_ReservedForModCtrl,
        UNKNOWN_KEYCODE, // ReservedForModShift = sys::ImGuiKey_ReservedForModShift,
        UNKNOWN_KEYCODE, // ReservedForModAlt = sys::ImGuiKey_ReservedForModAlt,
        UNKNOWN_KEYCODE, // ReservedForModSuper = sys::ImGuiKey_ReservedForModSuper
    ];

    let ui_ptr: *mut imgui::Ui;
    {
        let mut ctx = context.ctx.lock().unwrap();
        let io = ctx.io_mut();

        if let Ok((_, primary)) = primary_window.get_single() {
            io.display_size = [primary.width(), primary.height()];
            io.display_framebuffer_scale = [primary.scale_factor(), primary.scale_factor()];

            if let Some(pos) = primary.cursor_position() {
                io.mouse_pos = [pos.x, pos.y];
            }
        }

        io.mouse_down[0] = mouse.pressed(bevy::input::mouse::MouseButton::Left);
        io.mouse_down[1] = mouse.pressed(bevy::input::mouse::MouseButton::Right);
        io.mouse_down[2] = mouse.pressed(bevy::input::mouse::MouseButton::Middle);

        for e in received_chars.read() {
            if e.state == ButtonState::Pressed {
                match &e.logical_key {
                    Key::Character(c) => {
                        io.add_input_character(c.chars().last().unwrap());
                    }
                    Key::Dead(Some(c)) => {
                        io.add_input_character(*c);
                    }
                    Key::Space => {
                        io.add_input_character(' ');
                    }
                    _ => {}
                }
            }
        }

        for (key_index, key) in IMGUI_TO_BEVY_KEYS.iter().enumerate() {
            io.keys_down[key_index] = keyboard.pressed(*key);
        }

        io.key_alt = keyboard.pressed(KeyCode::AltLeft) || keyboard.pressed(KeyCode::AltRight);
        io.key_ctrl =
            keyboard.pressed(KeyCode::ControlLeft) || keyboard.pressed(KeyCode::ControlRight);
        io.key_shift =
            keyboard.pressed(KeyCode::ShiftLeft) || keyboard.pressed(KeyCode::ShiftRight);
        io.key_super =
            keyboard.pressed(KeyCode::SuperLeft) || keyboard.pressed(KeyCode::SuperRight);

        for e in mouse_wheel.read() {
            io.mouse_wheel = e.y;
            io.mouse_wheel_h = e.x;
        }
        ui_ptr = ctx.new_frame();
    }
    context.ui = ui_ptr;
}

fn imgui_extract_frame_system(
    mut world: ResMut<MainWorld>,
    mut context: ResMut<ImguiRenderContext>,
    extracted_windows: ResMut<ExtractedWindows>,
    device: Res<RenderDevice>,
    queue: ResMut<RenderQueue>,
) {
    // End the imgui frame.
    let owned_draw_data = {
        let mut ctx = context.ctx.lock().unwrap();
        let draw_data = ctx.render();
        OwnedDrawData::from(draw_data)
    };

    // Get the current display scale and ImGuiContext
    let (display_scale, cpu_context) = {
        let mut system_state: SystemState<Query<&Window, With<PrimaryWindow>>> =
            SystemState::new(&mut world);
        let primary_window = system_state.get(&world);
        if let Ok(single) = primary_window.get_single() {
            (
                single.scale_factor(),
                world.get_non_send_resource_mut::<ImguiContext>(),
            )
        } else {
            // Fall back to the previously captured display scale. This can happen during app shutdown.
            (
                context.display_scale,
                world.get_non_send_resource_mut::<ImguiContext>(),
            )
        }
    };

    let Some(mut cpu_context) = cpu_context else {
        return;
    };

    // Determine the current texture format of the primary window
    let Some(primary) = extracted_windows.primary else {
        return;
    };
    let Some(extracted_window) = extracted_windows.windows.get(&primary) else {
        return;
    };
    let Some(texture_format) = extracted_window.swap_chain_texture_format else {
        return;
    };

    // We've now recorded the draw data for the current frame, and this should be renderer agnostic.
    // So at this point, we can check to see whether the texture format of the target window matches
    // the renderer's texture format. If it doesn't, we recreate the Renderer here before we proceed
    // to render the frame.
    // We also recreate the renderer and the font atlas if the system display scale has changed, since
    // this is the only safe point in the frame to do so.
    context.draw = OwnedDrawData::default();
    if texture_format != context.texture_format || context.display_scale != display_scale {
        let renderer_config = RendererConfig {
            texture_format,
            ..default()
        };
        context.renderer.swap(&RefCell::new(Renderer::new(
            &mut context.ctx.lock().unwrap(),
            device.wgpu_device(),
            &queue,
            renderer_config,
        )));
        context.texture_format = texture_format;

        update_display_scale(
            context.display_scale,
            display_scale,
            &context.plugin,
            cpu_context.deref_mut(),
            &mut context.renderer.borrow_mut(),
            &device,
            &queue,
        );
        context.display_scale = display_scale;

        // Re-add all textures
        for texture_id in cpu_context.textures.keys() {
            context
                .textures_to_add
                .insert(*texture_id, cpu_context.textures[texture_id].clone());
        }
    } else {
        // Just add the textures that have been registered this frame
        for texture_id in &cpu_context.textures_to_add {
            context
                .textures_to_add
                .insert(*texture_id, cpu_context.textures[texture_id].clone());
        }
    }
    context
        .textures_to_remove
        .clone_from(&cpu_context.textures_to_remove);
    context.draw = owned_draw_data;
    cpu_context.textures_to_add.clear();
    cpu_context.textures_to_remove.clear();
}

fn imgui_update_textures_system(
    mut context: ResMut<ImguiRenderContext>,
    device: Res<RenderDevice>,
    gpu_images: Res<RenderAssets<GpuImage>>,
) {
    // Remove all textures that are flagged for removal
    let textures_to_remove = context.textures_to_remove.clone();
    context.textures_to_remove.clear();
    for texture_id in &textures_to_remove {
        context.renderer.borrow_mut().textures.remove(*texture_id);
        context.textures_to_add.remove(texture_id);
    }

    // Add new textures
    let mut added_textures = Vec::<TextureId>::new();
    for (texture_id, handle) in &context.textures_to_add {
        let mut renderer = context.renderer.borrow_mut();
        add_image_to_renderer(texture_id, handle, &gpu_images, &mut renderer, &device);
        added_textures.push(*texture_id);
    }
    for texture_id in &added_textures {
        context.textures_to_add.remove(texture_id);
    }
}

pub mod prelude {
    pub use crate::*;
    pub use imgui::*;
}
