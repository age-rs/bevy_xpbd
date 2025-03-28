use avian3d::{math::*, prelude::*};
use bevy::prelude::*;
use examples_common_3d::ExampleCommonPlugin;

fn main() {
    let mut app = App::new();

    app.add_plugins((DefaultPlugins, ExampleCommonPlugin));

    // Add PhysicsPlugins and replace the default broad phase with our custom broad phase.
    app.add_plugins(
        PhysicsPlugins::default()
            .build()
            .disable::<BroadPhasePlugin>()
            .add(BruteForceBroadPhasePlugin),
    );

    app.add_systems(Startup, setup).run();
}

// Modified from Bevy's 3d_scene example, a cube falling to the ground with velocity
fn setup(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    // Plane
    commands.spawn((
        Mesh3d(meshes.add(Plane3d::default().mesh().size(8.0, 8.0))),
        MeshMaterial3d(materials.add(Color::srgb(0.3, 0.5, 0.3))),
        RigidBody::Static,
        Collider::cuboid(8.0, 0.002, 8.0),
    ));
    // Cube
    commands.spawn((
        Mesh3d(meshes.add(Cuboid::default())),
        MeshMaterial3d(materials.add(Color::srgb(0.8, 0.7, 0.6))),
        Transform::from_xyz(0.0, 4.0, 0.0),
        RigidBody::Dynamic,
        AngularVelocity(Vector::new(2.5, 3.4, 1.6)),
        Collider::cuboid(1.0, 1.0, 1.0),
    ));
    // Light
    commands.spawn((
        PointLight {
            intensity: 2_000_000.0,
            shadows_enabled: true,
            ..default()
        },
        Transform::from_xyz(4.0, 8.0, 4.0),
    ));
    // Camera
    commands.spawn((
        Camera3d::default(),
        Transform::from_xyz(-4.0, 6.5, 8.0).looking_at(Vec3::ZERO, Vec3::Y),
    ));
}

// Collects pairs of potentially colliding entities into the BroadCollisionPairs resource provided by the physics engine.
// A brute force algorithm is used for simplicity.
pub struct BruteForceBroadPhasePlugin;

impl Plugin for BruteForceBroadPhasePlugin {
    fn build(&self, app: &mut App) {
        // Initialize the resource that the collision pairs are added to.
        app.init_resource::<BroadCollisionPairs>();

        // Make sure the PhysicsSchedule is available.
        let physics_schedule = app
            .get_schedule_mut(PhysicsSchedule)
            .expect("add PhysicsSchedule first");

        // Add the broad phase system into the broad phase set
        physics_schedule.add_systems(collect_collision_pairs.in_set(PhysicsStepSet::BroadPhase));
    }
}

fn collect_collision_pairs(
    bodies: Query<(Entity, &ColliderAabb, &RigidBody)>,
    mut broad_collision_pairs: ResMut<BroadCollisionPairs>,
) {
    // Clear old collision pairs.
    broad_collision_pairs.0.clear();

    // Loop through all entity combinations and collect pairs of bodies with intersecting AABBs.
    for [(ent_a, aabb_a, rb_a), (ent_b, aabb_b, rb_b)] in bodies.iter_combinations() {
        // At least one of the bodies is dynamic and their AABBs intersect
        if (rb_a.is_dynamic() || rb_b.is_dynamic()) && aabb_a.intersects(aabb_b) {
            broad_collision_pairs.0.push((ent_a, ent_b));
        }
    }
}
