use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use dcs_module_ipc::IPC;
use futures_util::FutureExt;
use stubs::atmosphere::v0::atmosphere_service_server::AtmosphereServiceServer;
use stubs::coalition::v0::coalition_service_server::CoalitionServiceServer;
use stubs::controller::v0::controller_service_server::ControllerServiceServer;
use stubs::custom::v0::custom_service_server::CustomServiceServer;
use stubs::group::v0::group_service_server::GroupServiceServer;
use stubs::hook::v0::hook_service_server::HookServiceServer;
use stubs::mission::v0::mission_service_server::MissionServiceServer;
use stubs::mission::v0::StreamEventsResponse;
use stubs::net::v0::net_service_server::NetServiceServer;
use stubs::timer::v0::timer_service_server::TimerServiceServer;
use stubs::trigger::v0::trigger_service_server::TriggerServiceServer;
use stubs::tts::v0::tts_service_server::{TtsService, TtsServiceServer};
use stubs::unit::v0::unit_service_server::UnitServiceServer;
use stubs::world::v0::world_service_server::WorldServiceServer;
use tokio::runtime::Runtime;
use tokio::sync::oneshot::{self, Receiver};
use tokio::time::sleep;
use tonic::transport;

use crate::config::{Config, SrsConfig, TtsConfig};
use crate::rpc::{HookRpc, MissionRpc, Tts};
use crate::shutdown::{Shutdown, ShutdownHandle};
use crate::stats::Stats;

pub struct Server {
    runtime: Runtime,
    shutdown: Shutdown,
    after_shutdown: Option<oneshot::Sender<()>>,
    state: ServerState,
    tts: Arc<Tts>,
}

#[derive(Clone)]
struct ServerState {
    addr: SocketAddr,
    eval_enabled: bool,
    ipc_mission: IPC<StreamEventsResponse>,
    ipc_hook: IPC<()>,
    stats: Stats,
    tts_config: TtsConfig,
    srs_config: SrsConfig,
}

impl Server {
    pub fn new(config: &Config) -> Result<Self, StartError> {
        let ipc_mission = IPC::default();
        let ipc_hook = IPC::default();
        let runtime = Runtime::new()?;
        let shutdown = Shutdown::new();
        Ok(Self {
            runtime,
            after_shutdown: None,
            state: ServerState {
                addr: format!("{}:{}", config.host, config.port).parse()?,
                eval_enabled: config.eval_enabled,
                ipc_mission: ipc_mission.clone(),
                ipc_hook,
                stats: Stats::new(shutdown.handle()),
                tts_config: config.tts.clone().unwrap_or_default(),
                srs_config: config.srs.clone().unwrap_or_default(),
            },
            tts: Arc::new(Tts::new(
                config.tts.clone().unwrap_or_default(),
                config.srs.clone().unwrap_or_default(),
                ipc_mission,
                shutdown.handle(),
            )),
            shutdown,
        })
    }

    pub fn run_in_background(&mut self) {
        if self.after_shutdown.is_some() {
            // already running
            return;
        }

        let (tx, rx) = oneshot::channel();
        self.after_shutdown = Some(tx);

        self.runtime.spawn(crate::server::run(
            self.state.clone(),
            self.shutdown.handle(),
            rx,
        ));

        self.runtime
            .spawn(self.state.stats.clone().run_in_background());

        self.runtime.spawn(crate::fps::run_in_background(
            self.state.ipc_mission.clone(),
            self.shutdown.handle().signal(),
        ));
    }

    pub fn stop_blocking(mut self) {
        // graceful shutdown
        self.runtime.block_on(self.shutdown.shutdown());
        if let Some(after_shutdown) = self.after_shutdown.take() {
            let _ = after_shutdown.send(());
        }

        // shutdown the async runtime, again give everything another 5 secs before forecefully
        // killing everything
        self.runtime.shutdown_timeout(Duration::from_secs(5));
    }

    pub fn ipc_mission(&self) -> &IPC<StreamEventsResponse> {
        &self.state.ipc_mission
    }

    pub fn ipc_hook(&self) -> &IPC<()> {
        &self.state.ipc_hook
    }

    pub fn stats(&self) -> &Stats {
        &self.state.stats
    }

    pub fn block_on<F: Future>(&self, future: F) -> F::Output {
        self.runtime.block_on(future)
    }

    pub fn tts(&self, ssml: String, frequency: u64, opts: Option<TtsOptions>) {
        let tts = self.tts.clone();
        let opts = opts.unwrap_or_default();
        log::debug!("TTS from Lua: `{}` @ {} ({:?})", ssml, frequency, opts);

        self.runtime.spawn(async move {
            let result = tts
                .transmit(tonic::Request::new(stubs::tts::v0::TransmitRequest {
                    ssml,
                    plaintext: opts.plaintext,
                    frequency,
                    srs_client_name: opts.srs_client_name,
                    position: opts.position,
                    coalition: opts
                        .coalition
                        .unwrap_or(stubs::common::v0::Coalition::Neutral)
                        .into(),
                    r#async: false,
                    provider: opts.provider,
                }))
                .await;
            match result {
                Ok(_) => {}
                Err(err) => {
                    log::error!("Error in TTS transmission from Lua: {}", err);
                }
            }
        });
    }
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TtsOptions {
    plaintext: Option<String>,
    srs_client_name: Option<String>,
    position: Option<stubs::common::v0::InputPosition>,
    coalition: Option<stubs::common::v0::Coalition>,
    provider: Option<stubs::tts::v0::transmit_request::Provider>,
}

async fn run(
    state: ServerState,
    shutdown_signal: ShutdownHandle,
    mut after_shutdown: Receiver<()>,
) {
    loop {
        match try_run(state.clone(), shutdown_signal.clone(), &mut after_shutdown).await {
            Ok(_) => break,
            Err(err) => {
                log::error!("{}", err);
                log::info!("Restarting gIPC Server in 10 seconds ...");
                sleep(Duration::from_secs(10)).await;
            }
        }
    }
}

async fn try_run(
    state: ServerState,
    shutdown_signal: ShutdownHandle,
    after_shutdown: &mut Receiver<()>,
) -> Result<(), transport::Error> {
    log::info!("Staring gRPC Server (on {}) ...", state.addr);

    let ServerState {
        addr,
        eval_enabled,
        ipc_mission,
        ipc_hook,
        stats,
        tts_config,
        srs_config,
    } = state;

    let mut mission_rpc =
        MissionRpc::new(ipc_mission.clone(), stats.clone(), shutdown_signal.clone());
    let mut hook_rpc = HookRpc::new(ipc_hook, stats, shutdown_signal.clone());

    if eval_enabled {
        mission_rpc.enable_eval();
        hook_rpc.enable_eval();
    }

    transport::Server::builder()
        .add_service(AtmosphereServiceServer::new(mission_rpc.clone()))
        .add_service(CoalitionServiceServer::new(mission_rpc.clone()))
        .add_service(ControllerServiceServer::new(mission_rpc.clone()))
        .add_service(CustomServiceServer::new(mission_rpc.clone()))
        .add_service(GroupServiceServer::new(mission_rpc.clone()))
        .add_service(HookServiceServer::new(hook_rpc))
        .add_service(MissionServiceServer::new(mission_rpc.clone()))
        .add_service(NetServiceServer::new(mission_rpc.clone()))
        .add_service(TimerServiceServer::new(mission_rpc.clone()))
        .add_service(TriggerServiceServer::new(mission_rpc.clone()))
        .add_service(TtsServiceServer::new(Tts::new(
            tts_config,
            srs_config,
            ipc_mission,
            shutdown_signal.clone(),
        )))
        .add_service(UnitServiceServer::new(mission_rpc.clone()))
        .add_service(WorldServiceServer::new(mission_rpc))
        .serve_with_shutdown(addr, after_shutdown.map(|_| ()))
        .await?;

    log::info!("Server stopped ...");

    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    AddrParse(#[from] std::net::AddrParseError),
}

impl<'lua> mlua::FromLua<'lua> for TtsOptions {
    fn from_lua(lua_value: mlua::Value<'lua>, lua: &'lua mlua::Lua) -> mlua::Result<Self> {
        use mlua::LuaSerdeExt;
        let opts: TtsOptions = lua.from_value(lua_value)?;
        Ok(opts)
    }
}
