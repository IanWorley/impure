//! Shows the guest's frames in the window: a sprite whose texture each swap copies the presented
//! front buffer into, and the hand-over of Bevy's GPU device to the guest renderer.

use bevy::asset::RenderAssetUsages;
use bevy::camera::ScalingMode;
use bevy::prelude::*;
use bevy::render::extract_resource::{ExtractResource, ExtractResourcePlugin};
use bevy::render::render_asset::RenderAssets;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat, TextureUsages};
use bevy::render::renderer::{RenderAdapterInfo, RenderDevice, RenderQueue};
use bevy::render::texture::GpuImage;
use bevy::render::{Render, RenderApp, RenderSystems};

use crate::renderer;

/// The console's front buffer: 720p.
const FRONT_BUFFER_WIDTH: u32 = 1280;
const FRONT_BUFFER_HEIGHT: u32 = 720;
/// Shown until the first guest frame arrives.
const OPAQUE_BLACK: [u8; 4] = [0, 0, 0, u8::MAX];

#[derive(Resource, Clone, ExtractResource)]
struct DisplayImage(Handle<Image>);

pub struct DisplayPlugin;

impl Plugin for DisplayPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(ExtractResourcePlugin::<DisplayImage>::default())
            .add_systems(Startup, spawn_display)
            .add_systems(Update, start_renderer);
        app.sub_app_mut(RenderApp)
            .add_systems(Render, share_display.in_set(RenderSystems::Prepare));
    }
}

/// A camera that fits the front buffer to the window, and the sprite showing it.
fn spawn_display(mut commands: Commands, mut images: ResMut<Assets<Image>>) {
    let size = Extent3d {
        width: FRONT_BUFFER_WIDTH,
        height: FRONT_BUFFER_HEIGHT,
        depth_or_array_layers: 1,
    };
    // The front buffer holds gamma-encoded bytes; as an sRGB texture drawn to an sRGB window
    // they reach the screen unchanged.
    let mut image = Image::new_fill(
        size,
        TextureDimension::D2,
        &OPAQUE_BLACK,
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::RENDER_WORLD,
    );
    // The renderer draws into it through a plain (not sRGB) view, and copies it out to capture
    // frames.
    image.texture_descriptor.usage =
        TextureUsages::TEXTURE_BINDING | TextureUsages::RENDER_ATTACHMENT | TextureUsages::COPY_SRC;
    image.texture_descriptor.view_formats = &[TextureFormat::Rgba8Unorm];
    let handle = images.add(image);
    commands.spawn((
        Camera2d,
        // The sprite only shows the front buffer, so multisampling (Bevy's default) would just
        // cost a multisampled pass and a resolve per window frame.
        Msaa::Off,
        Projection::Orthographic(OrthographicProjection {
            scaling_mode: ScalingMode::AutoMin {
                min_width: FRONT_BUFFER_WIDTH as f32,
                min_height: FRONT_BUFFER_HEIGHT as f32,
            },
            ..OrthographicProjection::default_2d()
        }),
    ));
    commands.spawn(Sprite::from_image(handle.clone()));
    commands.insert_resource(DisplayImage(handle));
}

/// Hands Bevy's device and queue to the guest renderer once they exist.
fn start_renderer(
    device: Option<Res<RenderDevice>>,
    queue: Option<Res<RenderQueue>>,
    adapter: Option<Res<RenderAdapterInfo>>,
    mut started: Local<bool>,
) {
    if *started {
        return;
    }
    if let (Some(device), Some(queue), Some(adapter)) = (device, queue, adapter) {
        let software = adapter.device_type == wgpu::DeviceType::Cpu;
        renderer::install(device.wgpu_device().clone(), (****queue).clone(), software);
        *started = true;
    }
}

/// Gives the renderer the display sprite's GPU texture once it has been created.
fn share_display(
    images: Res<RenderAssets<GpuImage>>,
    display: Option<Res<DisplayImage>>,
    mut shared: Local<bool>,
) {
    if *shared {
        return;
    }
    if let Some(image) = display.and_then(|display| images.get(&display.0)) {
        renderer::set_display((*image.texture).clone());
        *shared = true;
    }
}
