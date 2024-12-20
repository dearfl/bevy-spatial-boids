use bevy::{
    color::palettes::css::GRAY,
    input::common_conditions::input_just_released,
    math::Vec3Swizzles,
    prelude::*,
    render::{mesh::*, render_asset::RenderAssetUsages},
    tasks::ComputeTaskPool,
};
use bevy_spatial::{kdtree::KDTree2, AutomaticUpdate, SpatialAccess, SpatialStructure};
use halton::Sequence;
use rand::prelude::*;
use std::time::Duration;

const WINDOW_BOUNDS: Vec2 = Vec2::new(800., 400.);
const NEIGHBOR_CAP: usize = 100;
const BOID_BOUNDARY_SIZE: f32 = 150.;
const BOID_COUNT: i32 = 256;
const BOID_SIZE: f32 = 7.5;
const BOID_VIS_RANGE: f32 = 40.;
const VIS_RANGE_SQ: f32 = BOID_VIS_RANGE * BOID_VIS_RANGE;
const BOID_PROT_RANGE: f32 = 8.;
// https://en.wikipedia.org/wiki/Bird_vision#Extraocular_anatomy
const BOID_FOV: f32 = 120. * std::f32::consts::PI / 180.;
const PROT_RANGE_SQ: f32 = BOID_PROT_RANGE * BOID_PROT_RANGE;
const BOID_CENTER_FACTOR: f32 = 0.0005;
const BOID_MATCHING_FACTOR: f32 = 0.05;
const BOID_AVOID_FACTOR: f32 = 0.05;
const BOID_TURN_FACTOR: f32 = 0.2;
const BOID_MOUSE_CHASE_FACTOR: f32 = 0.0005;
const BOID_MIN_SPEED: f32 = 2.0;
const BOID_MAX_SPEED: f32 = 4.0;

fn main() {
    App::new()
        .add_plugins((
            DefaultPlugins.set(WindowPlugin {
                primary_window: Some(Window {
                    canvas: Some("#bevy_boids_canvas".into()),
                    resolution: WINDOW_BOUNDS.into(),
                    resizable: true,
                    ..default()
                }),
                ..default()
            }),
            // Track boids in the KD-Tree
            AutomaticUpdate::<Boid>::new()
                // TODO: check perf of other tree types
                .with_spatial_ds(SpatialStructure::KDTree2)
                .with_frequency(Duration::from_millis(16)),
        ))
        .insert_resource(Time::<Fixed>::from_hz(60.0))
        .add_event::<DvEvent>()
        .add_systems(Startup, setup)
        .add_systems(
            FixedUpdate,
            (flocking_system, velocity_system, movement_system).chain(),
        )
        .add_systems(
            Update,
            (
                draw_boid_gizmos,
                exit.run_if(input_just_released(KeyCode::Escape)),
            ),
        )
        .run();
}

#[derive(Component, Default)]
struct Velocity(Vec2);

impl Velocity {
    pub fn random() -> Self {
        let mut rng = rand::rng();
        Velocity(Vec2::new(
            rng.random_range(-1.0..1.0),
            rng.random_range(-1.0..1.0),
        ))
    }
}

// Marker for entities tracked by KDTree
#[derive(Component, Default)]
#[require(Velocity, Mesh2d, MeshMaterial2d<ColorMaterial>, Transform)]
struct Boid;

impl Boid {
    pub fn mesh(meshes: &mut ResMut<Assets<Mesh>>) -> Mesh2d {
        let mesh = Mesh::new(
            PrimitiveTopology::TriangleList,
            RenderAssetUsages::default(),
        )
        .with_inserted_attribute(
            Mesh::ATTRIBUTE_POSITION,
            vec![
                [-0.5, 0.5, 0.0],
                [1.0, 0.0, 0.0],
                [-0.5, -0.5, 0.0],
                [0.0, 0.0, 0.0],
            ],
        )
        .with_inserted_indices(Indices::U32(vec![1, 3, 0, 1, 2, 3]));
        Mesh2d(meshes.add(mesh))
    }
}

// Event for a change of velocity on some boid
#[derive(Event)]
struct DvEvent(Entity, Vec2);

fn setup(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<ColorMaterial>>,
    window: Single<&Window>,
) {
    commands.spawn(Camera2d);

    let mut rng = rand::rng();

    // Halton sequence for Boid spawns
    let seq = halton::Sequence::new(2)
        .zip(Sequence::new(3))
        .zip(1..BOID_COUNT);

    let res = &window.resolution;
    let mesh = Boid::mesh(&mut meshes);

    for ((x, y), idx) in seq {
        let spawn_x = (x as f32 * res.width()) - res.width() / 2.0;
        let spawn_y = (y as f32 * res.height()) - res.height() / 2.0;

        // give each bird a distinct depth so they overlap in a consistent way
        let depth = idx as f32 / BOID_COUNT as f32;
        let mut transform =
            Transform::from_xyz(spawn_x, spawn_y, depth).with_scale(Vec3::splat(BOID_SIZE));

        transform.rotate_z(0.0);

        let velocity = Velocity::random();
        let material = MeshMaterial2d(materials.add(
            // Random color for each boid
            Color::hsl(360. * rng.random::<f32>(), rng.random(), 0.7),
        ));

        commands.spawn((mesh.clone(), material, velocity, transform, Boid));
    }
}

fn draw_boid_gizmos(window: Single<&Window>, mut gizmos: Gizmos) {
    let res = &window.resolution;

    gizmos.rect_2d(
        Isometry2d::default(),
        Vec2::new(
            res.width() - BOID_BOUNDARY_SIZE,
            res.height() - BOID_BOUNDARY_SIZE,
        ),
        GRAY,
    );
}

fn angle_towards(a: Vec2, b: Vec2) -> f32 {
    // https://stackoverflow.com/a/68929139
    let dir = b - a;
    dir.y.atan2(dir.x)
}

fn flocking_dv(
    kdtree: &Res<KDTree2<Boid>>,
    boid_query: &Query<(Entity, &Velocity, &Transform), With<Boid>>,
    camera: &Single<(&Camera, &GlobalTransform)>,
    window: &Single<&Window>,
    boid: &Entity,
    t0: &&Transform,
) -> Vec2 {
    // https://vanhunteradams.com/Pico/Animal_Movement/Boids-algorithm.html
    let mut dv = Vec2::default();
    let mut vec_away = Vec2::default();
    let mut avg_position = Vec2::default();
    let mut avg_velocity = Vec2::default();
    let mut neighboring_boids = 0;
    let mut close_boids = 0;

    for (_, entity) in kdtree.k_nearest_neighbour(t0.translation.xy(), NEIGHBOR_CAP) {
        let Ok((other, v1, t1)) = boid_query.get(entity.unwrap()) else {
            todo!()
        };

        // Don't evaluate against itself
        if *boid == other {
            continue;
        }

        let vec_to = (t1.translation - t0.translation).xy();
        let dist_sq = vec_to.x * vec_to.x + vec_to.y * vec_to.y;

        // Don't evaluate boids out of range
        if dist_sq > VIS_RANGE_SQ {
            continue;
        }

        // Don't evaluate boids behind
        if let Some(vec_to_norm) = vec_to.try_normalize() {
            if t0
                .rotation
                .angle_between(Quat::from_rotation_arc_2d(Vec2::X, vec_to_norm))
                > BOID_FOV
            {
                continue;
            }
        }

        if dist_sq < PROT_RANGE_SQ {
            // separation
            vec_away -= vec_to;
            close_boids += 1;
        } else {
            // cohesion
            avg_position += vec_to;
            // alignment
            avg_velocity += v1.0;
            neighboring_boids += 1;
        }
    }

    if neighboring_boids > 0 {
        let neighbors = neighboring_boids as f32;
        dv += avg_position / neighbors * BOID_CENTER_FACTOR;
        dv += avg_velocity / neighbors * BOID_MATCHING_FACTOR;
    }

    if close_boids > 0 {
        let close = close_boids as f32;
        dv += vec_away / close * BOID_AVOID_FACTOR;
    }

    // Chase the mouse
    let (camera, t_camera) = **camera;
    if let Some(c_window) = window.cursor_position() {
        if let Ok(c_world) = camera.viewport_to_world_2d(t_camera, c_window) {
            let to_cursor = c_world - t0.translation.xy();
            dv += to_cursor * BOID_MOUSE_CHASE_FACTOR;
        }
    }

    dv
}

fn flocking_system(
    boid_query: Query<(Entity, &Velocity, &Transform), With<Boid>>,
    kdtree: Res<KDTree2<Boid>>,
    mut dv_event_writer: EventWriter<DvEvent>,
    camera: Single<(&Camera, &GlobalTransform)>,
    window: Single<&Window>,
) {
    let pool = ComputeTaskPool::get();
    let boids = boid_query.iter().collect::<Vec<_>>();
    let boids_per_thread = boids.len().div_ceil(pool.thread_num());

    // https://docs.rs/bevy/latest/bevy/tasks/struct.ComputeTaskPool.html
    // https://github.com/kvietcong/rusty-boids
    for batch in pool.scope(|s| {
        for chunk in boids.chunks(boids_per_thread) {
            let kdtree = &kdtree;
            let boid_query = &boid_query;
            let camera = &camera;
            let window = &window;

            s.spawn(async move {
                let mut dv_batch: Vec<DvEvent> = vec![];

                for (boid, _, t0) in chunk {
                    dv_batch.push(DvEvent(
                        *boid,
                        flocking_dv(kdtree, boid_query, camera, window, boid, t0),
                    ));
                }

                dv_batch
            });
        }
    }) {
        dv_event_writer.send_batch(batch);
    }
}

fn velocity_system(
    mut events: EventReader<DvEvent>,
    mut boids: Query<(&mut Velocity, &mut Transform)>,
    window: Single<&Window>,
) {
    for DvEvent(boid, dv) in events.read() {
        let Ok((mut velocity, transform)) = boids.get_mut(*boid) else {
            todo!()
        };

        velocity.0.x += dv.x;
        velocity.0.y += dv.y;

        let res = &window.resolution;

        let width = (res.width() - BOID_BOUNDARY_SIZE) / 2.;
        let height = (res.height() - BOID_BOUNDARY_SIZE) / 2.;

        // Steer back into visible region
        if transform.translation.x < -width {
            velocity.0.x += BOID_TURN_FACTOR;
        }
        if transform.translation.x > width {
            velocity.0.x -= BOID_TURN_FACTOR;
        }
        if transform.translation.y < -height {
            velocity.0.y += BOID_TURN_FACTOR;
        }
        if transform.translation.y > height {
            velocity.0.y -= BOID_TURN_FACTOR;
        }

        // Clamp speed
        let speed = velocity.0.length();

        if speed < BOID_MIN_SPEED {
            velocity.0 *= BOID_MIN_SPEED / speed;
        }
        if speed > BOID_MAX_SPEED {
            velocity.0 *= BOID_MAX_SPEED / speed;
        }
    }
}

fn movement_system(mut query: Query<(&mut Velocity, &mut Transform)>) {
    for (velocity, mut transform) in query.iter_mut() {
        transform.rotation = Quat::from_axis_angle(Vec3::Z, angle_towards(Vec2::ZERO, velocity.0));
        transform.translation.x += velocity.0.x;
        transform.translation.y += velocity.0.y;
    }
}

fn exit(mut exit: EventWriter<AppExit>) {
    exit.send(AppExit::Success);
}
