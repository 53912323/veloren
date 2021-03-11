#![deny(unsafe_code)]
#![allow(clippy::option_map_unit_fn)]
#![deny(clippy::clone_on_ref_ptr)]
#![feature(
    label_break_value,
    bool_to_option,
    drain_filter,
    option_unwrap_none,
    option_zip,
    str_split_once
)]
#![cfg_attr(not(feature = "worldgen"), feature(const_panic))]

pub mod alias_validator;
mod character_creator;
pub mod chunk_generator;
pub mod client;
pub mod cmd;
pub mod connection_handler;
mod data_dir;
pub mod error;
pub mod events;
pub mod input;
pub mod login_provider;
pub mod metrics;
pub mod persistence;
pub mod presence;
pub mod rtsim;
pub mod settings;
pub mod state_ext;
pub mod sys;
#[cfg(not(feature = "worldgen"))] mod test_world;

// Reexports
pub use crate::{
    data_dir::DEFAULT_DATA_DIR_NAME,
    error::Error,
    events::Event,
    input::Input,
    settings::{EditableSettings, Settings},
};

use crate::{
    alias_validator::AliasValidator,
    chunk_generator::ChunkGenerator,
    client::Client,
    cmd::ChatCommandExt,
    connection_handler::ConnectionHandler,
    data_dir::DataDir,
    login_provider::LoginProvider,
    presence::{Presence, RegionSubscription},
    rtsim::RtSim,
    state_ext::StateExt,
    sys::sentinel::{DeletedEntities, TrackedComps},
};
#[cfg(not(feature = "worldgen"))]
use common::grid::Grid;
use common::{
    assets::AssetExt,
    cmd::ChatCommand,
    comp,
    comp::{item::MaterialStatManifest, CharacterAbility},
    event::{EventBus, ServerEvent},
    recipe::default_recipe_book,
    resources::TimeOfDay,
    rtsim::RtSimEntity,
    terrain::TerrainChunkSize,
    uid::UidAllocator,
    vol::{ReadVol, RectVolSize},
};
use common_ecs::run_now;
use common_net::{
    msg::{
        ClientType, DisconnectReason, ServerGeneral, ServerInfo, ServerInit, ServerMsg, WorldMapMsg,
    },
    sync::WorldSyncExt,
};
#[cfg(feature = "plugins")]
use common_sys::plugin::memory_manager::EcsWorld;
#[cfg(feature = "plugins")]
use common_sys::plugin::PluginMgr;
use common_sys::state::State;
use metrics::{EcsSystemMetrics, PhysicsMetrics, TickMetrics};
use network::{Network, Pid, ProtocolAddr};
use persistence::{
    character_loader::{CharacterLoader, CharacterLoaderResponseKind},
    character_updater::CharacterUpdater,
};
use plugin_api::Uid;
use prometheus::Registry;
use prometheus_hyper::Server as PrometheusServer;
use specs::{join::Join, Builder, Entity as EcsEntity, SystemData, WorldExt};
use std::{
    i32,
    ops::{Deref, DerefMut},
    sync::Arc,
    time::{Duration, Instant},
};
#[cfg(not(feature = "worldgen"))]
use test_world::{IndexOwned, World};
use tokio::{runtime::Runtime, sync::Notify};
use tracing::{debug, error, info, trace};
use vek::*;

#[cfg(feature = "worldgen")]
use world::{
    sim::{FileOpts, WorldOpts, DEFAULT_WORLD_MAP},
    IndexOwned, World,
};

#[macro_use] extern crate diesel;
#[macro_use] extern crate diesel_migrations;

#[derive(Copy, Clone)]
struct SpawnPoint(Vec3<f32>);

// Tick count used for throttling network updates
// Note this doesn't account for dt (so update rate changes with tick rate)
#[derive(Copy, Clone, Default)]
pub struct Tick(u64);

#[derive(Clone)]
pub struct HwStats {
    hardware_threads: u32,
    rayon_threads: u32,
}

// Start of Tick, used for metrics
#[derive(Copy, Clone)]
pub struct TickStart(Instant);

pub struct Server {
    state: State,
    world: Arc<World>,
    index: IndexOwned,
    map: WorldMapMsg,

    connection_handler: ConnectionHandler,

    runtime: Arc<Runtime>,

    metrics_shutdown: Arc<Notify>,
}

impl Server {
    /// Create a new `Server`
    #[allow(clippy::expect_fun_call)] // TODO: Pending review in #587
    #[allow(clippy::needless_update)] // TODO: Pending review in #587
    pub fn new(
        settings: Settings,
        editable_settings: EditableSettings,
        data_dir: &std::path::Path,
        runtime: Arc<Runtime>,
    ) -> Result<Self, Error> {
        info!("Server is data dir is: {}", data_dir.display());
        if settings.auth_server_address.is_none() {
            info!("Authentication is disabled");
        }

        // Relative to data_dir
        const PERSISTENCE_DB_DIR: &str = "saves";
        let persistence_db_dir = data_dir.join(PERSISTENCE_DB_DIR);

        // Run pending DB migrations (if any)
        debug!("Running DB migrations...");
        if let Some(e) = persistence::run_migrations(&persistence_db_dir).err() {
            panic!("Migration error: {:?}", e);
        }

        let registry = Arc::new(Registry::new());
        let chunk_gen_metrics = metrics::ChunkGenMetrics::new(&registry).unwrap();
        let network_request_metrics = metrics::NetworkRequestMetrics::new(&registry).unwrap();
        let player_metrics = metrics::PlayerMetrics::new(&registry).unwrap();
        let ecs_system_metrics = EcsSystemMetrics::new(&registry).unwrap();
        let tick_metrics = TickMetrics::new(&registry).unwrap();
        let physics_metrics = PhysicsMetrics::new(&registry).unwrap();

        let mut state = State::server();
        state.ecs_mut().insert(settings.clone());
        state.ecs_mut().insert(editable_settings);
        state.ecs_mut().insert(DataDir {
            path: data_dir.to_owned(),
        });
        state.ecs_mut().insert(EventBus::<ServerEvent>::default());
        state.ecs_mut().insert(LoginProvider::new(
            settings.auth_server_address.clone(),
            Arc::clone(&runtime),
        ));
        state.ecs_mut().insert(HwStats {
            hardware_threads: num_cpus::get() as u32,
            rayon_threads: num_cpus::get() as u32,
        });
        state.ecs_mut().insert(Tick(0));
        state.ecs_mut().insert(TickStart(Instant::now()));
        state.ecs_mut().insert(network_request_metrics);
        state.ecs_mut().insert(player_metrics);
        state.ecs_mut().insert(ecs_system_metrics);
        state.ecs_mut().insert(tick_metrics);
        state.ecs_mut().insert(physics_metrics);
        state
            .ecs_mut()
            .insert(ChunkGenerator::new(chunk_gen_metrics));
        state
            .ecs_mut()
            .insert(CharacterUpdater::new(&persistence_db_dir)?);

        let ability_map = comp::item::tool::AbilityMap::<CharacterAbility>::load_expect_cloned(
            "common.abilities.weapon_ability_manifest",
        );
        state.ecs_mut().insert(ability_map);

        let msm = comp::inventory::item::MaterialStatManifest::default();
        state.ecs_mut().insert(msm);

        state
            .ecs_mut()
            .insert(CharacterLoader::new(&persistence_db_dir)?);

        // System schedulers to control execution of systems
        state
            .ecs_mut()
            .insert(sys::PersistenceScheduler::every(Duration::from_secs(10)));

        // Server-only components
        state.ecs_mut().register::<RegionSubscription>();
        state.ecs_mut().register::<Client>();
        state.ecs_mut().register::<Presence>();
        state.ecs_mut().register::<comp::HomeChunk>();
        state.ecs_mut().register::<login_provider::PendingLogin>();

        //Alias validator
        let banned_words_paths = &settings.banned_words_files;
        let mut banned_words = Vec::new();
        for path in banned_words_paths {
            let mut list = match std::fs::File::open(&path) {
                Ok(file) => match ron::de::from_reader(&file) {
                    Ok(vec) => vec,
                    Err(error) => {
                        tracing::warn!(?error, ?file, "Couldn't deserialize banned words file");
                        return Err(Error::Other(format!(
                            "Couldn't read banned words file \"{}\"",
                            path.to_string_lossy()
                        )));
                    },
                },
                Err(error) => {
                    tracing::warn!(?error, ?path, "Couldn't open banned words file");
                    return Err(Error::Other(format!(
                        "Couldn't open banned words file \"{}\". Error: {}",
                        path.to_string_lossy(),
                        error
                    )));
                },
            };
            banned_words.append(&mut list);
        }
        let banned_words_count = banned_words.len();
        tracing::debug!(?banned_words_count);
        tracing::trace!(?banned_words);
        state.ecs_mut().insert(AliasValidator::new(banned_words));

        #[cfg(feature = "worldgen")]
        let (world, index) = World::generate(settings.world_seed, WorldOpts {
            seed_elements: true,
            world_file: if let Some(ref opts) = settings.map_file {
                opts.clone()
            } else {
                // Load default map from assets.
                FileOpts::LoadAsset(DEFAULT_WORLD_MAP.into())
            },
            ..WorldOpts::default()
        });
        #[cfg(feature = "worldgen")]
        let map = world.get_map_data(index.as_index_ref());

        #[cfg(not(feature = "worldgen"))]
        let (world, index) = World::generate(settings.world_seed);
        #[cfg(not(feature = "worldgen"))]
        let map = WorldMapMsg {
            dimensions_lg: Vec2::zero(),
            max_height: 1.0,
            rgba: Grid::new(Vec2::new(1, 1), 1),
            horizons: [(vec![0], vec![0]), (vec![0], vec![0])],
            sea_level: 0.0,
            alt: Grid::new(Vec2::new(1, 1), 1),
            sites: Vec::new(),
        };

        #[cfg(feature = "worldgen")]
        let spawn_point = {
            let index = index.as_index_ref();
            // NOTE: all of these `.map(|e| e as [type])` calls should compile into no-ops,
            // but are needed to be explicit about casting (and to make the compiler stop
            // complaining)

            // spawn in the chunk, that is in the middle of the world
            let center_chunk: Vec2<i32> = world.sim().map_size_lg().chunks().map(i32::from) / 2;

            // Find a town to spawn in that's close to the centre of the world
            let spawn_chunk = world
                .civs()
                .sites()
                .filter(|site| matches!(site.kind, world::civ::SiteKind::Settlement))
                .map(|site| site.center)
                .min_by_key(|site_pos| site_pos.distance_squared(center_chunk))
                .unwrap_or(center_chunk);

            // calculate the absolute position of the chunk in the world
            // (we could add TerrainChunkSize::RECT_SIZE / 2 here, to spawn in the middle of
            // the chunk)
            let spawn_wpos = spawn_chunk.map2(TerrainChunkSize::RECT_SIZE, |e, sz| {
                e as i32 * sz as i32 + sz as i32 / 2
            });

            // unwrapping because generate_chunk only returns err when should_continue evals
            // to true
            let (tc, _cs) = world.generate_chunk(index, spawn_chunk, || false).unwrap();
            let min_z = tc.get_min_z();
            let max_z = tc.get_max_z();

            let pos = Vec3::new(spawn_wpos.x, spawn_wpos.y, min_z);
            (0..(max_z - min_z))
                .map(|z_diff| pos + Vec3::unit_z() * z_diff)
                .find(|test_pos| {
                    let chunk_relative_xy = test_pos
                        .xy()
                        .map2(TerrainChunkSize::RECT_SIZE, |e, sz| e.rem_euclid(sz as i32));
                    tc.get(
                        Vec3::new(chunk_relative_xy.x, chunk_relative_xy.y, test_pos.z)
                            - Vec3::unit_z(),
                    )
                    .map_or(false, |b| b.is_filled())
                        && (0..3).all(|z| {
                            tc.get(
                                Vec3::new(chunk_relative_xy.x, chunk_relative_xy.y, test_pos.z)
                                    + Vec3::unit_z() * z,
                            )
                            .map_or(true, |b| !b.is_solid())
                        })
                })
                .unwrap_or(pos)
                .map(|e| e as f32)
                + 0.5
        };
        #[cfg(not(feature = "worldgen"))]
        let spawn_point = Vec3::new(0.0, 0.0, 256.0);

        // set the spawn point we calculated above
        state.ecs_mut().insert(SpawnPoint(spawn_point));

        // Insert the world into the ECS (todo: Maybe not an Arc?)
        let world = Arc::new(world);
        state.ecs_mut().insert(Arc::clone(&world));
        state.ecs_mut().insert(index.clone());

        // Set starting time for the server.
        state.ecs_mut().write_resource::<TimeOfDay>().0 = settings.start_time;

        // Register trackers
        sys::sentinel::register_trackers(&mut state.ecs_mut());

        state.ecs_mut().insert(DeletedEntities::default());

        let network = Network::new_with_registry(Pid::new(), &runtime, &registry);
        let metrics_shutdown = Arc::new(Notify::new());
        let metrics_shutdown_clone = Arc::clone(&metrics_shutdown);
        let addr = settings.metrics_address;
        runtime.spawn(async move {
            PrometheusServer::run(
                Arc::clone(&registry),
                addr,
                metrics_shutdown_clone.notified(),
            )
            .await
        });
        runtime.block_on(network.listen(ProtocolAddr::Tcp(settings.gameserver_address)))?;
        runtime.block_on(network.listen(ProtocolAddr::Mpsc(14004)))?;
        let connection_handler = ConnectionHandler::new(network, &runtime);

        // Initiate real-time world simulation
        #[cfg(feature = "worldgen")]
        rtsim::init(&mut state, &world);
        #[cfg(not(feature = "worldgen"))]
        rtsim::init(&mut state);

        let this = Self {
            state,
            world,
            index,
            map,

            connection_handler,
            runtime,

            metrics_shutdown,
        };

        debug!(?settings, "created veloren server with");

        let git_hash = *common::util::GIT_HASH;
        let git_date = common::util::GIT_DATE.clone();
        let git_time = *common::util::GIT_TIME;
        let version = common::util::DISPLAY_VERSION_LONG.clone();
        info!(?version, "Server version");
        debug!(?git_hash, ?git_date, ?git_time, "detailed Server version");

        Ok(this)
    }

    pub fn get_server_info(&self) -> ServerInfo {
        let settings = self.state.ecs().fetch::<Settings>();
        let editable_settings = self.state.ecs().fetch::<EditableSettings>();
        ServerInfo {
            name: settings.server_name.clone(),
            description: (&*editable_settings.server_description).clone(),
            git_hash: common::util::GIT_HASH.to_string(),
            git_date: common::util::GIT_DATE.to_string(),
            auth_provider: settings.auth_server_address.clone(),
        }
    }

    /// Get a reference to the server's settings
    pub fn settings(&self) -> impl Deref<Target = Settings> + '_ {
        self.state.ecs().fetch::<Settings>()
    }

    /// Get a mutable reference to the server's settings
    pub fn settings_mut(&self) -> impl DerefMut<Target = Settings> + '_ {
        self.state.ecs().fetch_mut::<Settings>()
    }

    /// Get a mutable reference to the server's editable settings
    pub fn editable_settings_mut(&self) -> impl DerefMut<Target = EditableSettings> + '_ {
        self.state.ecs().fetch_mut::<EditableSettings>()
    }

    /// Get a reference to the server's editable settings
    pub fn editable_settings(&self) -> impl Deref<Target = EditableSettings> + '_ {
        self.state.ecs().fetch::<EditableSettings>()
    }

    /// Get path to the directory that the server info into
    pub fn data_dir(&self) -> impl Deref<Target = DataDir> + '_ {
        self.state.ecs().fetch::<DataDir>()
    }

    /// Get a reference to the server's game state.
    pub fn state(&self) -> &State { &self.state }

    /// Get a mutable reference to the server's game state.
    pub fn state_mut(&mut self) -> &mut State { &mut self.state }

    /// Get a reference to the server's world.
    pub fn world(&self) -> &World { &self.world }

    /// Get a reference to the server's world map.
    pub fn map(&self) -> &WorldMapMsg { &self.map }

    /// Execute a single server tick, handle input and update the game state by
    /// the given duration.
    pub fn tick(&mut self, _input: Input, dt: Duration) -> Result<Vec<Event>, Error> {
        self.state.ecs().write_resource::<Tick>().0 += 1;
        self.state.ecs().write_resource::<TickStart>().0 = Instant::now();

        // This tick function is the centre of the Veloren universe. Most server-side
        // things are managed from here, and as such it's important that it
        // stays organised. Please consult the core developers before making
        // significant changes to this code. Here is the approximate order of
        // things. Please update it as this code changes.
        //
        // 1) Collect input from the frontend, apply input effects to the
        //    state of the game
        // 2) Go through any events (timer-driven or otherwise) that need handling
        //    and apply them to the state of the game
        // 3) Go through all incoming client network communications, apply them to
        //    the game state
        // 4) Perform a single LocalState tick (i.e: update the world and entities
        //    in the world)
        // 5) Go through the terrain update queue and apply all changes to
        //    the terrain
        // 6) Send relevant state updates to all clients
        // 7) Check for persistence updates related to character data, and message the
        //    relevant entities
        // 8) Update Metrics with current data
        // 9) Finish the tick, passing control of the main thread back
        //    to the frontend

        // 1) Build up a list of events for this frame, to be passed to the frontend.
        let mut frontend_events = Vec::new();

        // 2)

        let before_new_connections = Instant::now();

        // 3) Handle inputs from clients
        self.handle_new_connections(&mut frontend_events);

        let before_message_system = Instant::now();

        // Run message receiving sys before the systems in common for decreased latency
        // (e.g. run before controller system)
        //TODO: run in parallel
        run_now::<sys::msg::general::Sys>(&self.state.ecs());
        run_now::<sys::msg::register::Sys>(&self.state.ecs());
        run_now::<sys::msg::character_screen::Sys>(&self.state.ecs());
        run_now::<sys::msg::in_game::Sys>(&self.state.ecs());
        run_now::<sys::msg::terrain::Sys>(&self.state.ecs());
        run_now::<sys::msg::ping::Sys>(&self.state.ecs());
        run_now::<sys::agent::Sys>(&self.state.ecs());

        let before_state_tick = Instant::now();

        // 4) Tick the server's LocalState.
        // 5) Fetch any generated `TerrainChunk`s and insert them into the terrain.
        // in sys/terrain.rs
        self.state.tick(
            dt,
            |dispatcher_builder| {
                sys::add_server_systems(dispatcher_builder);
                #[cfg(feature = "worldgen")]
                rtsim::add_server_systems(dispatcher_builder);
            },
            false,
        );

        let before_handle_events = Instant::now();

        // Handle game events
        frontend_events.append(&mut self.handle_events());

        let before_update_terrain_and_regions = Instant::now();

        // Apply terrain changes and update the region map after processing server
        // events so that changes made by server events will be immediately
        // visible to client synchronization systems, minimizing the latency of
        // `ServerEvent` mediated effects
        self.state.update_region_map();
        self.state.apply_terrain_changes();

        let before_sync = Instant::now();

        // 6) Synchronise clients with the new state of the world.
        sys::run_sync_systems(self.state.ecs_mut());

        let before_world_tick = Instant::now();

        // Tick the world
        self.world.tick(dt);

        let before_entity_cleanup = Instant::now();

        // Remove NPCs that are outside the view distances of all players
        // This is done by removing NPCs in unloaded chunks
        let to_delete = {
            let terrain = self.state.terrain();
            (
                &self.state.ecs().entities(),
                &self.state.ecs().read_storage::<comp::Pos>(),
                !&self.state.ecs().read_storage::<comp::Player>(),
                self.state.ecs().read_storage::<comp::HomeChunk>().maybe(),
            )
                .join()
                .filter(|(_, pos, _, home_chunk)| {
                    let chunk_key = terrain.pos_key(pos.0.map(|e| e.floor() as i32));
                    // Check if both this chunk and the NPCs `home_chunk` is unloaded. If so,
                    // we delete them. We check for `home_chunk` in order to avoid duplicating
                    // the entity under some circumstances.
                    terrain.get_key(chunk_key).is_none()
                        && home_chunk.map_or(true, |hc| terrain.get_key(hc.0).is_none())
                })
                .map(|(entity, _, _, _)| entity)
                .collect::<Vec<_>>()
        };

        for entity in to_delete {
            // Assimilate entities that are part of the real-time world simulation
            if let Some(rtsim_entity) = self
                .state
                .ecs()
                .read_storage::<RtSimEntity>()
                .get(entity)
                .copied()
            {
                self.state
                    .ecs()
                    .write_resource::<RtSim>()
                    .assimilate_entity(rtsim_entity.0);
            }

            if let Err(e) = self.state.delete_entity_recorded(entity) {
                error!(?e, "Failed to delete agent outside the terrain");
            }
        }

        // 7 Persistence updates
        let before_persistence_updates = Instant::now();

        // Get character-related database responses and notify the requesting client
        self.state
            .ecs()
            .read_resource::<persistence::character_loader::CharacterLoader>()
            .messages()
            .for_each(|query_result| match query_result.result {
                CharacterLoaderResponseKind::CharacterList(result) => match result {
                    Ok(character_list_data) => self.notify_client(
                        query_result.entity,
                        ServerGeneral::CharacterListUpdate(character_list_data),
                    ),
                    Err(error) => self.notify_client(
                        query_result.entity,
                        ServerGeneral::CharacterActionError(error.to_string()),
                    ),
                },
                CharacterLoaderResponseKind::CharacterCreation(result) => match result {
                    Ok((character_id, list)) => {
                        self.notify_client(
                            query_result.entity,
                            ServerGeneral::CharacterListUpdate(list),
                        );
                        self.notify_client(
                            query_result.entity,
                            ServerGeneral::CharacterCreated(character_id),
                        );
                    },
                    Err(error) => self.notify_client(
                        query_result.entity,
                        ServerGeneral::CharacterActionError(error.to_string()),
                    ),
                },
                CharacterLoaderResponseKind::CharacterData(result) => {
                    let message = match *result {
                        Ok(character_data) => ServerEvent::UpdateCharacterData {
                            entity: query_result.entity,
                            components: character_data,
                        },
                        Err(error) => {
                            // We failed to load data for the character from the DB. Notify the
                            // client to push the state back to character selection, with the error
                            // to display
                            self.notify_client(
                                query_result.entity,
                                ServerGeneral::CharacterDataLoadError(error.to_string()),
                            );

                            // Clean up the entity data on the server
                            ServerEvent::ExitIngame {
                                entity: query_result.entity,
                            }
                        },
                    };

                    self.state
                        .ecs()
                        .read_resource::<EventBus<ServerEvent>>()
                        .emit_now(message);
                },
            });

        {
            // Check for new chunks; cancel and regenerate all chunks if the asset has been
            // reloaded. Note that all of these assignments are no-ops, so the
            // only work we do here on the fast path is perform a relaxed read on an atomic.
            // boolean.
            let index = &mut self.index;
            let runtime = &mut self.runtime;
            let world = &mut self.world;
            let ecs = self.state.ecs_mut();

            index.reload_colors_if_changed(|index| {
                let mut chunk_generator = ecs.write_resource::<ChunkGenerator>();
                let client = ecs.read_storage::<Client>();
                let mut terrain = ecs.write_resource::<common::terrain::TerrainGrid>();

                // Cancel all pending chunks.
                chunk_generator.cancel_all();

                if client.is_empty() {
                    // No clients, so just clear all terrain.
                    terrain.clear();
                } else {
                    // There's at least one client, so regenerate all chunks.
                    terrain.iter().for_each(|(pos, _)| {
                        chunk_generator.generate_chunk(
                            None,
                            pos,
                            runtime,
                            Arc::clone(&world),
                            index.clone(),
                        );
                    });
                }
            });
        }

        let end_of_server_tick = Instant::now();

        // 8) Update Metrics
        run_now::<sys::metrics::Sys>(&self.state.ecs());

        {
            // Report timing info
            let tick_metrics = self.state.ecs().read_resource::<metrics::TickMetrics>();

            let tt = &tick_metrics.tick_time;
            tt.with_label_values(&["new connections"])
                .set((before_message_system - before_new_connections).as_nanos() as i64);
            tt.with_label_values(&["handle server events"])
                .set((before_update_terrain_and_regions - before_handle_events).as_nanos() as i64);
            tt.with_label_values(&["update terrain and region map"])
                .set((before_sync - before_update_terrain_and_regions).as_nanos() as i64);
            tt.with_label_values(&["state"])
                .set((before_handle_events - before_state_tick).as_nanos() as i64);
            tt.with_label_values(&["world tick"])
                .set((before_entity_cleanup - before_world_tick).as_nanos() as i64);
            tt.with_label_values(&["entity cleanup"])
                .set((before_persistence_updates - before_entity_cleanup).as_nanos() as i64);
            tt.with_label_values(&["persistence_updates"])
                .set((end_of_server_tick - before_persistence_updates).as_nanos() as i64);
        }

        // 9) Finish the tick, pass control back to the frontend.

        Ok(frontend_events)
    }

    /// Clean up the server after a tick.
    pub fn cleanup(&mut self) {
        // Cleanup the local state
        self.state.cleanup();
    }

    fn initialize_client(
        &mut self,
        client: crate::connection_handler::IncomingClient,
    ) -> Result<Option<specs::Entity>, Error> {
        if self.settings().max_players <= self.state.ecs().read_storage::<Client>().join().count() {
            trace!(
                ?client.participant,
                "to many players, wont allow participant to connect"
            );
            client.send(ServerInit::TooManyPlayers)?;
            return Ok(None);
        }

        let entity = self
            .state
            .ecs_mut()
            .create_entity_synced()
            .with(client)
            .build();
        self.state
            .ecs()
            .read_resource::<metrics::PlayerMetrics>()
            .clients_connected
            .inc();
        // Send client all the tracked components currently attached to its entity as
        // well as synced resources (currently only `TimeOfDay`)
        debug!("Starting initial sync with client.");
        self.state
            .ecs()
            .read_storage::<Client>()
            .get(entity)
            .unwrap()
            .send(ServerInit::GameSync {
                // Send client their entity
                entity_package: TrackedComps::fetch(&self.state.ecs())
                    .create_entity_package(entity, None, None, None),
                time_of_day: *self.state.ecs().read_resource(),
                max_group_size: self.settings().max_player_group_size,
                client_timeout: self.settings().client_timeout,
                world_map: self.map.clone(),
                recipe_book: default_recipe_book().cloned(),
                material_stats: MaterialStatManifest::default(),
                ability_map: (&*self
                    .state
                    .ecs()
                    .read_resource::<comp::item::tool::AbilityMap>())
                    .clone(),
            })?;
        Ok(Some(entity))
    }

    /// Handle new client connections.
    fn handle_new_connections(&mut self, frontend_events: &mut Vec<Event>) {
        while let Ok(sender) = self.connection_handler.info_requester_receiver.try_recv() {
            // can fail, e.g. due to timeout or network prob.
            trace!("sending info to connection_handler");
            let _ = sender.send(crate::connection_handler::ServerInfoPacket {
                info: self.get_server_info(),
                time: self.state.get_time(),
            });
        }

        while let Ok(incoming) = self.connection_handler.client_receiver.try_recv() {
            match self.initialize_client(incoming) {
                Ok(None) => (),
                Ok(Some(entity)) => {
                    frontend_events.push(Event::ClientConnected { entity });
                    debug!("Done initial sync with client.");
                },
                Err(e) => {
                    debug!(?e, "failed initializing a new client");
                },
            }
        }
    }

    pub fn notify_client<S>(&self, entity: EcsEntity, msg: S)
    where
        S: Into<ServerMsg>,
    {
        self.state
            .ecs()
            .read_storage::<Client>()
            .get(entity)
            .map(|c| c.send(msg));
    }

    pub fn notify_players(&mut self, msg: ServerGeneral) { self.state.notify_players(msg); }

    pub fn generate_chunk(&mut self, entity: EcsEntity, key: Vec2<i32>) {
        self.state
            .ecs()
            .write_resource::<ChunkGenerator>()
            .generate_chunk(
                Some(entity),
                key,
                &self.runtime,
                Arc::clone(&self.world),
                self.index.clone(),
            );
    }

    fn process_chat_cmd(&mut self, entity: EcsEntity, cmd: String) {
        // Separate string into keyword and arguments.
        let sep = cmd.find(' ');
        let (kwd, args) = match sep {
            Some(i) => (cmd[..i].to_string(), cmd[(i + 1)..].to_string()),
            None => (cmd, "".to_string()),
        };

        // Find the command object and run its handler.
        if let Ok(command) = kwd.parse::<ChatCommand>() {
            command.execute(self, entity, args);
        } else {
            #[cfg(feature = "plugins")]
            {
                let plugin_manager = self.state.ecs().read_resource::<PluginMgr>();
                let ecs_world = EcsWorld {
                    entities: &self.state.ecs().entities(),
                    health: self.state.ecs().read_component().into(),
                    uid: self.state.ecs().read_component().into(),
                    uid_allocator: &self.state.ecs().read_resource::<UidAllocator>().into(),
                    player: self.state.ecs().read_component().into(),
                };
                let rs = plugin_manager.execute_event(
                    &ecs_world,
                    &plugin_api::event::ChatCommandEvent {
                        command: kwd.clone(),
                        command_args: args.split(' ').map(|x| x.to_owned()).collect(),
                        player: plugin_api::event::Player {
                            id: plugin_api::Uid(
                                (self
                                    .state
                                    .ecs()
                                    .read_storage::<Uid>()
                                    .get(entity)
                                    .expect("Can't get player UUID [This should never appen]"))
                                .0,
                            ),
                        },
                    },
                );
                match rs {
                    Ok(e) => {
                        if e.is_empty() {
                            self.notify_client(
                                entity,
                                ServerGeneral::server_msg(
                                    comp::ChatType::CommandError,
                                    format!(
                                        "Unknown command '/{}'.\nType '/help' for available \
                                         commands",
                                        kwd
                                    ),
                                ),
                            );
                        } else {
                            e.into_iter().for_each(|e| match e {
                                Ok(e) => {
                                    if !e.is_empty() {
                                        self.notify_client(
                                            entity,
                                            ServerGeneral::server_msg(
                                                comp::ChatType::CommandInfo,
                                                e.join("\n"),
                                            ),
                                        );
                                    }
                                },
                                Err(e) => {
                                    self.notify_client(
                                        entity,
                                        ServerGeneral::server_msg(
                                            comp::ChatType::CommandError,
                                            format!(
                                                "Error occurred while executing command '/{}'.\n{}",
                                                kwd, e
                                            ),
                                        ),
                                    );
                                },
                            });
                        }
                    },
                    Err(e) => {
                        error!(?e, "Can't execute command {} {}", kwd, args);
                        self.notify_client(
                            entity,
                            ServerGeneral::server_msg(
                                comp::ChatType::CommandError,
                                format!(
                                    "Internal error while executing '/{}'.\nContact the server \
                                     administrator",
                                    kwd
                                ),
                            ),
                        );
                    },
                }
            }
        }
    }

    fn entity_is_admin(&self, entity: EcsEntity) -> bool {
        self.state
            .read_storage::<comp::Admin>()
            .get(entity)
            .is_some()
    }

    pub fn number_of_players(&self) -> i64 {
        self.state.ecs().read_storage::<Client>().join().count() as i64
    }

    // TODO: add Admin comp if ingame
    pub fn add_admin(&self, username: &str) {
        let mut editable_settings = self.editable_settings_mut();
        let login_provider = self.state.ecs().fetch::<LoginProvider>();
        let data_dir = self.data_dir();
        add_admin(
            username,
            &login_provider,
            &mut editable_settings,
            &data_dir.path,
        );
    }

    // TODO: remove Admin comp if ingame
    pub fn remove_admin(&self, username: &str) {
        let mut editable_settings = self.editable_settings_mut();
        let login_provider = self.state.ecs().fetch::<LoginProvider>();
        let data_dir = self.data_dir();
        remove_admin(
            username,
            &login_provider,
            &mut editable_settings,
            &data_dir.path,
        );
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.metrics_shutdown.notify_one();
        self.state
            .notify_players(ServerGeneral::Disconnect(DisconnectReason::Shutdown));
    }
}

pub fn add_admin(
    username: &str,
    login_provider: &LoginProvider,
    editable_settings: &mut EditableSettings,
    data_dir: &std::path::Path,
) {
    use crate::settings::EditableSetting;
    match login_provider.username_to_uuid(username) {
        Ok(uuid) => editable_settings.admins.edit(data_dir, |admins| {
            if admins.insert(uuid) {
                info!("Successfully added {} ({}) as an admin!", username, uuid);
            } else {
                info!("{} ({}) is already an admin!", username, uuid);
            }
        }),
        Err(err) => error!(
            ?err,
            "Could not find uuid for this name either the user does not exist or there was an \
             error communicating with the auth server."
        ),
    }
}

pub fn remove_admin(
    username: &str,
    login_provider: &LoginProvider,
    editable_settings: &mut EditableSettings,
    data_dir: &std::path::Path,
) {
    use crate::settings::EditableSetting;
    match login_provider.username_to_uuid(username) {
        Ok(uuid) => editable_settings.admins.edit(data_dir, |admins| {
            if admins.remove(&uuid) {
                info!(
                    "Successfully removed {} ({}) from the admins",
                    username, uuid
                );
            } else {
                info!("{} ({}) is not an admin!", username, uuid);
            }
        }),
        Err(err) => error!(
            ?err,
            "Could not find uuid for this name either the user does not exist or there was an \
             error communicating with the auth server."
        ),
    }
}
