//! Manages and solves contacts, joints, and other constraints.
//!
//! See [`SolverPlugin`].

pub mod contact;
pub mod joints;
pub mod schedule;
pub mod softness_parameters;
pub mod solver_body;
pub mod xpbd;

mod diagnostics;
pub use diagnostics::SolverDiagnostics;
use solver_body::{SolverBody, SolverBodyInertia, SolverBodyPlugin};
use xpbd::EntityConstraint;

use crate::prelude::*;
use bevy::prelude::*;
use core::cmp::Ordering;
use schedule::SubstepSolverSet;

use self::{
    contact::ContactConstraint,
    softness_parameters::{SoftnessCoefficients, SoftnessParameters},
};

/// Manages and solves contacts, joints, and other constraints.
///
/// Note that the [`ContactConstraints`] are currently generated by tbe [`NarrowPhasePlugin`].
///
/// # Implementation
///
/// Avian uses an impulse-based solver with substepping and [soft constraints](softness_parameters).
/// Warm starting is used to improve convergence, along with a relaxation pass to reduce overshooting.
///
/// [Speculative collision](dynamics::ccd#speculative-collision) is used by default to prevent tunneling.
/// Optional [sweep-based Continuous Collision Detection (CCD)](dynamics::ccd#swept-ccd) is handled by the [`CcdPlugin`].
///
/// [Joints](joints) and user constraints are currently solved using [Extended Position-Based Dynamics (XPBD)](xpbd).
/// In the future, they may transition to an impulse-based approach as well.
///
/// ## Solver Bodies
///
/// The solver maintains a [`SolverBody`] for each awake dynamic and kinematic body.
/// It stores the body data needed by the solver in a more optimized format
/// with better memory locality.
///
/// Only awake dynamic bodies and kinematic bodies have an associated solver body.
/// Static bodies and sleeping dynamic bodies do not move and are not included in the solver.
///
/// The [`SolverBodyPlugin`] is added for managing solver bodies and synchronizing them with rigid body data.
///
/// # Steps
///
/// Below are the main steps of the `SolverPlugin`.
///
/// 1. Generate and prepare constraints (contact constraints are generated by the [`NarrowPhasePlugin`])
/// 2. [Prepare solver bodies](SolverSet::PrepareSolverBodies)
/// 3. [Prepare joint constraints](SolverSet::PrepareJoints)
/// 4. Substepping loop (runs the [`SubstepSchedule`] [`SubstepCount`] times)
///     1. [Integrate velocities](super::integrator::IntegrationSet::Velocity)
///     2. [Warm start](SubstepSolverSet::WarmStart)
///     3. [Solve constraints with bias](SubstepSolverSet::SolveConstraints)
///     4. [Integrate positions](super::integrator::IntegrationSet::Position)
///     5. [Solve constraints without bias to relax velocities](SubstepSolverSet::Relax)
///     6. [Solve XPBD constraints (joints)](SubstepSolverSet::SolveXpbdConstraints)
///     7. [Solve user-defined constraints](SubstepSolverSet::SolveUserConstraints)
///     8. [Update velocities after XPBD constraint solving.](SubstepSolverSet::XpbdVelocityProjection)
/// 5. [Apply restitution](SolverSet::Restitution)
/// 6. [Write back solver body data to rigid bodies](SolverSet::Finalize)
/// 7. [Store contact impulses for next frame's warm starting](SolverSet::StoreContactImpulses)
pub struct SolverPlugin {
    length_unit: Scalar,
}

impl Default for SolverPlugin {
    fn default() -> Self {
        Self::new_with_length_unit(1.0)
    }
}

impl SolverPlugin {
    /// Creates a [`SolverPlugin`] with the given approximate dimensions of most objects.
    ///
    /// The length unit will be used for initializing the [`PhysicsLengthUnit`]
    /// resource unless it already exists.
    pub fn new_with_length_unit(unit: Scalar) -> Self {
        Self { length_unit: unit }
    }
}

impl Plugin for SolverPlugin {
    fn build(&self, app: &mut App) {
        // Add the `SolverBodyPlugin` to manage solver bodies and synchronize them with rigid body data.
        app.add_plugins(SolverBodyPlugin);

        app.init_resource::<SolverConfig>()
            .init_resource::<ContactSoftnessCoefficients>()
            .init_resource::<ContactConstraints>();

        if app
            .world()
            .get_resource::<PhysicsLengthUnit>()
            .is_none_or(|unit| unit.0 == 1.0)
        {
            app.insert_resource(PhysicsLengthUnit(self.length_unit));
        }

        // Get the `PhysicsSchedule`, and panic if it doesn't exist.
        let physics = app
            .get_schedule_mut(PhysicsSchedule)
            .expect("add PhysicsSchedule first");

        physics.add_systems(update_contact_softness.before(PhysicsStepSet::NarrowPhase));

        // Prepare joints before the substepping loop.
        physics.add_systems(
            (
                xpbd::prepare_xpbd_joint::<FixedJoint>,
                xpbd::prepare_xpbd_joint::<RevoluteJoint>,
                xpbd::prepare_xpbd_joint::<PrismaticJoint>,
                xpbd::prepare_xpbd_joint::<DistanceJoint>,
                #[cfg(feature = "3d")]
                xpbd::prepare_xpbd_joint::<SphericalJoint>,
            )
                .chain()
                .in_set(SolverSet::PrepareJoints),
        );

        // Apply restitution.
        physics.add_systems(solve_restitution.in_set(SolverSet::Restitution));

        // Store the current contact impulses for the next frame's warm starting.
        physics.add_systems(store_contact_impulses.in_set(SolverSet::StoreContactImpulses));

        // Get the `SubstepSchedule`, and panic if it doesn't exist.
        let substeps = app
            .get_schedule_mut(SubstepSchedule)
            .expect("add SubstepSchedule first");

        // Warm start the impulses.
        // This applies the impulses stored from the previous substep,
        // which improves convergence.
        substeps.add_systems(warm_start.in_set(SubstepSolverSet::WarmStart));

        // Solve velocities using a position bias.
        substeps.add_systems(solve_contacts::<true>.in_set(SubstepSolverSet::SolveConstraints));

        // Relax biased velocities and impulses.
        // This reduces overshooting caused by warm starting.
        substeps.add_systems(solve_contacts::<false>.in_set(SubstepSolverSet::Relax));

        // Solve joints with XPBD.
        substeps.add_systems(
            (
                |mut query: Query<
                    (
                        &SolverBody,
                        &mut PreSolveDeltaPosition,
                        &mut PreSolveDeltaRotation,
                    ),
                    Without<RigidBodyDisabled>,
                >| {
                    for (body, mut pre_solve_delta_position, mut pre_solve_delta_rotation) in
                        &mut query
                    {
                        // Store the previous delta translation and rotation for XPBD velocity updates.
                        pre_solve_delta_position.0 = body.delta_position;
                        pre_solve_delta_rotation.0 = body.delta_rotation;
                    }
                },
                xpbd::solve_xpbd_joint::<FixedJoint>,
                xpbd::solve_xpbd_joint::<RevoluteJoint>,
                #[cfg(feature = "3d")]
                xpbd::solve_xpbd_joint::<SphericalJoint>,
                xpbd::solve_xpbd_joint::<PrismaticJoint>,
                xpbd::solve_xpbd_joint::<DistanceJoint>,
            )
                .chain()
                .in_set(SubstepSolverSet::SolveXpbdConstraints),
        );

        // Perform XPBD velocity updates after constraint solving.
        substeps.add_systems(
            (
                xpbd::project_linear_velocity,
                xpbd::project_angular_velocity,
                joint_damping::<FixedJoint>,
                joint_damping::<RevoluteJoint>,
                #[cfg(feature = "3d")]
                joint_damping::<SphericalJoint>,
                joint_damping::<PrismaticJoint>,
                joint_damping::<DistanceJoint>,
            )
                .chain()
                .in_set(SubstepSolverSet::XpbdVelocityProjection),
        );
    }

    fn finish(&self, app: &mut App) {
        // Register timer and counter diagnostics for the solver.
        app.register_physics_diagnostics::<SolverDiagnostics>();
    }
}

// TODO: Where should this type be and which plugin should initialize it?
/// A units-per-meter scaling factor that adjusts the engine's internal properties
/// to the scale of the world.
///
/// For example, a 2D game might use pixels as units and have an average object size
/// of around 100 pixels. By setting the length unit to `100.0`, the physics engine
/// will interpret 100 pixels as 1 meter for internal thresholds, improving stability.
///
/// Note that this is *not* used to scale forces or any other user-facing inputs or outputs.
/// Instead, the value is only used to scale some internal length-based tolerances, such as
/// [`SleepingThreshold::linear`] and [`NarrowPhaseConfig::default_speculative_margin`],
/// as well as the scale used for [debug rendering](PhysicsDebugPlugin).
///
/// Choosing the appropriate length unit can help improve stability and robustness.
///
/// Default: `1.0`
///
/// # Example
///
/// The [`PhysicsLengthUnit`] can be inserted as a resource like normal,
/// but it can also be specified through the [`PhysicsPlugins`] plugin group.
///
/// ```no_run
/// # #[cfg(feature = "2d")]
/// use avian2d::prelude::*;
/// use bevy::prelude::*;
///
/// # #[cfg(feature = "2d")]
/// fn main() {
///     App::new()
///         .add_plugins((
///             DefaultPlugins,
///             // A 2D game with 100 pixels per meter
///             PhysicsPlugins::default().with_length_unit(100.0),
///         ))
///         .run();
/// }
/// # #[cfg(not(feature = "2d"))]
/// # fn main() {} // Doc test needs main
/// ```
#[derive(Resource, Clone, Debug, Deref, DerefMut, PartialEq, Reflect)]
#[reflect(Resource)]
pub struct PhysicsLengthUnit(pub Scalar);

impl Default for PhysicsLengthUnit {
    fn default() -> Self {
        Self(1.0)
    }
}

/// Configuration parameters for the constraint solver that handles
/// things like contacts and joints.
///
/// These are tuned to give good results for most applications, but can
/// be configured if more control over the simulation behavior is needed.
#[derive(Resource, Clone, Debug, PartialEq, Reflect)]
#[reflect(Resource)]
pub struct SolverConfig {
    /// The damping ratio used for contact stabilization.
    ///
    /// Lower values make contacts more compliant or "springy",
    /// allowing more visible penetration before overlap has been
    /// resolved and the contact has been stabilized.
    ///
    /// Consider using a higher damping ratio if contacts seem too soft.
    /// Note that making the value too large can cause instability.
    ///
    /// Default: `10.0`.
    pub contact_damping_ratio: Scalar,

    /// Scales the frequency used for contacts. A higher frequency
    /// makes contact responses faster and reduces visible springiness,
    /// but can hurt stability.
    ///
    /// The solver computes the frequency using the time step and substep count,
    /// and limits the maximum frequency to be at most half of the time step due to
    /// [Nyquist's theorem](https://en.wikipedia.org/wiki/Nyquist%E2%80%93Shannon_sampling_theorem).
    /// This factor scales the resulting frequency, which can lead to unstable behavior
    /// if the factor is too large.
    ///
    /// Default: `1.5`
    pub contact_frequency_factor: Scalar,

    /// The maximum speed at which overlapping bodies are pushed apart by the solver.
    ///
    /// With a small value, overlap is resolved gently and gradually, while large values
    /// can result in more snappy behavior.
    ///
    /// This is implicitly scaled by the [`PhysicsLengthUnit`].
    ///
    /// Default: `4.0`
    pub max_overlap_solve_speed: Scalar,

    /// The coefficient in the `[0, 1]` range applied to
    /// [warm start](SubstepSolverSet::WarmStart) impulses.
    ///
    /// Warm starting uses the impulses from the previous frame as the initial
    /// solution for the current frame. This helps the solver reach the desired
    /// state much faster, meaning that *convergence* is improved.
    ///
    /// The coefficient should typically be set to `1.0`.
    ///
    /// Default: `1.0`
    pub warm_start_coefficient: Scalar,

    /// The minimum speed along the contact normal in units per second
    /// for [restitution](Restitution) to be applied.
    ///
    /// An appropriate threshold should typically be small enough that objects
    /// keep bouncing until the bounces are effectively unnoticeable,
    /// but large enough that restitution is not applied unnecessarily,
    /// improving performance and stability.
    ///
    /// This is implicitly scaled by the [`PhysicsLengthUnit`].
    ///
    /// Default: `1.0`
    pub restitution_threshold: Scalar,

    /// The number of iterations used for applying [restitution](Restitution).
    ///
    /// A higher number of iterations can result in more accurate bounces,
    /// but it only makes a difference when there are more than one contact point.
    ///
    /// For example, with just one iteration, a cube falling flat on the ground
    /// might bounce and rotate to one side, because the impulses are applied
    /// to the corners sequentially, and some of the impulses are likely to be larger
    /// than the others. With multiple iterations, the impulses are applied more evenly.
    ///
    /// Default: `1`
    pub restitution_iterations: usize,
}

impl Default for SolverConfig {
    fn default() -> Self {
        Self {
            contact_damping_ratio: 10.0,
            contact_frequency_factor: 1.5,
            max_overlap_solve_speed: 4.0,
            warm_start_coefficient: 1.0,
            restitution_threshold: 1.0,
            restitution_iterations: 1,
        }
    }
}

/// The [`SoftnessCoefficients`] used for contacts.
///
/// **Note**: This resource is updated automatically and not intended to be modified manually.
/// Use the [`SolverConfig`] resource instead for tuning contact behavior.
#[derive(Resource, Clone, Copy, PartialEq, Reflect)]
#[reflect(Resource)]
pub struct ContactSoftnessCoefficients {
    /// The [`SoftnessCoefficients`] used for contacts against dynamic bodies.
    pub dynamic: SoftnessCoefficients,
    /// The [`SoftnessCoefficients`] used for contacts against static or kinematic bodies.
    pub non_dynamic: SoftnessCoefficients,
}

impl Default for ContactSoftnessCoefficients {
    fn default() -> Self {
        Self {
            dynamic: SoftnessParameters::new(10.0, 30.0).compute_coefficients(1.0 / 60.0),
            non_dynamic: SoftnessParameters::new(10.0, 60.0).compute_coefficients(1.0 / 60.0),
        }
    }
}

fn update_contact_softness(
    mut coefficients: ResMut<ContactSoftnessCoefficients>,
    solver_config: Res<SolverConfig>,
    physics_time: Res<Time<Physics>>,
    substep_time: Res<Time<Substeps>>,
) {
    if solver_config.is_changed() || physics_time.is_changed() || substep_time.is_changed() {
        let dt = physics_time.delta_secs_f64() as Scalar;
        let h = substep_time.delta_secs_f64() as Scalar;

        // The contact frequency should at most be half of the time step due to Nyquist's theorem.
        // https://en.wikipedia.org/wiki/Nyquist%E2%80%93Shannon_sampling_theorem
        let max_hz = 1.0 / (dt * 2.0);
        let hz = solver_config.contact_frequency_factor * max_hz.min(0.25 / h);

        coefficients.dynamic = SoftnessParameters::new(solver_config.contact_damping_ratio, hz)
            .compute_coefficients(h);

        // TODO: Perhaps the non-dynamic softness should be configurable separately.
        // Make contacts against static and kinematic bodies stiffer to avoid clipping through the environment.
        coefficients.non_dynamic =
            SoftnessParameters::new(solver_config.contact_damping_ratio, 2.0 * hz)
                .compute_coefficients(h);
    }
}

/// A resource that stores the contact constraints.
#[derive(Resource, Default, Deref, DerefMut)]
pub struct ContactConstraints(pub Vec<ContactConstraint>);

/// Warm starts the solver by applying the impulses from the previous frame or substep.
///
/// See [`SubstepSolverSet::WarmStart`] for more information.
fn warm_start(
    bodies: Query<(&mut SolverBody, &SolverBodyInertia)>,
    mut constraints: ResMut<ContactConstraints>,
    solver_config: Res<SolverConfig>,
    mut diagnostics: ResMut<SolverDiagnostics>,
) {
    let start = crate::utils::Instant::now();

    let mut dummy_body1 = SolverBody::default();
    let mut dummy_body2 = SolverBody::default();
    let dummy_inertia = SolverBodyInertia::default();

    for constraint in constraints.iter_mut() {
        debug_assert!(!constraint.points.is_empty());

        let (mut body1, mut inertia1) = (&mut dummy_body1, &dummy_inertia);
        let (mut body2, mut inertia2) = (&mut dummy_body2, &dummy_inertia);

        // Get the solver bodies for the two colliding entities.
        if let Ok((body, inertia)) = unsafe { bodies.get_unchecked(constraint.body1) } {
            body1 = body.into_inner();
            inertia1 = inertia;
        }
        if let Ok((body, inertia)) = unsafe { bodies.get_unchecked(constraint.body2) } {
            body2 = body.into_inner();
            inertia2 = inertia;
        }

        // If a body has a higher dominance, it is treated as a static or kinematic body.
        match constraint.relative_dominance.cmp(&0) {
            Ordering::Greater => inertia1 = &dummy_inertia,
            Ordering::Less => inertia2 = &dummy_inertia,
            _ => {}
        }

        let normal = constraint.normal;
        let tangent_directions =
            constraint.tangent_directions(body1.linear_velocity, body2.linear_velocity);

        constraint.warm_start(
            body1,
            body2,
            inertia1,
            inertia2,
            normal,
            tangent_directions,
            solver_config.warm_start_coefficient,
        );
    }

    diagnostics.warm_start += start.elapsed();
}

/// Solves contacts by iterating through the given contact constraints
/// and applying impulses to colliding rigid bodies.
///
/// This solve is done `iterations` times. With a substepped solver,
/// `iterations` should typically be `1`, as substeps will handle the iteration.
///
/// If `use_bias` is `true`, the impulses will be boosted to account for overlap.
/// The solver should often be run twice per frame or substep: first with the bias,
/// and then without it to *relax* the velocities and reduce overshooting caused by
/// [warm starting](SubstepSolverSet::WarmStart).
///
/// See [`SubstepSolverSet::SolveConstraints`] and [`SubstepSolverSet::Relax`] for more information.
#[allow(clippy::too_many_arguments)]
#[allow(clippy::type_complexity)]
fn solve_contacts<const USE_BIAS: bool>(
    bodies: Query<(&mut SolverBody, &SolverBodyInertia)>,
    mut constraints: ResMut<ContactConstraints>,
    solver_config: Res<SolverConfig>,
    length_unit: Res<PhysicsLengthUnit>,
    time: Res<Time>,
    mut diagnostics: ResMut<SolverDiagnostics>,
) {
    let start = crate::utils::Instant::now();

    let delta_secs = time.delta_seconds_adjusted();
    let max_overlap_solve_speed = solver_config.max_overlap_solve_speed * length_unit.0;

    let mut dummy_body1 = SolverBody::default();
    let mut dummy_body2 = SolverBody::default();
    let dummy_inertia = SolverBodyInertia::default();

    for constraint in &mut constraints.0 {
        let (mut body1, mut inertia1) = (&mut dummy_body1, &dummy_inertia);
        let (mut body2, mut inertia2) = (&mut dummy_body2, &dummy_inertia);

        // Get the solver bodies for the two colliding entities.
        if let Ok((body, inertia)) = unsafe { bodies.get_unchecked(constraint.body1) } {
            body1 = body.into_inner();
            inertia1 = inertia;
        }
        if let Ok((body, inertia)) = unsafe { bodies.get_unchecked(constraint.body2) } {
            body2 = body.into_inner();
            inertia2 = inertia;
        }

        // If a body has a higher dominance, it is treated as a static or kinematic body.
        match constraint.relative_dominance.cmp(&0) {
            Ordering::Greater => inertia1 = &dummy_inertia,
            Ordering::Less => inertia2 = &dummy_inertia,
            _ => {}
        }

        constraint.solve(
            body1,
            body2,
            inertia1,
            inertia2,
            delta_secs,
            USE_BIAS,
            max_overlap_solve_speed,
        );
    }

    if USE_BIAS {
        diagnostics.solve_constraints += start.elapsed();
    } else {
        diagnostics.relax_velocities += start.elapsed();
    }
}

/// Iterates through contact constraints and applies impulses to account for [`Restitution`].
///
/// Note that restitution with TGS Soft and speculative contacts may not be perfectly accurate.
/// This is a tradeoff, but cheap CCD is often more important than perfect restitution.
///
/// The number of iterations can be increased with [`SolverConfig::restitution_iterations`]
/// to apply restitution for multiple contact points more evenly.
#[allow(clippy::too_many_arguments)]
#[allow(clippy::type_complexity)]
fn solve_restitution(
    bodies: Query<(&mut SolverBody, &SolverBodyInertia)>,
    mut constraints: ResMut<ContactConstraints>,
    solver_config: Res<SolverConfig>,
    length_unit: Res<PhysicsLengthUnit>,
    mut diagnostics: ResMut<SolverDiagnostics>,
) {
    let start = crate::utils::Instant::now();

    // The restitution threshold determining the speed required for restitution to be applied.
    let threshold = solver_config.restitution_threshold * length_unit.0;

    let mut dummy_body1 = SolverBody::default();
    let mut dummy_body2 = SolverBody::default();
    let dummy_inertia = SolverBodyInertia::default();

    for constraint in constraints.iter_mut() {
        let restitution = constraint.restitution;

        if restitution == 0.0 {
            continue;
        }

        let (mut body1, mut inertia1) = (&mut dummy_body1, &dummy_inertia);
        let (mut body2, mut inertia2) = (&mut dummy_body2, &dummy_inertia);

        // Get the solver bodies for the two colliding entities.
        if let Ok((body, inertia)) = unsafe { bodies.get_unchecked(constraint.body1) } {
            body1 = body.into_inner();
            inertia1 = inertia;
        }
        if let Ok((body, inertia)) = unsafe { bodies.get_unchecked(constraint.body2) } {
            body2 = body.into_inner();
            inertia2 = inertia;
        }

        // If a body has a higher dominance, it is treated as a static or kinematic body.
        match constraint.relative_dominance.cmp(&0) {
            Ordering::Greater => inertia1 = &dummy_inertia,
            Ordering::Less => inertia2 = &dummy_inertia,
            _ => {}
        }

        // Performing multiple iterations can result in more accurate restitution,
        // but only if there are more than one contact point.
        let restitution_iterations = if constraint.points.len() > 1 {
            solver_config.restitution_iterations
        } else {
            1
        };

        for _ in 0..restitution_iterations {
            constraint.apply_restitution(body1, body2, inertia1, inertia2, threshold);
        }
    }

    diagnostics.apply_restitution += start.elapsed();
}

/// Copies contact impulses from [`ContactConstraints`] to the contacts in the [`ContactGraph`].
/// They will be used for [warm starting](SubstepSolverSet::WarmStart).
fn store_contact_impulses(
    constraints: Res<ContactConstraints>,
    mut contact_graph: ResMut<ContactGraph>,
    mut diagnostics: ResMut<SolverDiagnostics>,
) {
    let start = crate::utils::Instant::now();

    for constraint in constraints.iter() {
        let Some(contact_pair) = contact_graph.get_mut(constraint.collider1, constraint.collider2)
        else {
            unreachable!(
                "Contact pair between {} and {} not found in contact graph.",
                constraint.collider1, constraint.collider2
            );
        };

        let manifold = &mut contact_pair.manifolds[constraint.manifold_index];

        for (contact, constraint_point) in manifold.points.iter_mut().zip(constraint.points.iter())
        {
            contact.normal_impulse = constraint_point.normal_part.impulse;
            contact.tangent_impulse = constraint_point
                .tangent_part
                .as_ref()
                .map_or(default(), |part| part.impulse);
        }
    }

    diagnostics.store_impulses += start.elapsed();
}

/// Applies velocity corrections caused by joint damping.
#[allow(clippy::type_complexity)]
pub fn joint_damping<T: Joint + EntityConstraint<2>>(
    mut bodies: Query<
        (
            &RigidBody,
            &mut LinearVelocity,
            &mut AngularVelocity,
            &ComputedMass,
            Option<&Dominance>,
        ),
        RigidBodyActiveFilter,
    >,
    joints: Query<&T, Without<RigidBody>>,
    time: Res<Time>,
) {
    let delta_secs = time.delta_seconds_adjusted();

    for joint in &joints {
        if let Ok(
            [
                (rb1, mut lin_vel1, mut ang_vel1, mass1, dominance1),
                (rb2, mut lin_vel2, mut ang_vel2, mass2, dominance2),
            ],
        ) = bodies.get_many_mut(joint.entities())
        {
            let delta_omega =
                (ang_vel2.0 - ang_vel1.0) * (joint.damping_angular() * delta_secs).min(1.0);

            if rb1.is_dynamic() {
                ang_vel1.0 += delta_omega;
            }
            if rb2.is_dynamic() {
                ang_vel2.0 -= delta_omega;
            }

            let delta_v =
                (lin_vel2.0 - lin_vel1.0) * (joint.damping_linear() * delta_secs).min(1.0);

            let w1 = if rb1.is_dynamic() {
                mass1.inverse()
            } else {
                0.0
            };
            let w2 = if rb2.is_dynamic() {
                mass2.inverse()
            } else {
                0.0
            };

            if w1 + w2 <= Scalar::EPSILON {
                continue;
            }

            let p = delta_v / (w1 + w2);

            let dominance1 = dominance1.map_or(0, |dominance| dominance.0);
            let dominance2 = dominance2.map_or(0, |dominance| dominance.0);

            if rb1.is_dynamic() && (!rb2.is_dynamic() || dominance1 <= dominance2) {
                lin_vel1.0 += p * mass1.inverse();
            }
            if rb2.is_dynamic() && (!rb1.is_dynamic() || dominance2 <= dominance1) {
                lin_vel2.0 -= p * mass2.inverse();
            }
        }
    }
}
